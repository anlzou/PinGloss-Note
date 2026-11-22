//! 数据面一帧的合同（§2.3.5 那三行示例帧）：`v` 是这一面的号、`request` 配对一次往返、
//! `op` 说要办哪件事、`ok` 那一位说成没成。宿主 `pg_plugin::envelope` 的原生复刻（见 handshake.rs 头上那条 R-5/G-9）。
//!
//! ⚠ **`op` 与 `ok` 恰有其一**：写了 `op` 的是请求、写了 `ok` 的是答复；都给或都不给都是整帧形状错。
//! ⚠ **`payload` 必须在**（请求与成形答复都是）：缺着与 `{}` 与 `null` 是三件事，缺着会被读成后两件。
//! ⚠ **未知顶层字段一律拒**；`v`/`request` 收整数不收浮点（`1.0` 会被 serde_json 记成浮点，`as_u64()` 给 `None`）。
//!
//! `payload` 逐字带过，本层不解析插件业务。大对象不进帧（正文/图片只走路径）与 1 MiB 上限都是
//! 调用方的事实（见 [`crate::plugin::frame`]）。

use serde_json::{Map, Value};

pub use crate::plugin::handshake::PROTOCOL;

const V: &str = "v";
const REQUEST: &str = "request";
const OP: &str = "op";
const OK: &str = "ok";
const PAYLOAD: &str = "payload";
const ERROR: &str = "error";

/// 一帧解不开时点名的那一位与说清为什么的一句话。`field` 永远指回**顶层那一位**（嵌套错也报它所属的位）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BadMessage {
    pub field: String,
    pub message: String,
}

impl BadMessage {
    fn new(field: &str, message: impl Into<String>) -> Self {
        Self {
            field: field.to_string(),
            message: message.into(),
        }
    }
}

impl std::fmt::Display for BadMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "`{}`：{}", self.field, self.message)
    }
}

/// 数据面的一帧：一形请求、两形答复（成／不成）。
#[derive(Clone, Debug, PartialEq)]
pub enum Envelope {
    Request {
        request: u64,
        op: String,
        payload: Value,
    },
    Reply {
        request: u64,
        payload: Value,
    },
    Failure {
        request: u64,
        code: String,
        message: String,
    },
}

impl Envelope {
    /// 这一形除公共两位以外还认哪几位（写侧读侧同一份名单）。
    fn allowed(&self) -> &'static [&'static str] {
        match self {
            Self::Request { .. } => &[V, REQUEST, OP, PAYLOAD],
            Self::Reply { .. } => &[V, REQUEST, OK, PAYLOAD],
            Self::Failure { .. } => &[V, REQUEST, OK, ERROR],
        }
    }

    fn shape(&self) -> &'static str {
        match self {
            Self::Request { .. } => "请求",
            Self::Reply { .. } => "答复",
            Self::Failure { .. } => "报错",
        }
    }

    /// 编成那个对象。`v` 由这一层写死，不接受调用方给的号（同一颗数从两处来才是分叉的开始）。
    pub fn to_value(&self) -> Value {
        let mut obj = Map::new();
        obj.insert(V.to_string(), Value::from(PROTOCOL));
        obj.insert(
            REQUEST.to_string(),
            Value::from(match self {
                Self::Request { request, .. }
                | Self::Reply { request, .. }
                | Self::Failure { request, .. } => *request,
            }),
        );
        match self {
            Self::Request { op, payload, .. } => {
                obj.insert(OP.to_string(), Value::String(op.clone()));
                obj.insert(PAYLOAD.to_string(), payload.clone());
            }
            Self::Reply { payload, .. } => {
                obj.insert(OK.to_string(), Value::Bool(true));
                obj.insert(PAYLOAD.to_string(), payload.clone());
            }
            Self::Failure { code, message, .. } => {
                obj.insert(OK.to_string(), Value::Bool(false));
                let mut error = Map::new();
                error.insert("code".to_string(), Value::String(code.clone()));
                error.insert("message".to_string(), Value::String(message.clone()));
                obj.insert(ERROR.to_string(), Value::Object(error));
            }
        }
        Value::Object(obj)
    }

    /// 编成一行的那串文本。⚠ 必须是一行：对面按换行分帧。
    pub fn to_line(&self) -> String {
        serde_json::to_string(&self.to_value()).expect("自己编的帧编不出错")
    }

    /// 已经解好的 `Value` → 一帧。
    pub fn from_value(value: &Value) -> Result<Self, BadMessage> {
        let obj = match value {
            Value::Object(map) => map,
            _ => return Err(BadMessage::new(V, "一帧的顶层要是一个对象")),
        };
        version_at(obj)?;
        let request = request_at(obj)?;
        let frame = match (obj.get(OP), obj.get(OK)) {
            (Some(_), Some(_)) => {
                return Err(BadMessage::new(
                    V,
                    "`op`（请求）与 `ok`（答复）只能给一个：两个都给＝不知道听谁的",
                ))
            }
            (None, None) => {
                return Err(BadMessage::new(
                    V,
                    "`op` 与 `ok` 得给一个：都不给＝这一帧既不是请求也不是答复",
                ))
            }
            (Some(_), None) => Self::Request {
                request,
                op: segment_at(obj, OP)?,
                payload: payload_at(obj)?,
            },
            (None, Some(v)) => match bool_at(OK, v)? {
                true => Self::Reply {
                    request,
                    payload: payload_at(obj)?,
                },
                false => {
                    let (code, message) = error_at(obj)?;
                    Self::Failure {
                        request,
                        code,
                        message,
                    }
                }
            },
        };
        // 多余的位放在取完值**以后**扫：名字先写错那一帧每一位都可能"多余"，先报那一位会引偏。
        if let Some(extra) = obj.keys().find(|k| !frame.allowed().contains(&k.as_str())) {
            return Err(BadMessage::new(
                V,
                format!("`{}` 这一形里没有 `{extra}` 这一位", frame.shape()),
            ));
        }
        Ok(frame)
    }
}

impl std::str::FromStr for Envelope {
    type Err = BadMessage;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let value: Value = serde_json::from_str(text)
            .map_err(|e| BadMessage::new(V, format!("JSON 读不动：{e}")))?;
        Self::from_value(&value)
    }
}

fn version_at(obj: &Map<String, Value>) -> Result<(), BadMessage> {
    let v = obj
        .get(V)
        .ok_or_else(|| BadMessage::new(V, "缺这一位：一帧要写明它说哪一版"))?;
    let raw = match v {
        Value::Number(n) => n.as_u64(),
        _ => None,
    }
    .ok_or_else(|| BadMessage::new(V, "要的是非负整数（`1.0` 那种浮点写法不收）"))?;
    let got = u32::try_from(raw).map_err(|_| BadMessage::new(V, format!("{raw} 放不进 32 位")))?;
    if got != PROTOCOL {
        return Err(BadMessage::new(
            V,
            format!("这一面只说 {PROTOCOL} 版，这一帧说的是 {got} 版"),
        ));
    }
    Ok(())
}

fn request_at(obj: &Map<String, Value>) -> Result<u64, BadMessage> {
    let v = obj
        .get(REQUEST)
        .ok_or_else(|| BadMessage::new(REQUEST, "缺这一位：没有它答复配不回请求"))?;
    match v {
        Value::Number(n) => n.as_u64().ok_or_else(|| {
            BadMessage::new(REQUEST, "要的是非负整数（`7.0` 与 `-1` 都不是一个关联号）")
        }),
        _ => Err(BadMessage::new(REQUEST, "要的是整数")),
    }
}

/// `payload` 逐字带过、但**必须在**。
fn payload_at(obj: &Map<String, Value>) -> Result<Value, BadMessage> {
    match obj.get(PAYLOAD) {
        Some(v) => Ok(v.clone()),
        None => Err(BadMessage::new(
            PAYLOAD,
            "缺这一位：没有参数请写 `{}`，缺着与 `null` 是两件事",
        )),
    }
}

/// 一个名字要像名字不像路径：非空、无分隔符、无 `:`、首字符不是 `.`／`-`、无空白。
/// 与宿主清单 `check_segment`／`manifest::Reject` 同一族，判的是「进了 ops 表查找与日志」。
fn segment_at(obj: &Map<String, Value>, key: &str) -> Result<String, BadMessage> {
    let v = obj
        .get(key)
        .ok_or_else(|| BadMessage::new(key, "缺这一位"))?;
    let s = match v {
        Value::String(s) => s.clone(),
        _ => return Err(BadMessage::new(key, "要的是一个字符串名字")),
    };
    check_segment(key, &s)?;
    Ok(s)
}

fn check_segment(key: &str, s: &str) -> Result<(), BadMessage> {
    let bad = s.is_empty()
        || s.contains('/')
        || s.contains('\\')
        || s.contains(':')
        || s.chars().any(char::is_whitespace)
        || s.starts_with('.')
        || s.starts_with('-');
    if bad {
        return Err(BadMessage::new(
            key,
            format!("`{s}` 不像一个名字（不许有分隔符、空白、`:`，或以 `.`／`-` 开头）"),
        ));
    }
    Ok(())
}

fn bool_at(key: &str, v: &Value) -> Result<bool, BadMessage> {
    match v {
        Value::Bool(b) => Ok(*b),
        _ => Err(BadMessage::new(key, "要的是布尔（字符串 `\"true\"` 不算真）")),
    }
}

/// `error` 那一位：`code` 与 `message` 都要，`code` 还要过那颗段名闸；里面出错报顶层 `error`。
fn error_at(obj: &Map<String, Value>) -> Result<(String, String), BadMessage> {
    let raw = obj
        .get(ERROR)
        .ok_or_else(|| BadMessage::new(ERROR, "`ok: false` 要说错在哪：缺 `error` 那一位"))?;
    let error = match raw {
        Value::Object(map) => map,
        _ => return Err(BadMessage::new(ERROR, "这一位要的是一个对象")),
    };
    let code = match error.get("code") {
        Some(v) => match v {
            Value::String(s) => s.clone(),
            _ => return Err(BadMessage::new(ERROR, "`error.code` 要的是字符串")),
        },
        None => {
            return Err(BadMessage::new(
                ERROR,
                "`error` 里缺 `code`：那一个词是给调用方程序读的",
            ))
        }
    };
    check_segment(ERROR, &code)?;
    let message = match error.get("message") {
        Some(v) => match v {
            Value::String(s) => s.clone(),
            _ => return Err(BadMessage::new(ERROR, "`error.message` 要的是字符串")),
        },
        None => return Err(BadMessage::new(ERROR, "`error` 里缺 `message`")),
    };
    if let Some(extra) = error
        .keys()
        .find(|k| !["code", "message"].contains(&k.as_str()))
    {
        return Err(BadMessage::new(
            ERROR,
            format!("`error` 里只有 `code` 与 `message`，没有 `{extra}` 这一位"),
        ));
    }
    Ok((code, message))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// §2.3.5 那三行示例帧逐字抄进来当底：任何一位改名都会在这里红。
    const PLAN_REQUEST: &str = r#"{ "v": 1, "request": 7, "op": "record.start", "payload": { } }"#;
    const PLAN_REPLY: &str = r#"{ "v": 1, "request": 7, "ok": true, "payload": { } }"#;
    const PLAN_FAILURE: &str =
        r#"{ "v": 1, "request": 7, "ok": false, "error": { "code": "invalid_arg", "message": "..." } }"#;

    #[test]
    fn the_three_frames_the_plan_prints_parse_into_the_three_shapes() {
        assert_eq!(
            PLAN_REQUEST.parse::<Envelope>().unwrap(),
            Envelope::Request {
                request: 7,
                op: "record.start".to_string(),
                payload: json!({}),
            }
        );
        assert_eq!(
            PLAN_REPLY.parse::<Envelope>().unwrap(),
            Envelope::Reply {
                request: 7,
                payload: json!({}),
            }
        );
        assert_eq!(
            PLAN_FAILURE.parse::<Envelope>().unwrap(),
            Envelope::Failure {
                request: 7,
                code: "invalid_arg".to_string(),
                message: "...".to_string(),
            }
        );
    }

    #[test]
    fn every_shape_round_trips_and_stays_on_one_line() {
        for frame in [
            Envelope::Request {
                request: 7,
                op: "record.start".to_string(),
                payload: json!({}),
            },
            Envelope::Reply {
                request: 7,
                payload: json!({"path": "folders/a/b.png"}),
            },
            Envelope::Failure {
                request: 7,
                code: "invalid_arg".to_string(),
                message: "没有这一篇".to_string(),
            },
        ] {
            let line = frame.to_line();
            assert!(!line.contains('\n') && !line.contains('\r'), "编成多行：{line}");
            let back = line
                .parse::<Envelope>()
                .unwrap_or_else(|e| panic!("{line} 解不回来：{e}"));
            assert_eq!(back, frame);
            assert_eq!(frame.to_value().get(V).and_then(Value::as_u64), Some(1));
        }
    }

    #[test]
    fn a_frame_speaking_another_version_is_refused_at_that_word() {
        for bad in [
            json!({"request": 1, "op": "note.new", "payload": {}}),
            json!({"v": 2, "request": 1, "op": "note.new", "payload": {}}),
            json!({"v": "1", "request": 1, "op": "note.new", "payload": {}}),
            json!({"v": 1.0, "request": 1, "op": "note.new", "payload": {}}),
        ] {
            let e = Envelope::from_value(&bad).expect_err(&format!("{bad} 不该被收下"));
            assert_eq!(e.field, V);
        }
        let e = Envelope::from_value(&json!({"v": 2, "request": 1, "op": "x", "payload": {}}))
            .expect_err("2 版不该被收下");
        assert!(e.message.contains('2') && e.message.contains('1'), "{e}");
    }

    #[test]
    fn op_and_ok_are_the_dividing_line() {
        let e = Envelope::from_value(&json!({
            "v": 1, "request": 7, "op": "record.start", "ok": true, "payload": {},
        }))
        .expect_err("请求与答复两位都给不该被收下");
        assert_eq!(e.field, V);
        let e = Envelope::from_value(&json!({"v": 1, "request": 7, "payload": {}}))
            .expect_err("既没 op 也没 ok");
        assert_eq!(e.field, V);
        let e = Envelope::from_value(&json!({"v": 1, "request": 7, "ok": "true", "payload": {}}))
            .expect_err("ok 要布尔");
        assert_eq!(e.field, OK);
    }

    #[test]
    fn an_op_name_must_look_like_a_word_not_a_path() {
        for bad in ["", "..", "../start", "a/b", "a\\b", "-record", ".hidden", "re cord"] {
            let e = Envelope::from_value(&json!({
                "v": 1, "request": 1, "op": bad, "payload": {},
            }))
            .expect_err(&format!("`{bad}` 不该被当成 op 名"));
            assert_eq!(e.field, OP);
        }
        for good in ["record.start", "notes.open", "host.pin_list", "todo_new"] {
            assert!(Envelope::from_value(&json!({
                "v": 1, "request": 1, "op": good, "payload": {},
            }))
            .is_ok());
        }
    }

    #[test]
    fn a_payload_is_carried_verbatim_but_still_required() {
        let payload = json!({"note_id": 12, "frames": [1, 2, 3], "nested": {"a": null}});
        let frame = Envelope::from_value(&json!({
            "v": 1, "request": 4, "op": "notes.list", "payload": payload,
        }))
        .unwrap();
        match &frame {
            Envelope::Request { payload: got, .. } => assert_eq!(got, &payload),
            other => panic!("解成了 {other:?}"),
        }
        let e = Envelope::from_value(&json!({"v": 1, "request": 1, "op": "notes.open"}))
            .expect_err("缺载荷不该读成空载荷");
        assert_eq!(e.field, PAYLOAD);
    }

    #[test]
    fn extra_top_level_word_is_refused_after_values_are_read() {
        let e = Envelope::from_value(&json!({
            "v": 1, "request": 1, "ok": true, "payload": {}, "extra": 1,
        }))
        .expect_err("成形答复带多余位");
        assert_eq!(e.field, V);
        // 成形的答复里出现 error、没办成那帧带 payload，都在这一格被挡（不各写特判）。
        assert!(Envelope::from_value(&json!({
            "v": 1, "request": 1, "ok": true, "payload": {}, "error": {"code":"x","message":"y"},
        }))
        .is_err());
    }

    #[test]
    fn the_request_word_wants_a_plain_integer() {
        for (bad, _) in [
            (json!({"v": 1, "op": "x", "payload": {}}), "缺关联号"),
            (json!({"v": 1, "request": "7", "op": "x", "payload": {}}), "字符串"),
            (json!({"v": 1, "request": 7.0, "op": "x", "payload": {}}), "浮点"),
            (json!({"v": 1, "request": -1, "op": "x", "payload": {}}), "负数"),
        ] {
            let e = Envelope::from_value(&bad).expect_err(&format!("{bad} 不该被收下"));
            assert_eq!(e.field, REQUEST);
        }
        // 0 不是特殊值。
        assert_eq!(
            Envelope::from_value(&json!({"v": 1, "request": 0, "op": "x", "payload": {}})).unwrap(),
            Envelope::Request { request: 0, op: "x".to_string(), payload: json!({}) }
        );
    }
}
