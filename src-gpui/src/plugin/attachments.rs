//! 插件笔记的公开附件：插入时复制独立副本，正文保存后再与笔记行同事务记账。
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use super::storage::{self, AssetKind, AssetRow, Root, SourceKind};
use crate::image_import::{
    ImageInput, ImportCancellation, ImportedImage, MAX_BATCH_BYTES, inspect_image, materialize,
};

#[derive(Clone, Debug)]
pub(crate) struct PluginAttachmentConfig {
    pub note_id: i64,
    pub root: Root,
    pub body_path: PathBuf,
    body_rel: String,
}

impl PluginAttachmentConfig {
    pub fn for_note_path(note_id: i64, path: &Path) -> storage::Res<Self> {
        let body_path = std::fs::canonicalize(path)
            .map_err(|error| format!("正文路径无法确认：{error}"))?;
        let notes = body_path
            .parent()
            .ok_or_else(|| "正文没有父目录".to_string())?;
        let directory = notes
            .parent()
            .ok_or_else(|| "正文不在笔记目录里".to_string())?;
        let folders = directory
            .parent()
            .ok_or_else(|| "正文不在 folders 下".to_string())?;
        let root_path = folders
            .parent()
            .ok_or_else(|| "正文库根无法确认".to_string())?;
        if notes.file_name().and_then(|name| name.to_str()) != Some("notes")
            || folders.file_name().and_then(|name| name.to_str()) != Some("folders")
        {
            return Err("插件附件只接受 folders/<dir>/notes/ 下的正文".to_string());
        }
        let root = Root::open(root_path)?;
        let relative = body_path
            .strip_prefix(root.path())
            .map_err(|_| "正文落在库根之外".to_string())?;
        let body_rel = relative.to_string_lossy().replace('\\', "/");
        storage::check_rel(&body_rel)?;
        Ok(Self { note_id, root, body_path, body_rel })
    }

    fn asset_directory(&self) -> storage::Res<PathBuf> {
        let parent = self
            .body_rel
            .rsplit_once('/')
            .map(|(parent, _)| parent)
            .ok_or_else(|| "正文相对路径没有父目录".to_string())?;
        let path = self.root.resolve(&format!("{parent}/a"))?;
        fs::create_dir_all(&path).map_err(|error| format!("创建附件目录失败：{error}"))?;
        let canonical = fs::canonicalize(&path)
            .map_err(|error| format!("附件目录无法确认：{error}"))?;
        if !canonical.starts_with(self.root.path()) {
            return Err("附件目录落在库根之外".to_string());
        }
        Ok(canonical)
    }
}

#[derive(Debug)]
pub(crate) struct PluginImportBatch {
    pub images: Vec<ImportedImage>,
    pub assets: Vec<AssetRow>,
    pub failed: usize,
    pub errors: Vec<String>,
}

struct CopyGuard {
    temporary: PathBuf,
    target: PathBuf,
    target_created: bool,
    verified: bool,
}

impl Drop for CopyGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.temporary);
        if self.target_created && !self.verified {
            let _ = fs::remove_file(&self.target);
        }
    }
}

fn new_asset_id() -> storage::Res<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|error| format!("生成附件标识失败：{error}"))?;
    Ok(storage::hex_lower(&bytes))
}

fn create_temporary(path: &Path) -> storage::Res<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path).map_err(|error| error.to_string())
}

fn publish(temporary: &Path, target: &Path) -> storage::Res<()> {
    fs::hard_link(temporary, target).map_err(|error| {
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            "附件标识已存在，拒绝覆盖".into()
        } else {
            format!("发布附件失败：{error}")
        }
    })
}

fn verify_published(
    target: &Path,
    expected_bytes: u64,
    expected_hash: &[u8; 32],
    cancel: &ImportCancellation,
) -> storage::Res<()> {
    let on_disk = fs::metadata(target)
        .map_err(|error| format!("读回附件大小失败：{error}"))?;
    let on_disk_hash = crate::image_import::fingerprint(target, cancel)?;
    if on_disk.len() != expected_bytes || on_disk_hash != *expected_hash {
        return Err("附件落盘后的大小或 SHA-256 对不上".to_string());
    }
    Ok(())
}

fn copy_one(
    config: &PluginAttachmentConfig,
    directory: &Path,
    source: &Path,
    cancel: &ImportCancellation,
) -> storage::Res<(ImportedImage, AssetRow)> {
    cancel.check()?;
    let info = inspect_image(source)?;
    let source_bytes = fs::metadata(source)
        .map_err(|error| format!("读取图片大小失败：{error}"))?
        .len();
    let expected_hash = crate::image_import::fingerprint(source, cancel)?;
    let extension = source
        .extension()
        .and_then(|value| value.to_str())
        .ok_or_else(|| "图片缺少扩展名".to_string())?
        .to_ascii_lowercase();
    let id = new_asset_id()?;
    let name = format!("{id}.{extension}");
    let target = directory.join(&name);
    let temporary = directory.join(format!(
        ".{name}.tmp-{}-{id}",
        std::process::id()
    ));
    let mut guard = CopyGuard {
        temporary: temporary.clone(),
        target: target.clone(),
        target_created: false,
        verified: false,
    };
    let mut output = create_temporary(&temporary)?;
    let copied = (|| -> storage::Res<()> {
        let mut input = File::open(source).map_err(|error| format!("读取图片失败：{error}"))?;
        let mut actual = Sha256::new();
        let mut buffer = [0u8; 64 * 1024];
        let mut bytes = 0u64;
        loop {
            cancel.check()?;
            let count = input
                .read(&mut buffer)
                .map_err(|error| format!("读取图片失败：{error}"))?;
            if count == 0 {
                break;
            }
            bytes += count as u64;
            if bytes > crate::images::MAX_INPUT_BYTES as u64 {
                return Err("导入期间图片超过 32 MiB 输入预算".to_string());
            }
            output
                .write_all(&buffer[..count])
                .map_err(|error| format!("写入附件失败：{error}"))?;
            actual.update(&buffer[..count]);
        }
        if bytes != source_bytes || <[u8; 32]>::from(actual.finalize()) != expected_hash {
            return Err("图片在导入期间发生变化，请重试".to_string());
        }
        output.sync_all().map_err(|error| format!("同步附件失败：{error}"))?;
        cancel.check()
    })();
    drop(output);
    copied?;
    publish(&temporary, &target)?;
    guard.target_created = true;
    verify_published(&target, source_bytes, &expected_hash, cancel)?;
    guard.verified = true;
    cancel.record_resource();
    let parent = config
        .body_rel
        .rsplit_once('/')
        .map(|(parent, _)| parent)
        .ok_or_else(|| "正文相对路径没有父目录".to_string())?;
    let asset = AssetRow {
        id: id.clone(),
        note_id: config.note_id,
        kind: if extension == "gif" {
            AssetKind::Gif
        } else {
            AssetKind::Image
        },
        rel_path: format!("{parent}/a/{name}"),
        source_kind: SourceKind::File,
        source_ref: String::new(),
        bytes: source_bytes,
        width: info.width,
        height: info.height,
        frames: None,
        duration_ms: None,
        sha256: storage::hex_lower(&expected_hash),
        created_at_ms: storage::now_ms(),
    };
    Ok((
        ImportedImage {
            src: format!("a/{name}"),
            alt: source
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
        },
        asset,
    ))
}

pub(crate) fn import_inputs(
    config: &PluginAttachmentConfig,
    inputs: Vec<ImageInput>,
    cancel: &ImportCancellation,
) -> storage::Res<PluginImportBatch> {
    let files = materialize(inputs, cancel)?;
    let mut total = 0u64;
    for path in &files.paths {
        cancel.check()?;
        inspect_image(path)?;
        total = total
            .checked_add(
                fs::metadata(path)
                    .map_err(|error| format!("读取图片大小失败：{error}"))?
                    .len(),
            )
            .ok_or_else(|| "图片输入过大".to_string())?;
        if total > MAX_BATCH_BYTES as u64 {
            return Err("一次最多导入 64 MiB 图片数据".to_string());
        }
    }
    let directory = config.asset_directory()?;
    let mut batch = PluginImportBatch {
        images: Vec::new(),
        assets: Vec::new(),
        failed: 0,
        errors: Vec::new(),
    };
    for path in files.paths.clone() {
        cancel.check()?;
        match copy_one(config, &directory, &path, cancel) {
            Ok((image, asset)) => {
                batch.images.push(image);
                batch.assets.push(asset);
            }
            Err(error) => {
                cancel.check()?;
                batch.failed += 1;
                if batch.errors.len() < 4 {
                    batch.errors.push(format!(
                        "{}：{error}",
                        path.file_name().unwrap_or_default().to_string_lossy()
                    ));
                }
            }
        }
    }
    Ok(batch)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::storage::{Db, NoteRow};
    use std::io::Cursor;

    struct Fixture {
        _directory: tempfile::TempDir,
        root: Root,
        _db: Db,
        note: NoteRow,
        config: PluginAttachmentConfig,
        source: PathBuf,
    }

    fn fixture(tag: &str) -> Fixture {
        let directory = tempfile::Builder::new().prefix(&format!("nomo-attachment-{tag}")).tempdir().unwrap();
        let root = Root::open(directory.path()).unwrap();
        let mut db = Db::open(&directory.path().join("notes.db")).unwrap();
        let note = db.create_note(&root, "inbox", 7).unwrap();
        let body = root.resolve(&note.body_path).unwrap();
        let config = PluginAttachmentConfig::for_note_path(note.id, &body).unwrap();
        let source = directory.path().join("source.png");
        let image = image::RgbaImage::from_fn(2, 2, |x, y| {
            image::Rgba([(x * 80) as u8, (y * 80) as u8, 40, 255])
        });
        let mut encoded = Cursor::new(Vec::new());
        image.write_to(&mut encoded, image::ImageFormat::Png).unwrap();
        fs::write(&source, encoded.into_inner()).unwrap();
        Fixture { _directory: directory, root, _db: db, note, config, source }
    }

    #[test]
    fn repeated_import_creates_independent_random_copies() {
        let fixture = fixture("independent");
        let batch = import_inputs(
            &fixture.config,
            vec![ImageInput::File(fixture.source.clone()), ImageInput::File(fixture.source.clone())],
            &ImportCancellation::default(),
        ).unwrap();
        assert_eq!(batch.failed, 0);
        assert_eq!(batch.images.len(), 2);
        assert_eq!(batch.assets.len(), 2);
        assert_ne!(batch.assets[0].id, batch.assets[1].id);
        let source_bytes = fs::read(&fixture.source).unwrap();
        for (image, asset) in batch.images.iter().zip(&batch.assets) {
            assert_eq!(asset.id.len(), 32);
            assert!(asset.id.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)));
            assert_eq!(asset.note_id, fixture.note.id);
            assert_eq!(asset.rel_path, format!("folders/inbox/notes/a/{}.png", asset.id));
            assert_eq!(image.src, format!("a/{}.png", asset.id));
            assert_eq!((asset.width, asset.height), (Some(2), Some(2)));
            let target = fixture.root.resolve(&asset.rel_path).unwrap();
            assert_eq!(fs::read(&target).unwrap(), source_bytes);
            assert_eq!(asset.sha256, storage::hex_lower(&crate::image_import::fingerprint(&target, &Default::default()).unwrap()));
        }
        assert!(fixture.source.exists(), "导入不能移走或改写源文件");
        let directory = fixture.root.resolve("folders/inbox/notes/a").unwrap();
        assert_eq!(fs::read_dir(directory).unwrap().count(), 2, "成功后不应留下临时文件");
    }

    #[test]
    fn publish_refuses_to_replace_an_existing_asset() {
        let fixture = fixture("no-overwrite");
        let temporary = fixture.root.path().join("temporary.png");
        let target = fixture.root.path().join("target.png");
        fs::write(&temporary, b"new").unwrap();
        fs::write(&target, b"old").unwrap();
        assert!(publish(&temporary, &target).unwrap_err().contains("拒绝覆盖"));
        assert_eq!(fs::read(target).unwrap(), b"old");
        assert_eq!(fs::read(temporary).unwrap(), b"new");
    }

    #[test]
    fn published_copy_is_rejected_when_size_or_hash_changes() {
        let fixture = fixture("published-check");
        let target = fixture.root.path().join("published.png");
        let original = fs::read(&fixture.source).unwrap();
        let hash = crate::image_import::fingerprint(&fixture.source, &ImportCancellation::default()).unwrap();
        fs::write(&target, &original).unwrap();
        verify_published(&target, original.len() as u64, &hash, &ImportCancellation::default()).unwrap();

        let mut corrupted = original.clone();
        corrupted[0] ^= 0xff;
        fs::write(&target, corrupted).unwrap();
        let error = verify_published(&target, original.len() as u64, &hash, &ImportCancellation::default()).unwrap_err();
        assert!(error.contains("SHA-256"), "{error}");

        let shortened = &original[..original.len() - 1];
        fs::write(&target, shortened).unwrap();
        let shortened_hash: [u8; 32] = Sha256::digest(shortened).into();
        let error = verify_published(&target, original.len() as u64, &shortened_hash, &ImportCancellation::default()).unwrap_err();
        assert!(error.contains("大小"), "{error}");
    }

    #[test]
    fn invalid_input_is_rejected_before_creating_the_asset_directory() {
        let fixture = fixture("invalid");
        let source = fixture.root.path().join("source.txt");
        fs::write(&source, b"not an image").unwrap();
        assert!(import_inputs(&fixture.config, vec![ImageInput::File(source)], &Default::default()).is_err());
        assert!(!fixture.root.resolve("folders/inbox/notes/a").unwrap().exists());
    }

    #[test]
    fn config_rejects_a_body_outside_the_plugin_tree() {
        let directory = tempfile::Builder::new().prefix("nomo-attachment-outside").tempdir().unwrap();
        let body = directory.path().join("note.md");
        fs::write(&body, b"# note").unwrap();
        assert!(PluginAttachmentConfig::for_note_path(1, &body).is_err());
    }
}
