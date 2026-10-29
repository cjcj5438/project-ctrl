//! AI 子进程的 JSONL 分帧与入站消息分类。
//!
//! 分帧只认 `\n`（0x0A）：U+2028/U+2029 是合法的 JSON 字符串内容（多语言排版文本里常见），
//! 若按 Unicode 行分隔符切分，会把一条完整的 JSON 从中间劈开。行尾的 `\r` 只在紧邻换行时剥离，
//! 兼容 Windows 换行。非法 UTF-8 不视为错误——按有损方式解码，避免一处脏字节毁掉整条流。

// 本模块是分阶段接入的：`LineSplitter` 与 UI 应答序列化器要在后续步骤才被调用，
// 现在先由单元测试保证行为正确。
#![allow(dead_code)]

use serde_json::Value;

/// 单行上限。超过即视为异常，防止对端一直不发换行把内存吃光。
pub const MAX_LINE: usize = 16 << 20;

const OVERSIZED: &str = "oversized line";

/// 增量行拆分器：喂任意字节块，吐出完整的行。
#[derive(Default)]
pub struct LineSplitter {
    buf: Vec<u8>,
    /// 正在丢弃一个超长行的剩余字节，直到下一个 `\n` 为止。
    discarding: bool,
}

impl LineSplitter {
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂一块字节，返回本次凑齐的行。空行不产出任何东西。
    pub fn push(&mut self, chunk: &[u8]) -> Vec<Result<String, String>> {
        let mut out = Vec::new();
        for &b in chunk {
            if b == b'\n' {
                if self.discarding {
                    self.discarding = false;
                    self.buf.clear();
                    continue;
                }
                let mut line = std::mem::take(&mut self.buf);
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                if line.is_empty() {
                    continue;
                }
                out.push(Ok(String::from_utf8_lossy(&line).into_owned()));
                continue;
            }
            if self.discarding {
                continue;
            }
            self.buf.push(b);
            if self.buf.len() > MAX_LINE {
                self.buf.clear();
                self.discarding = true;
                out.push(Err(OVERSIZED.to_string()));
            }
        }
        out
    }
}

/// 入站消息三类：命令应答、UI 反向请求、事件。
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    Response {
        id: Option<String>,
        command: String,
        success: bool,
        data: Value,
        error: Option<String>,
    },
    UiRequest {
        id: String,
        method: String,
        title: Option<String>,
        placeholder: Option<String>,
        message: Option<String>,
    },
    Event {
        kind: String,
        raw: Value,
    },
}

fn opt_str(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_string)
}

/// 出错信息里最多带 200 个字符的行内容，避免把整行大 JSON 写进日志。
fn truncate(line: &str) -> String {
    line.chars().take(200).collect()
}

/// 把一行分类。未知事件类型一律容忍（只归类为 `Event`），绝不报错。
pub fn parse(line: &str) -> Result<Incoming, String> {
    let v: Value = serde_json::from_str(line).map_err(|e| e.to_string())?;
    if !v.is_object() {
        return Err(format!("不是 JSON 对象：{}", truncate(line)));
    }
    match v.get("type").and_then(Value::as_str) {
        Some("response") => Ok(Incoming::Response {
            id: opt_str(&v, "id"),
            command: opt_str(&v, "command").unwrap_or_default(),
            success: v.get("success").and_then(Value::as_bool).unwrap_or(false),
            data: v.get("data").cloned().unwrap_or(Value::Null),
            error: opt_str(&v, "error"),
        }),
        Some("extension_ui_request") => {
            let id = opt_str(&v, "id")
                .ok_or_else(|| format!("extension_ui_request 缺少 id：{}", truncate(line)))?;
            Ok(Incoming::UiRequest {
                id,
                method: opt_str(&v, "method").unwrap_or_default(),
                title: opt_str(&v, "title"),
                placeholder: opt_str(&v, "placeholder"),
                message: opt_str(&v, "message"),
            })
        }
        _ => Ok(Incoming::Event {
            kind: opt_str(&v, "type").unwrap_or_else(|| "unknown".to_string()),
            raw: v,
        }),
    }
}

fn encode(v: Value) -> String {
    let mut s = v.to_string();
    s.push('\n');
    s
}

/// 命令帧：`{"type":"<command>","id":..}`，`extra` 是对象时并入其余字段。
pub fn cmd(id: &str, command: &str, extra: Value) -> String {
    let mut m = serde_json::Map::new();
    m.insert("type".to_string(), Value::String(command.to_string()));
    m.insert("id".to_string(), Value::String(id.to_string()));
    if let Value::Object(e) = extra {
        m.extend(e);
    }
    encode(Value::Object(m))
}

/// UI 应答帧（用户填了值）。
pub fn ui_value(id: &str, value: &str) -> String {
    encode(serde_json::json!({
        "type": "extension_ui_response",
        "id": id,
        "value": value,
    }))
}

/// UI 应答帧（用户取消）。
pub fn ui_cancelled(id: &str) -> String {
    encode(serde_json::json!({
        "type": "extension_ui_response",
        "id": id,
        "cancelled": true,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn one(results: Vec<Result<String, String>>) -> Result<String, String> {
        assert_eq!(results.len(), 1, "应恰好产出一行：{results:?}");
        results.into_iter().next().unwrap()
    }

    #[test]
    fn 跨三次喂入的一行能拼回完整消息() {
        let line = r#"{"type":"response","id":"1","command":"prompt","success":true,"data":{"ok":1}}"#;
        let bytes = line.as_bytes();
        let cut1 = bytes.len() / 3;
        let cut2 = cut1 * 2;
        let mut s = LineSplitter::new();
        assert!(s.push(&bytes[..cut1]).is_empty());
        assert!(s.push(&bytes[cut1..cut2]).is_empty());
        let mut last = s.push(&bytes[cut2..]);
        last.extend(s.push(b"\n"));
        let text = one(last).unwrap();
        assert_eq!(text, line);
        match parse(&text).unwrap() {
            Incoming::Response { id, command, success, data, error } => {
                assert_eq!(id.as_deref(), Some("1"));
                assert_eq!(command, "prompt");
                assert!(success);
                assert_eq!(data, json!({"ok": 1}));
                assert!(error.is_none());
            }
            other => panic!("应解析为 Response：{other:?}"),
        }
    }

    #[test]
    fn crlf_剥掉回车() {
        let mut s = LineSplitter::new();
        let text = one(s.push(b"{\"type\":\"e\"}\r\n")).unwrap();
        assert_eq!(text, "{\"type\":\"e\"}");
        assert!(matches!(parse(&text).unwrap(), Incoming::Event { .. }));
    }

    #[test]
    fn 字符串里的_u2028_不切行() {
        let line = "{\"type\":\"e\",\"s\":\"甲\u{2028}乙\"}";
        assert!(line.contains('\u{2028}'));
        let mut s = LineSplitter::new();
        let mut out = s.push(format!("{line}\n{line}\n").as_bytes());
        assert_eq!(out.len(), 2, "U+2028 不能当成行分隔符：{out:?}");
        let text = out.pop().unwrap().unwrap();
        match parse(&text).unwrap() {
            Incoming::Event { raw, .. } => assert_eq!(raw["s"], json!("甲\u{2028}乙")),
            other => panic!("应解析为 Event：{other:?}"),
        }
    }

    #[test]
    fn 超长行报错一次并恢复() {
        let mut s = LineSplitter::new();
        let big = vec![b'a'; MAX_LINE + 1];
        let errs = s.push(&big);
        assert_eq!(errs, vec![Err(OVERSIZED.to_string())]);
        // 继续丢弃同一行的剩余字节，但不再报错
        assert!(s.push(b"tail-of-the-same-line").is_empty());
        let out = s.push(b"\n{\"type\":\"e\"}\n");
        assert_eq!(out.len(), 1, "新行应正常解析：{out:?}");
        assert_eq!(out[0].as_ref().unwrap(), "{\"type\":\"e\"}");
        assert!(matches!(parse(out[0].as_ref().unwrap()).unwrap(), Incoming::Event { .. }));
    }

    #[test]
    fn 空行跳过() {
        let mut s = LineSplitter::new();
        assert!(s.push(b"\n\r\n\n\n").is_empty());
        let out = s.push(b"\n{\"type\":\"e\"}\n\n");
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn 非法_utf8_不报错() {
        let mut s = LineSplitter::new();
        let out = one(s.push(b"{\"type\":\"e\",\"s\":\"\xff\"}\n"));
        assert!(out.unwrap().contains('\u{fffd}'));
    }

    #[test]
    fn 未知事件类型容忍() {
        match parse(r#"{"type":"session_update","payload":1}"#).unwrap() {
            Incoming::Event { kind, raw } => {
                assert_eq!(kind, "session_update");
                assert_eq!(raw["payload"], json!(1));
            }
            other => panic!("应解析为 Event：{other:?}"),
        }
        match parse(r#"{"no_type":true}"#).unwrap() {
            Incoming::Event { kind, .. } => assert_eq!(kind, "unknown"),
            other => panic!("应解析为 Event：{other:?}"),
        }
    }

    #[test]
    fn 非法_json_与非对象报错() {
        assert!(parse("{not json").is_err());
        let err = parse("[1,2,3]").unwrap_err();
        assert!(err.contains("不是 JSON 对象"), "实际：{err}");
    }

    #[test]
    fn ui_请求解析() {
        match parse(r#"{"type":"extension_ui_request","id":"7","method":"input","title":"标题","placeholder":"提示"}"#).unwrap() {
            Incoming::UiRequest { id, method, title, placeholder, message } => {
                assert_eq!(id, "7");
                assert_eq!(method, "input");
                assert_eq!(title.as_deref(), Some("标题"));
                assert_eq!(placeholder.as_deref(), Some("提示"));
                assert!(message.is_none());
            }
            other => panic!("应解析为 UiRequest：{other:?}"),
        }
        assert!(parse(r#"{"type":"extension_ui_request"}"#).is_err());
    }

    #[test]
    fn 序列化带单个换行() {
        let s = cmd("9", "prompt", json!({"text":"hi"}));
        assert!(s.ends_with('\n') && !s.ends_with("\n\n"));
        let v: Value = serde_json::from_str(s.trim_end()).unwrap();
        assert_eq!(v["type"], json!("prompt"));
        assert_eq!(v["id"], json!("9"));
        assert_eq!(v["text"], json!("hi"));

        // 非对象 extra 直接忽略
        let s = cmd("9", "abort", json!("nope"));
        let v: Value = serde_json::from_str(s.trim_end()).unwrap();
        assert_eq!(v.as_object().unwrap().len(), 2, "实际：{v}");

        let s = ui_value("3", "abc");
        assert!(s.ends_with('\n') && !s.ends_with("\n\n"));
        let v: Value = serde_json::from_str(s.trim_end()).unwrap();
        assert_eq!(v, json!({"type":"extension_ui_response","id":"3","value":"abc"}));

        let s = ui_cancelled("3");
        assert!(s.ends_with('\n') && !s.ends_with("\n\n"));
        let v: Value = serde_json::from_str(s.trim_end()).unwrap();
        assert_eq!(v, json!({"type":"extension_ui_response","id":"3","cancelled":true}));
    }
}
