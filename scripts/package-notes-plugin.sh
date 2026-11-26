#!/usr/bin/env bash
# 把 Nomo 打成 PingGloss 的 `notes` 随包插件树（重构文档 §9 P1）：
#   .artifacts/plugins/notes-<时间戳>/share/pingloss/plugins/notes/
#     ├── plugin.json          清单（§5.4 第 2 条：version 与 min_host 由发布人主动改）
#     ├── bin/PinGloss-Note x Nomo   产物（D-D①：联名可执行名，内容就是 `nomo`）
#     ├── LICENSE              AGPL-3.0-or-later 原文（D-A② 那句"随包附许可声明"）
#     ├── SOURCE.md            **当趟生成**的源码要约（commit／构建命令／产物 sha256／三年有效期）
#     └── licenses/            组件各自许可四份（摆法照 package-linux.sh，不在这里另造口径）
#
# 这一格只**组装**、不自装不发布（AGENTS：scripts/ 的产物写 `.artifacts`，绝不自动安装／打标签／发布）。
# 宿主按清单 `entry.path` = `bin/PinGloss-Note x Nomo` 拉起这颗进程（D-D①：二进制名为联名可执行名，
# 历史栏／待办／热键的调用方一行不改）；插件模式靠 spawn 那两位 env 判定（见
# `src-gpui/src/plugin/endpoints.rs`），不是靠 `--mode`，所以清单用 `path` 那一形、不用 `self_mode`。
#
# 本脚本只构建 Linux target；插件数据面已按平台分形：Linux/Unix 用 socket，Windows 用 named pipe。
# Windows 的随包树由 `package-notes-plugin.ps1` 组装，本脚本不跨平台复制产物。
#
# 清单那三位与 root Cargo.toml 解耦（重构文档 §5.4 第 2 条）：`version`＝插件包自己的号（0.3.1）、
# `min_host`＝宿主里能转发全部七条 op（含 `notes.rename`，落地于 v0.9.597）的那一下限。二者都写在
# `scripts/packaging/notes-plugin.json` 里、由发布人主动改，不随 Nomo 涨版自动动。
set -euo pipefail

usage() {
  printf 'Usage: %s [--skip-build]\n' "$0"
}

[[ "$(uname -s)" == Linux && "$(uname -m)" == x86_64 ]] || {
  echo 'Run on x86_64 Linux (this script builds the Linux plugin payload).' >&2
  exit 1
}

skip_build=false
while (($#)); do
  case "$1" in
    --skip-build) skip_build=true ;;
    -h|--help) usage; exit 0 ;;
    *) echo "Unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
  shift
done

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
target=x86_64-unknown-linux-gnu
target_dir="$(cargo metadata --locked --no-deps --format-version 1 | jq -r '.target_directory')"

if ! $skip_build; then
  # GPUI 的 x11 那一头在链接期点名 `-lxkbcommon-x11`，而链接器只认 dev 符号链
  # `libxkbcommon-x11.so`；本机（Ubuntu）的 `libxkbcommon-x11` 运行时包只带 `.so.0`，`.pc` 也没装。
  # 差的就是那一枚链，报出来却像"编译失败"（`找不到 -lxkbcommon-x11`）。这一格在 `target/` 下现补：
  # 走 `LIBRARY_PATH` 而不写 `RUSTFLAGS="-L …"`——后者会改到每一枚依赖的指纹，把整棵 Release 树重编一遍。
  # 只在确实缺的时候动手，不碰系统库目录（不要 sudo）。
  for soname in xkbcommon-x11; do
    case "$(cc -print-file-name="lib$soname.so")" in
      /*) ;;  # 链接器本来就找得到 ⇒ 什么都不加
      *)
        runtime="$(ls -1 "/usr/lib/"*-linux-gnu"/lib$soname".so.* /usr/lib/"lib$soname".so.* 2>/dev/null | sort -V | head -1 || true)"
        [[ -n "$runtime" ]] || {
          echo "缺 lib$soname 的运行时库（连 .so.0 都没有），装提供它的包后再打包。" >&2
          exit 1
        }
        mkdir -p "$target_dir/native-link"
        ln -sfn "$runtime" "$target_dir/native-link/lib$soname.so"
        export LIBRARY_PATH="$target_dir/native-link${LIBRARY_PATH:+:$LIBRARY_PATH}"
        echo "[链接] 补 $soname 的 dev 符号链: $target_dir/native-link/lib$soname.so -> $runtime"
        ;;
    esac
  done
  echo "[构建] cargo build --locked --release --target $target --bin nomo"
  cargo build --locked --release --target "$target" --bin nomo
fi
# Nomo 编出来的那颗就叫 `nomo`；进插件树按清单 entry 改名为 `bin/PinGloss-Note x Nomo`（内容同一颗）。
binary="$target_dir/$target/release/nomo"
[[ -x "$binary" ]] || { echo "找不到产物 $binary（先不带 --skip-build 编一发）" >&2; exit 1; }
file "$binary" | grep -q 'ELF 64-bit.*x86-64'
if ldd "$binary" | grep 'not found'; then
  echo 'Missing shared libraries' >&2
  exit 1
fi

stamp="$(date +%Y%m%d-%H%M%S)"
stage="$root/.artifacts/plugins/notes-$stamp"
plugin_rel="share/pingloss/plugins/notes"
mkdir -p "$stage/$plugin_rel/bin"

# ① 清单：原样搬进随包那一格（0644）。
install -m 644 scripts/packaging/notes-plugin.json "$stage/$plugin_rel/plugin.json"
# ② 那颗产物按 `entry.path` 摆进 `bin/PinGloss-Note x Nomo`（可执行位 0755）。
install -m 755 "$binary" "$stage/$plugin_rel/bin/PinGloss-Note x Nomo"

# ③ 许可原文随包（D-A②／重构文档 §5.4 第 5 条那句"若选 AGPL 分发，必须随包附源码与许可声明"）。
#    ⚠ 这一棵树走的是**用户自装**那一档：装出去的那一格自己带许可，不指望安装包里的 notices ——
#    闭源安装包内置的那颗 notes 是本仓之外另一支（pingloss 自己 `apps/pingloss-notes` 编出来的侧车），
#    与这一棵树无关，也不许把它塞进那一颗的位置（D-A②／§10 R-1）。
install -m 644 LICENSE "$stage/$plugin_rel/LICENSE"
#    ⚠ 随包树这一头发多了三位（`LICENSE`／`licenses/`／`SOURCE.md`），清单那一格没动 ⇒ 真把这棵树发出去
#    那一发要把 `scripts/packaging/notes-plugin.json` 的 `version` 抬一格（§5.4 第 2 条：这一位与 Nomo 版本
#    解耦、由发布人主动改）。本仓今天还没有对外发过这一棵树，所以这一发没动那位（还是 `0.3.1`）。
#    组件各自许可那一份照 `package-linux.sh` 的摆法（同名同来源，不在这里另造一套口径）：
#    桌面包放 `usr/share/doc/nomo/`，插件树里没有 doc 那一层，就放在包自己那一格下面的 `licenses/`。
mkdir -p "$stage/$plugin_rel/licenses"
for pair in 'icons/LICENSE:LICENSE-Lucide' 'licenses/LICENSE-STIX:LICENSE-STIX' \
            'licenses/LICENSE-Rendering:LICENSE-Rendering' 'licenses/LICENSE-Export:LICENSE-Export'; do
  src="src-gpui/assets/${pair%%:*}"
  [[ -f "$src" ]] || { echo "缺组件许可原文 $src ⇒ 许可清单不完整，不停在产物上也不发。" >&2; exit 1; }
  install -m 644 "$src" "$stage/$plugin_rel/licenses/${pair#*:}"
done

# ④ 源码那一纸是**当趟生成**的，不写死：commit、构建命令、产物 sha256、打包时间都取自现场 ——
#    AGPL-3.0 §13 要的是"随二进制交出完整对应源码，或 §11 那一条书面要约"，而要约要能对上号：
#    拿到的那一颗算得出 sha256、并且指得回某一枚 commit，这一纸才算数，抄一份静态文本不算。
commit="$(git -C "$root" rev-parse HEAD)"
source_url="$(git -C "$root" remote get-url origin 2>/dev/null || echo '')"
nomo_version="$(sed -n 's/^version = "\([^"]*\)".*/\1/p' Cargo.toml | head -1)"
plugin_version="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1],encoding="utf-8"))["version"])' \
  "$stage/$plugin_rel/plugin.json")"
bin_sha="$(sha256sum "$stage/$plugin_rel/bin/PinGloss-Note x Nomo" | cut -c1-64)"
built_at="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
# 纸上那句"sha256 与源码一一对应"只在树干净时成立：未提交的改动不进 commit，抄上去就成了假账。
# `--skip-build` 那一趟更容易碰见（产物是早先那次构建留下的），所以这一位现读、把话头跟着收。
dirty="$(git -C "$root" status --porcelain --untracked-files=normal)"
if [[ -n "$dirty" ]]; then
  n="$(printf '%s\n' "$dirty" | grep -c '')"
  workspace_state="**有 $n 处未提交改动** ⇒ 上面的 commit 记不下这一趟的改动，那一行构建命令照原样重编**不保证**复现出同一个 sha256"
  echo "[警告] 工作区有 $n 处未提交改动 ⇒ SOURCE.md 上那句对账话会写成不保证复现。" >&2
else
  workspace_state="干净，就是上面那一枚 commit 的那棵树 ⇒ 照上面的构建命令重编应能复现同一个 sha256"
fi
[[ ${#commit} -eq 40 ]] || { echo "读不出 40 位的 commit（$commit）⇒ 源码那一纸指不回去" >&2; exit 1; }
[[ -n "$source_url" ]] || echo "[提示] 这一份 clone 没有 origin remote ⇒ 要约里只留 commit，索取方式写邮件那条。" >&2
cat > "$stage/$plugin_rel/SOURCE.md" <<EOF
# \`notes\` 插件包的许可与源码声明

这一格是 **PinGloss-Note x Nomo** 的 \`notes\` 插件（清单＝同目录 \`plugin.json\`，可执行＝\`bin/PinGloss-Note x Nomo\`），
由 Nomo 以插件模式构建。Nomo 采用 **AGPL-3.0-or-later**：原文见同目录 \`LICENSE\`，随包的第三方组件
许可见 \`licenses/\`。按 AGPL-3.0 §13，随可执行文件交出的是**完整对应源码**，或下面这一纸指向源码的要约。

## 产物 ⇄ 源码对账

| 项 | 值 |
| --- | --- |
| 许可 | AGPL-3.0-or-later（\`LICENSE\`） |
| Nomo 产品版本 | $nomo_version |
| 插件包版本 | $plugin_version（与 Nomo 版本解耦，见清单） |
| 对应源码 | 下面那一枚 commit 的整棵仓库（含 \`src-gpui/\` 全部源码、构建脚本、字体与图标资源） |
| commit | \`$commit\` |
| 源码地址 | ${source_url:-（这一份构建里没有 remote 地址，按下面"索取方式"那一条走）} |
| 构建命令 | \`cargo build --locked --release --target $target --bin nomo\` |
| 产物 sha256 | \`$bin_sha\` |
| 打包那一下的工作区 | $workspace_state |
| 打包时间 | $built_at（UTC） |

拿到 \`bin/PinGloss-Note x Nomo\` 的人算一下 sha256 就能核对：对上表里那一串 ⇒ 这一颗就是那一枚 commit 的产物。

## 修改说明

- 产物由上表那一枚 commit 的源码树构建；**那一下工作区是否干净记在上表「打包那一下的工作区」那一行** ——
  只有那一行写"干净"时，sha256 与 commit 才是一一对应的，重编者照构建命令复现即可自证。
- "插件模式"那一层（\`src-gpui/src/plugin/\`：数据面 socket、\`notes.db\` 数据层、七条 op 的派发）本来就是
  本仓库源码的一部分，不是外挂补丁；它由宿主通过 \`PINGLOSS_PLUGIN_SOCKET\`／\`PINGLOSS_PLUGIN_DATA\`
  两位环境变量启用，没有那两位就还是桌面那颗 Nomo。

## 索取方式与有效期

自本包发布起**三年内有效**（AGPL-3.0 §11 那一档书面要约）：任何人可索取完整对应源码。

1. 自己取：\`git clone $source_url && git checkout $commit\`；
2. 索取：致信仓库维护者（issue 或 README 上那个邮箱），说明要哪一枚 commit 的对应源码，以离线介质或
   公开地址交回，不收费（介质成本除外）。

*Written offer valid for at least three years, in accordance with AGPL-3.0 section 11 — the complete
corresponding source is the repository at the commit above.*
EOF

# 自证①：清单读得动、十一位必填齐备、三位与发布人应核对的取值一致、七条 op 与运行侧
# [`plugin::runtime::Engine::handle`] 一字不差（形状闸同宿主 `crates/pg-plugin/src/manifest.rs`）。
python3 - "$stage/$plugin_rel/plugin.json" "$root/src-gpui/src/plugin/runtime.rs" <<'PY'
import json, re, sys
from pathlib import Path
m = json.load(open(sys.argv[1], encoding="utf-8"))
required = ["id", "kind", "version", "protocol", "min_host", "entry",
            "capabilities", "permissions", "operations", "license", "signature"]
missing = [f for f in required if f not in m]
assert not missing, f"清单缺必填位：{missing}"
assert m["id"] == "notes", m["id"]
assert m["kind"] == "ui", m["kind"]
assert m["protocol"] == 1, m["protocol"]
assert m["version"] == "0.3.1", m["version"]          # 与 Nomo 版本解耦（§5.4②）
assert m["min_host"] == "0.9.597", m["min_host"]      # 转发 notes.rename 的那一下限
assert m["entry"] == {"path": "bin/PinGloss-Note x Nomo"}, m["entry"]  # D-D①：联名可执行名
assert m["signature"] is None, m["signature"]         # 随包内置＝null，由安装包整体校验兜底
assert m.get("display_name") == "PinGloss-Note x Nomo", m.get("display_name")  # 宿主 0.9.638 起读那位（屏上第一列；没写回 id）
ops = [o["id"] for o in m["operations"]]
expected = ["folders.snapshot", "notes.open", "notes.list", "note.new", "notes.rename", "note.delete", "notes.info"]
assert ops == expected, ops
runtime = Path(sys.argv[2]).read_text(encoding="utf-8")
# 先圈定 impl Engine，再按 handle 自身缩进取方法体，兼容签名换行与可见性修饰。
engine = re.search(r"(?ms)^impl Engine \{.*?^\}", runtime)
assert engine, "runtime.rs 里找不到 impl Engine 的完整落点"
engine_text = engine.group(0)
start = re.search(r"(?m)^(?P<indent>[ \t]+)(?:pub(?:\([^)]*\))?\s+)?fn\s+handle\s*\(", engine_text)
assert start, "runtime.rs 里找不到 Engine::handle 的完整落点"
body_start = engine_text.find("{", start.end())
assert body_start >= 0, "Engine::handle 的签名后找不到方法体"
indent = start.group("indent")
end = re.search(r"(?m)^" + re.escape(indent) + r"\}[ \t]*\r?$", engine_text[body_start:])
assert end, "Engine::handle 的方法体没有按自身缩进收口"
handle = engine_text[start.start():body_start + end.end()]
for op in expected:
    assert re.search(r'"' + re.escape(op) + r'"\s*=>', handle), f"runtime handler 缺 {op}"
print(f"[清单] plugin.json 十一位齐＋display_name 在场、version={m['version']}、min_host={m['min_host']}、七条 op 与 runtime handler 一致")
PY

# 自证②：那颗产物独立手起 `--probe` 要吐一份自述 JSON、退 0（不碰显示、不碰数据面；口径同 `pingloss-todo`）。
# 没有宿主 spawn 的两格 env ⇒ `data_socket`／`data_dir` 该是 null，不是错——装机的人据此分得清「没装好」。
probe="$("$stage/$plugin_rel/bin/PinGloss-Note x Nomo" --probe)"
python3 - "$probe" <<'PY'
import json, sys
d = json.loads(sys.argv[1])
assert d["app"] == "nomo" and d["plugin_id"] == "notes", d
assert d["display_name"] == "PinGloss-Note x Nomo", d
assert d["mode"] == "probe" and d["protocol"] == 1, d
assert d["data_socket"] is None and d["data_dir"] is None, d
print(f"[探针] bin/PinGloss-Note x Nomo --probe 交回联名自述、退 0（Nomo 版本 {d['version']}）")
PY

# 自证③：许可那三位齐备、且**这一趟的**读数写在纸上（D-A②／§5.4 第 5 条那一句的出口）。
#   · `LICENSE` 首行是 AGPL 的抬头（搬错文件＝首行不对）；
#   · `licenses/` 四份组件许可在场、非空；
#   · `SOURCE.md` 里 commit＝当趟 `git rev-parse HEAD`、sha256＝当趟算出的那串、构建命令带 `--bin nomo`、
#     要约那一句带"三年"；任一位对不上就停在这里，而不是等发布人读一遍。
python3 - "$stage/$plugin_rel" "$commit" "$bin_sha" "$workspace_state" <<'PY'
import hashlib
import json
import sys
from pathlib import Path

rel = Path(sys.argv[1])
commit, bin_sha, workspace_state = sys.argv[2], sys.argv[3], sys.argv[4]
head = (rel / "LICENSE").read_text(encoding="utf-8").splitlines()[0]
assert "GNU AFFERO GENERAL PUBLIC LICENSE" in head, head
for name in ("LICENSE-Lucide", "LICENSE-STIX", "LICENSE-Rendering", "LICENSE-Export"):
    assert (rel / "licenses" / name).stat().st_size > 0, name
offer = (rel / "SOURCE.md").read_text(encoding="utf-8")
for needle, label in ((commit, "当趟的 commit"),
                      (bin_sha, "当趟算出的产物 sha256"),
                      (workspace_state, "当趟读出的工作区状态"),
                      ("--bin nomo", "构建命令那一位"),
                      ("三年", "要约的有效期"),
                      ("src-gpui/src/plugin", "修改说明那一节指到的功能层")):
    assert needle in offer, f"SOURCE.md 里读不到{label}：{needle}"
actual = hashlib.sha256((rel / "bin" / "PinGloss-Note x Nomo").read_bytes()).hexdigest()
assert actual == bin_sha, f"纸上的 sha256 与产物不符：{bin_sha} vs {actual}"
manifest = json.loads((rel / "plugin.json").read_text(encoding="utf-8"))
assert manifest["license"] == {"required": False, "trial_days": 0}, manifest["license"]
print(f"[许可] LICENSE（AGPL 原文）＋ licenses/ 四份组件许可＋ SOURCE.md 一纸（{len(offer)} 字符、"
      f"commit {commit[:9]}、产物 sha256 现场复核）；清单 `license.required` 仍为 false"
      f"（copyleft 随包与宿主的授权闸是两件事）")
PY

echo "Nomo notes 插件随包树: $stage/$plugin_rel"
