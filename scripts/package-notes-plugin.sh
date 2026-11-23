#!/usr/bin/env bash
# 把 Nomo 打成 PingGloss 的 `notes` 随包插件树（重构文档 §9 P1）：
#   .artifacts/plugins/notes-<时间戳>/share/pingloss/plugins/notes/{plugin.json,bin/pingloss-notes}
#
# 这一格只**组装**、不自装不发布（AGENTS：scripts/ 的产物写 `.artifacts`，绝不自动安装／打标签／发布）。
# 宿主按清单 `entry.path` = `bin/pingloss-notes` 拉起这颗进程（D-D①：二进制名沿用 `pingloss-notes`，
# 历史栏／待办／热键的调用方一行不改）；插件模式靠 spawn 那两位 env 判定（见
# `src-gpui/src/plugin/endpoints.rs`），不是靠 `--mode`，所以清单用 `path` 那一形、不用 `self_mode`。
#
# ⚠ **Linux-first**：插件模式的数据面（unix socket）今天只在 Linux/Unix 接线，非 unix 上 `runtime::run`
#    碰 GPUI 之前就退 2。这一发也照 `package-linux.sh` 钉在 x86_64 Linux。
#
# 清单那三位与 root Cargo.toml 解耦（重构文档 §5.4 第 2 条）：`version`＝插件包自己的号（0.2.0）、
# `min_host`＝宿主里能转发全部六条 op（含 `notes.rename`，落地于 v0.9.597）的那一下限。二者都写在
# `scripts/packaging/notes-plugin.json` 里、由发布人主动改，不随 Nomo 涨版自动动。
set -euo pipefail

usage() {
  printf 'Usage: %s [--skip-build]\n' "$0"
}

[[ "$(uname -s)" == Linux && "$(uname -m)" == x86_64 ]] || {
  echo 'Run on x86_64 Linux (P1 的插件数据面只在 unix 接线)。' >&2
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
# Nomo 编出来的那颗就叫 `nomo`；进插件树按清单 entry 改名为 `bin/pingloss-notes`（内容同一颗）。
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
# ② 那颗产物按 `entry.path` 摆进 `bin/pingloss-notes`（可执行位 0755）。
install -m 755 "$binary" "$stage/$plugin_rel/bin/pingloss-notes"

# 自证①：清单读得动、十一位必填齐备、三位与发布人应核对的取值一致、六条 op 与运行侧
# [`plugin::runtime::Engine::handle`] 一字不差（形状闸同宿主 `crates/pg-plugin/src/manifest.rs`）。
python3 - "$stage/$plugin_rel/plugin.json" <<'PY'
import json, sys
m = json.load(open(sys.argv[1], encoding="utf-8"))
required = ["id", "kind", "version", "protocol", "min_host", "entry",
            "capabilities", "permissions", "operations", "license", "signature"]
missing = [f for f in required if f not in m]
assert not missing, f"清单缺必填位：{missing}"
assert m["id"] == "notes", m["id"]
assert m["kind"] == "ui", m["kind"]
assert m["protocol"] == 1, m["protocol"]
assert m["version"] == "0.2.0", m["version"]          # 与 Nomo 版本解耦（§5.4②）
assert m["min_host"] == "0.9.597", m["min_host"]      # 转发 notes.rename 的那一下限
assert m["entry"] == {"path": "bin/pingloss-notes"}, m["entry"]  # D-D①：名沿用 pingloss-notes
assert m["signature"] is None, m["signature"]         # 随包内置＝null，由安装包整体校验兜底
ops = [o["id"] for o in m["operations"]]
assert ops == ["folders.snapshot", "notes.open", "notes.list", "note.new", "notes.rename", "notes.info"], ops
print(f"[清单] plugin.json 十一位齐、version={m['version']}、min_host={m['min_host']}、六条 op 与运行侧一致")
PY

# 自证②：那颗产物独立手起 `--probe` 要吐一份自述 JSON、退 0（不碰显示、不碰数据面；口径同 `pingloss-todo`）。
# 没有宿主 spawn 的两格 env ⇒ `data_socket`／`data_dir` 该是 null，不是错——装机的人据此分得清「没装好」。
probe="$("$stage/$plugin_rel/bin/pingloss-notes" --probe)"
python3 - "$probe" <<'PY'
import json, sys
d = json.loads(sys.argv[1])
assert d["app"] == "nomo" and d["plugin_id"] == "notes", d
assert d["mode"] == "probe" and d["protocol"] == 1, d
assert d["data_socket"] is None and d["data_dir"] is None, d
print(f"[探针] bin/pingloss-notes --probe 交回自述、退 0（Nomo 版本 {d['version']}）")
PY

echo "Nomo notes 插件随包树: $stage/$plugin_rel"
