//! 数据面办事那一侧的四格：处理器交回来的结果、一次吸帧的读数、向宿主发请求的答复、
//! 以及把 [`Dispatch`] 编成线上答复帧的那一步。宿主 `pg-plugin-client::runtime` 的原生复刻。

use serde_json::Value;

use crate::plugin::envelope::Envelope;

/// 处理器交回来的一格：办成了（结果原样进 `Reply.payload`），或没办成（那两位进 `Failure.error`）。
#[derive(Clone, Debug, PartialEq)]
pub enum Dispatch {
    Done(Value),
    Failed { code: String, message: String },
}

impl Dispatch {
    /// 「表里没有这条 op」那一档。`code` 用宿主数据面已有的那个词 `unknown_op`
    /// （`crates/pg-plugin/src/intake.rs` 头上那条 ⚠：数据面 `error.code` 今天只有 `unknown_op`／
    /// `permission_denied` 两位有出处），不新造第三个词。`message` 点名那条 op 与这一格插件 id，
    /// 因为读这句话的是宿主管理窗，它要能看出「是清单里没这条，还是插件自己没实现」。
    pub fn unknown_op(op: &str, plugin_id: &str) -> Self {
        Self::Failed {
            code: "unknown_op".to_string(),
            message: format!("`{op}` 不在 `{plugin_id}` 这一格插件的 op 表里"),
        }
    }
}

/// 一次 [`crate::plugin::session::Session::pump`] 的读数。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// 这一格没活：连接还活着，缓冲里没有完整一帧。
    Idle,
    /// 办成了一条（`request` 是宿主那趟转发的关联号，答复已发回）。
    Served { request: u64, op: String },
    /// 对端关闭 ⇒ 这条连接不能再信。
    Closed,
}

/// 插件向宿主发一条 `host.*` 请求得到的答复（与 [`Dispatch`] 同两形，方向相反）。
#[derive(Clone, Debug, PartialEq)]
pub enum HostReply {
    Done(Value),
    Failed { code: String, message: String },
}

/// 处理器交回的那一格编成线上那一帧。
pub fn answer_of(request: u64, dispatch: Dispatch) -> Envelope {
    match dispatch {
        Dispatch::Done(payload) => Envelope::Reply { request, payload },
        Dispatch::Failed { code, message } => Envelope::Failure {
            request,
            code,
            message,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn done_becomes_a_reply_and_failed_becomes_an_error_frame() {
        let ok = answer_of(7, Dispatch::Done(json!({"path": "folders/x/notes/1.md"})));
        assert_eq!(
            ok,
            Envelope::Reply {
                request: 7,
                payload: json!({"path": "folders/x/notes/1.md"})
            }
        );
        let bad = answer_of(
            7,
            Dispatch::Failed {
                code: "no_root".to_string(),
                message: "还没收到 root".to_string(),
            },
        );
        match bad {
            Envelope::Failure {
                request,
                code,
                message,
            } => {
                assert_eq!(request, 7);
                assert_eq!(code, "no_root");
                assert_eq!(message, "还没收到 root");
            }
            other => panic!("编成了 {other:?}"),
        }
    }

    #[test]
    fn unknown_op_uses_the_existing_code_and_names_the_plugin() {
        let d = Dispatch::unknown_op("notes.rename", "notes");
        match d {
            Dispatch::Failed { code, message } => {
                assert_eq!(code, "unknown_op", "不新造第三个词");
                assert!(message.contains("notes.rename") && message.contains("notes"), "{message}");
            }
            Dispatch::Done(_) => panic!("未知 op 不该是成功"),
        }
    }

    /// 编出去的失败帧要能被对面解回来（`code` 过段名闸：`no_root` 那种下划线名合法）。
    #[test]
    fn a_failure_frame_round_trips_through_the_wire() {
        let frame = answer_of(
            3,
            Dispatch::Failed {
                code: "unknown_folder".to_string(),
                message: "快照里没有这一号".to_string(),
            },
        );
        let line = frame.to_line();
        assert_eq!(line.parse::<Envelope>().unwrap(), frame);
    }
}
