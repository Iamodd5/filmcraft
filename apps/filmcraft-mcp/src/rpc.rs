//! JSON-RPC 2.0 over newline-delimited stdio, the transport MCP uses.
//!
//! Everything read from stdin is hostile: bytes that are not UTF-8, lines that are not JSON, batches
//! (MCP dropped them), requests without a method and lines of unbounded length all get an error
//! reply and the server keeps serving.

use std::io::{self, BufRead};

use serde_json::{Value, json};

pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;

/// Longest accepted input line. A project with a long edit list is far below this.
pub const MAX_LINE: usize = 16 * 1024 * 1024;

/// One decoded input line.
#[derive(Debug, PartialEq)]
pub enum Incoming {
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    Notification {
        method: String,
    },
    /// Not usable: answer with this error (the id is `null` when it could not be read).
    Invalid {
        id: Value,
        code: i64,
        message: String,
    },
}

/// What one read of stdin produced.
#[derive(Debug, PartialEq)]
pub enum Line {
    Text(Vec<u8>),
    TooLong,
    Eof,
}

/// Read one `\n`-terminated line of at most `cap` bytes. A longer line is skipped to its end and
/// reported as [`Line::TooLong`], so one bad line can't exhaust memory or end the session.
pub fn read_line(r: &mut impl BufRead, cap: usize) -> io::Result<Line> {
    let mut out: Vec<u8> = Vec::new();
    let mut too_long = false;
    let mut any = false;
    loop {
        let buf = match r.fill_buf() {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if buf.is_empty() {
            return Ok(if !any {
                Line::Eof
            } else if too_long {
                Line::TooLong
            } else {
                Line::Text(out)
            });
        }
        any = true;
        let (chunk, found) = match buf.iter().position(|&b| b == b'\n') {
            Some(i) => (buf.get(..i).unwrap_or_default(), Some(i)),
            None => (buf, None),
        };
        if !too_long {
            if out.len().saturating_add(chunk.len()) > cap {
                too_long = true;
                out = Vec::new();
            } else {
                out.extend_from_slice(chunk);
            }
        }
        let used = found.map_or(chunk.len(), |i| i + 1);
        r.consume(used);
        if found.is_some() {
            return Ok(if too_long { Line::TooLong } else { Line::Text(out) });
        }
    }
}

pub fn decode(bytes: &[u8]) -> Incoming {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return invalid(PARSE_ERROR, "input is not valid UTF-8");
    };
    let v: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => return invalid(PARSE_ERROR, format!("parse error: {e}")),
    };
    let Some(obj) = v.as_object() else {
        let what = if v.is_array() { "batches are not supported: send one request per line" } else { "a request must be a JSON object" };
        return invalid(INVALID_REQUEST, what);
    };
    let id = obj.get("id").cloned();
    let reply_id = id.clone().filter(|i| i.is_string() || i.is_number()).unwrap_or(Value::Null);
    let Some(method) = obj.get("method").and_then(Value::as_str) else {
        return Incoming::Invalid { id: reply_id, code: INVALID_REQUEST, message: "a request needs a string `method`".into() };
    };
    let params = obj.get("params").cloned().unwrap_or(Value::Null);
    match id {
        None => Incoming::Notification { method: method.to_string() },
        Some(i) if i.is_string() || i.is_number() => Incoming::Request { id: i, method: method.to_string(), params },
        Some(_) => Incoming::Invalid { id: Value::Null, code: INVALID_REQUEST, message: "`id` must be a string or a number".into() },
    }
}

fn invalid(code: i64, message: impl Into<String>) -> Incoming {
    Incoming::Invalid { id: Value::Null, code, message: message.into() }
}

pub fn ok(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

pub fn error(id: &Value, code: i64, message: impl Into<String>) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message.into()}})
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn decodes_requests_notifications_and_garbage() {
        assert_eq!(decode(br#"{"jsonrpc":"2.0","id":3,"method":"ping"}"#), Incoming::Request { id: json!(3), method: "ping".into(), params: Value::Null });
        assert_eq!(decode(br#"{"method":"notifications/initialized"}"#), Incoming::Notification { method: "notifications/initialized".into() });
        for bad in [&b"not json"[..], b"[1,2]", b"42", b"\xff\xfe", b"{}", br#"{"id":true,"method":"x"}"#, br#"{"id":{},"method":"x"}"#] {
            assert!(matches!(decode(bad), Incoming::Invalid { .. }), "{bad:?}");
        }
    }

    #[test]
    fn invalid_request_keeps_a_readable_id() {
        let Incoming::Invalid { id, .. } = decode(br#"{"id":7}"#) else { panic!("not invalid") };
        assert_eq!(id, json!(7));
    }

    #[test]
    fn reads_capped_lines_and_recovers_after_a_long_one() {
        let mut r = Cursor::new(b"abc\n0123456789\nxy".to_vec());
        assert_eq!(read_line(&mut r, 5).ok(), Some(Line::Text(b"abc".to_vec())));
        assert_eq!(read_line(&mut r, 5).ok(), Some(Line::TooLong));
        assert_eq!(read_line(&mut r, 5).ok(), Some(Line::Text(b"xy".to_vec())));
        assert_eq!(read_line(&mut r, 5).ok(), Some(Line::Eof));
    }
}
