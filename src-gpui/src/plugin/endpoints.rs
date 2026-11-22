//! 宿主 spawn 时那两位环境变量读出来的落点。缺一位就说明「这颗不是宿主拉起来的」⇒ 走桌面模式。
//!
//! ⚠ **`PINGLOSS_PLUGIN_SOCKET` 在子进程环境里装的是数据面那颗的名字**（`plugin-<id>.sock` 那个形状），
//! 不是控制面 `plugin.sock`。名字与值是两档（控制面那颗的覆盖位同名同姓，见宿主 `pg_core::ipc` 头上那条 ⚠），
//! 若以为「env 里给的当然是控制面」，症状正是宿主那句「插件起来了、宿主永远连不上」。

use std::path::PathBuf;

/// 数据面 socket 那颗覆盖位（宿主 `pg_core::ipc::ENV_PLUGIN_SOCKET` 同值）。
pub const ENV_PLUGIN_SOCKET: &str = "PINGLOSS_PLUGIN_SOCKET";

/// 数据目录那颗（宿主 `pg_plugin::overrides::PLUGIN_DATA_ENV` 同值，指向 `<data>/plugins-data/<id>/`）。
pub const ENV_PLUGIN_DATA: &str = "PINGLOSS_PLUGIN_DATA";

/// 插件模式的两格落点：绑哪条 socket、往哪一层写运行时数据。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoints {
    /// 数据面 socket 的路径（本进程要绑的那一条）。
    pub data_socket: PathBuf,
    /// `<data>/plugins-data/<id>/`，宿主在 spawn 之前已建好。
    pub data_dir: PathBuf,
}

impl Endpoints {
    /// 读那两位 env。**两位都在** ⇒ 插件模式；任一位缺失 ⇒ `None`（桌面模式，不是错）。
    ///
    /// 桌面用户手上不会有一颗宿主 spawn 才塞得进来的 env，所以「没有 = 不在插件模式」是这条判定
    /// 的正确读法；只有「有 socket 没 data」这种半套装才值得单独报一句——那才是宿主 spawn 坏了。
    pub fn from_env() -> Option<Self> {
        Self::from_values(
            std::env::var_os(ENV_PLUGIN_SOCKET),
            std::env::var_os(ENV_PLUGIN_DATA),
        )
    }

    /// [`Endpoints::from_env`] 的纯那一半：判定只是「两格 Option 齐不齐」，挪出来才不必在判据里
    /// 动真进程 env（edition 2024 起 `set_var`/`remove_var` 是 `unsafe`，且会串到同仓其他测试）。
    pub(crate) fn from_values(
        socket: Option<std::ffi::OsString>,
        data: Option<std::ffi::OsString>,
    ) -> Option<Self> {
        match (socket, data) {
            (Some(data_socket), Some(data_dir)) => Some(Self {
                data_socket: PathBuf::from(data_socket),
                data_dir: PathBuf::from(data_dir),
            }),
            (Some(_), None) => {
                eprintln!(
                    "插件模式：有 {ENV_PLUGIN_SOCKET} 却没有 {ENV_PLUGIN_DATA}，宿主的 spawn 合同少一位 ⇒ 退回桌面"
                );
                None
            }
            (None, _) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    #[test]
    fn both_present_is_plugin_mode_and_solo_socket_falls_back() {
        // 两位都在 ⇒ 插件模式，两格路径原样递进去。
        let got = Endpoints::from_values(
            Some(OsString::from("/run/pingloss/plugin-notes.sock")),
            Some(OsString::from("/data/plugins-data/notes")),
        )
        .expect("两位齐了该进插件模式");
        assert_eq!(
            got.data_socket,
            PathBuf::from("/run/pingloss/plugin-notes.sock")
        );
        assert_eq!(got.data_dir, PathBuf::from("/data/plugins-data/notes"));

        // 有 socket 没 data ⇒ 半套装，退回桌面（None）。
        assert!(
            Endpoints::from_values(Some(OsString::from("/x.sock")), None).is_none(),
            "少一位不该被当成插件模式"
        );
        // 都没有 ⇒ 桌面模式，不是错。
        assert!(Endpoints::from_values(None, None).is_none());
    }
}
