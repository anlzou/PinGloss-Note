// P2：单行本地图片路径粘贴 → Markdown 图片。判定与转义移植自现有侧车
// （`apps/pingloss-notes` 的 `interactions.rs` / `events.rs`），保持口径一致。

use std::path::{Path, PathBuf};

/// 侧车接受的图片扩展名白名单。
fn is_supported_local_image_path(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    let Some(ext) = path.extension().and_then(|ext| ext.to_str()) else {
        return false;
    };
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" | "bmp" | "tif" | "tiff"
    )
}

/// 把一段剪贴板文本解析成本地图片路径：先按路径试（Windows 盘符必须先于 URL，
/// 否则盘符被当成 scheme），不成立才按 `file://` 解析；其它 scheme 一律拒。
fn local_image_path(text: &str) -> Option<PathBuf> {
    let unquoted = text
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .unwrap_or(text);
    let direct_path = PathBuf::from(unquoted);
    let path = if is_supported_local_image_path(&direct_path) {
        direct_path
    } else if let Ok(url) = url::Url::parse(unquoted) {
        if url.scheme() == "file" {
            url.to_file_path().ok()?
        } else {
            return None;
        }
    } else {
        return None;
    };
    is_supported_local_image_path(&path).then_some(path)
}

/// 路径统一 `/` 分隔。
fn markdown_path_string(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// 转义会打断 Markdown 图片目标的字符。
fn markdown_image_target(path: &str) -> String {
    path.chars()
        .flat_map(|ch| match ch {
            '\\' | '(' | ')' | '"' => vec!['\\', ch],
            _ => vec![ch],
        })
        .collect()
}

/// `alt` 取文件 stem，转义 `\` 与 `]`，空则 `image`。
fn markdown_image_alt(path: &Path) -> String {
    let alt = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .filter(|stem| !stem.is_empty())
        .unwrap_or("image");
    alt.chars()
        .flat_map(|ch| match ch {
            '\\' | ']' => vec!['\\', ch],
            _ => vec![ch],
        })
        .collect()
}

/// 单行本地图片路径或 `file://` 的原路径；远程 URL 与多行文本不转换。
pub(crate) fn pasted_image_path(text: &str) -> Option<PathBuf> {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed.contains('\n') || trimmed.contains('\r') {
        return None;
    }
    local_image_path(trimmed)
}

/// 若剪贴板文本是一行指向已存在本地图片的路径或 `file://`，生成 `![alt](path)`；
/// 否则返回 `None`（按原文本插入）。不复制文件、不下载远程图：本地路径保留原绝对路径。
pub(crate) fn pasted_image_markdown(text: &str) -> Option<String> {
    let path = pasted_image_path(text)?;
    let path_text = markdown_path_string(&path);
    Some(format!(
        "![{}]({})",
        markdown_image_alt(&path),
        markdown_image_target(&path_text)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_image(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, b"\x89PNG\r\n\x1a\nfake").unwrap();
        path
    }

    #[test]
    fn plain_local_image_path_becomes_markdown_with_stem_alt() {
        let dir = std::env::temp_dir().join(format!("nomo-p2-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let png = temp_image(&dir, "photo.png");
        let markdown = pasted_image_markdown(&png.to_string_lossy()).unwrap();
        assert!(markdown.starts_with("![photo]("), "{markdown}");
        assert!(markdown.contains(&markdown_path_string(&png)), "{markdown}");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn quoted_path_matches_unquoted() {
        let dir = std::env::temp_dir().join(format!("nomo-p2q-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let png = temp_image(&dir, "pic.jpg");
        let raw = png.to_string_lossy().into_owned();
        let quoted = pasted_image_markdown(&format!("\"{raw}\"")).unwrap();
        let plain = pasted_image_markdown(&raw).unwrap();
        assert_eq!(quoted, plain);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn file_url_equals_local_path() {
        let dir = std::env::temp_dir().join(format!("nomo-p2f-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let png = temp_image(&dir, "shot.gif");
        let url = url::Url::from_file_path(&png).unwrap().to_string();
        let markdown = pasted_image_markdown(&url).unwrap();
        assert!(markdown.starts_with("![shot]("), "{markdown}");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rejects_missing_non_image_multiline_and_remote() {
        let dir = std::env::temp_dir().join(format!("nomo-p2r-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        // 不存在的路径
        assert!(pasted_image_markdown("/definitely/not/here.png").is_none());
        // 非图片扩展名
        let txt = dir.join("notes.md");
        fs::write(&txt, "hi").unwrap();
        assert!(pasted_image_markdown(&txt.to_string_lossy()).is_none());
        // http(s) 远程：不下载、不转换
        assert!(pasted_image_markdown("https://example.com/a.png").is_none());
        assert!(pasted_image_markdown("http://example.com/a.png").is_none());
        // 多行
        let png = temp_image(&dir, "ok.webp");
        assert!(pasted_image_markdown(&format!("{}\nextra", png.to_string_lossy())).is_none());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn escapes_parenthesis_space_and_quote_in_target() {
        let target = "images/a (1) \"x\".png";
        let escaped = markdown_image_target(target);
        assert_eq!(escaped, "images/a \\(1\\) \\\"x\\\".png");
    }
}
