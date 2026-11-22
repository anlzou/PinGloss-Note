//! 握手那两位（`Hello` / `HelloOk`）：字段名、线上键序、两条拒绝规则。
//!
//! 这里是宿主 `pg_plugin::handshake` 的**原生复刻**，不跨 workspace 依赖：R-5 不许把两边的依赖树
//! 并起来、G-9 说 SDK 还没发布，而 `pg-plugin` 会连带拉进 `pg-core`。Nomo 只用手上已有的
//! `serde`/`serde_json` 把同一份线上合同重写一遍，判据＝与宿主字节级兼容（键序、`v`、两位拒绝）。
//!
//! ⚠ **键序是合同的一部分**：`HELLO_FIELDS` 的顺序照计划 §2.3.5 line 249 的书写顺序，结构体字段
//! 顺序与它逐位相同 ⇒ serde 序列化出的线上键序也是它。宿主侧那三条判据读的是原文键序，改一处必须
//! 连着改另一处。
//!
//! ⚠ **`deny_unknown_fields`**：§2.3.5「未知顶层字段一律拒绝」。这与清单的「未知字段忽略」
//! 故意相反 —— 线上帧收下不认得的位＝「对面以为说过的事其实没人处理」。

use serde::{Deserialize, Serialize};

/// 数据面/握手面共用的协议号（与宿主 `pg_plugin::envelope::PROTOCOL` 同值）。
///
/// ⚠ 它**不是**贴图面 `pg_core::PROTOCOL_VERSION`（那是主进程↔pinhost 那一面的号）。两颗号各自被
/// 各自那面的判据钉住，同源会让「改贴图协议号」把随包插件清单全判红。
pub const PROTOCOL: u32 = 1;

/// 计划 §2.3.5 那句 `Hello { ... }` 点名的五位，顺序照文档书写。
pub const HELLO_FIELDS: &[&str] = &[
    "protocol",
    "plugin_id",
    "plugin_version",
    "host_version",
    "pid",
];

/// `HelloOk { ... }` 点名的三位。与 `HELLO_FIELDS` 除 `protocol` 外不重合：方向感就在这里。
pub const HELLO_OK_FIELDS: &[&str] = &["protocol", "capabilities", "data_dir"];

/// 插件进程发往宿主的第一帧。字段顺序＝[`HELLO_FIELDS`]。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hello {
    /// 插件这一侧认的协议号。
    pub protocol: u32,
    /// 插件自报的 id（要与清单 `id` 逐字相同）。
    pub plugin_id: String,
    /// 插件自报的版本（semver 原文，不解析）。
    pub plugin_version: String,
    /// 插件看到的宿主版本（宿主 spawn 时给的那一位，插件只回显）。
    pub host_version: String,
    /// 插件自己的 pid（`std::process::id()`）。
    pub pid: u32,
}

/// 宿主回的确认帧。字段顺序＝[`HELLO_OK_FIELDS`]。`protocol` 是**宿主**正在用的号，不是复读插件的。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HelloOk {
    pub protocol: u32,
    /// 宿主认可的能力清单——就是清单那一份原样（清单是能力唯一真源）。
    pub capabilities: Vec<String>,
    /// 插件可写运行时数据的那一层（绝对路径，`<data>/plugins-data/<id>/`）。
    pub data_dir: String,
}

/// 收到 `Hello` 时宿主要对上的两位：我这颗认几号、我以为对面是谁。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Expectation<'a> {
    pub protocol: u32,
    pub plugin_id: &'a str,
}

/// §2.3.5 那两条拒绝规则，各有一句、不合并。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HelloRejection {
    Protocol { got: u32, want: u32 },
    PluginId { got: String, want: String },
}

impl HelloRejection {
    pub fn field(&self) -> &'static str {
        match self {
            Self::Protocol { .. } => "protocol",
            Self::PluginId { .. } => "plugin_id",
        }
    }
}

/// 判一条 `Hello` 能不能接。**只有那两条**，顺序照原文（先号后 id）。
pub fn check(hello: &Hello, expected: Expectation<'_>) -> Result<(), HelloRejection> {
    if hello.protocol != expected.protocol {
        return Err(HelloRejection::Protocol {
            got: hello.protocol,
            want: expected.protocol,
        });
    }
    if hello.plugin_id != expected.plugin_id {
        return Err(HelloRejection::PluginId {
            got: hello.plugin_id.clone(),
            want: expected.plugin_id.to_string(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hello() -> Hello {
        Hello {
            protocol: 1,
            plugin_id: "notes".into(),
            plugin_version: "0.1.0".into(),
            host_version: "0.9.131".into(),
            pid: 4242,
        }
    }

    /// 线上键序＝名单顺序（两处：`Hello` 与 `HelloOk`）。改结构体字段顺序要连着改名单常量。
    #[test]
    fn wire_key_order_is_the_const_order() {
        let line = serde_json::to_string(&hello()).unwrap();
        let mut at: Vec<(usize, &str)> = HELLO_FIELDS
            .iter()
            .map(|name| {
                (
                    line.find(&format!("\"{name}\":"))
                        .unwrap_or(usize::MAX),
                    *name,
                )
            })
            .collect();
        assert!(
            at.iter().all(|(pos, _)| *pos != usize::MAX),
            "Hello 线上认不全 HELLO_FIELDS 那五位：{line}"
        );
        at.sort();
        assert_eq!(
            at.iter().map(|(_, n)| *n).collect::<Vec<_>>(),
            HELLO_FIELDS,
            "Hello 的线上键序与 HELLO_FIELDS 不是同一串：{line}"
        );

        let ok = serde_json::to_string(&HelloOk {
            protocol: 1,
            capabilities: vec!["notes.open".into()],
            data_dir: "/data/plugins-data/notes".into(),
        })
        .unwrap();
        let mut at: Vec<(usize, &str)> = HELLO_OK_FIELDS
            .iter()
            .map(|name| {
                (
                    ok.find(&format!("\"{name}\":")).unwrap_or(usize::MAX),
                    *name,
                )
            })
            .collect();
        assert!(
            at.iter().all(|(pos, _)| *pos != usize::MAX),
            "HelloOk 线上认不全三位：{ok}"
        );
        at.sort();
        assert_eq!(
            at.iter().map(|(_, n)| *n).collect::<Vec<_>>(),
            HELLO_OK_FIELDS,
            "HelloOk 的线上键序漂了：{ok}"
        );
    }

    /// 多一位就拒（§2.3.5 未知顶层字段）。
    #[test]
    fn unknown_top_level_field_is_rejected() {
        let extra = r#"{"protocol":1,"plugin_id":"notes","plugin_version":"0.1.0","host_version":"0.9.3","pid":1,"extra":true}"#;
        let err = serde_json::from_str::<Hello>(extra)
            .expect_err("多出一位还解得开就是收下了不认得的位");
        assert!(err.to_string().contains("extra"), "没点出那一位：{err}");
    }

    /// 两条规则各有正反例；两位同时不对时先报协议。
    #[test]
    fn the_two_rules_and_their_order() {
        let expected = Expectation {
            protocol: 1,
            plugin_id: "notes",
        };
        assert_eq!(check(&hello(), expected), Ok(()));

        let mut both = hello();
        both.protocol = 2;
        both.plugin_id = "Notes".into();
        assert_eq!(
            check(&both, expected),
            Err(HelloRejection::Protocol { got: 2, want: 1 }),
            "两位同时不对时先报协议（原文顺序）"
        );
    }
}
