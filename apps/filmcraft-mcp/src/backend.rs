//! Where commands go. [`Local`] is an in-process engine session (no window). [`Live`] is the running
//! desktop app, reached over its JSON-lines control channel (`filmcraft --control 9876`), which
//! also lets the server press UI-only controls (tools, panels, monitor settings).
//!
//! Every tool is written against [`Backend`] and the JSON the engine commands return, so a tool
//! behaves the same in both.

use std::io::{BufReader, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use filmcraft_engine::Session;
use serde_json::{Value, json};

use crate::rpc;

pub trait Backend {
    /// `"headless"` or `"live"`.
    fn kind(&self) -> &'static str;
    /// Run one command. The error is the engine's message.
    fn exec(&mut self, id: &str, params: Value) -> Result<Value, String>;
    /// Every command with its label, menu, shortcut, parameters and whether it can run now.
    fn commands(&mut self) -> Result<Vec<Value>, String>;
    /// Why `id` can't run right now, when the backend can say.
    fn disabled_reason(&mut self, _id: &str) -> Option<String> {
        None
    }
    /// A control-channel method of the live app (`ui.click`, `ui.elements`…).
    fn ui_call(&mut self, method: &str, _params: Value) -> Result<Value, String> {
        Err(format!(
            "`{method}` needs the live app: start FilmCraft with `filmcraft --control 9876` and this server with `--live 127.0.0.1:9876`. \
             A headless session has no window; every engine command is still available through `edit` and `commands`"
        ))
    }
}

/// An in-process engine session.
pub struct Local {
    session: Box<Session>,
}

impl Local {
    pub fn new(session: Session) -> Self {
        Self { session: Box::new(session) }
    }
}

impl Drop for Local {
    fn drop(&mut self) {
        self.session.shutdown();
    }
}

impl Backend for Local {
    fn kind(&self) -> &'static str {
        "headless"
    }

    fn exec(&mut self, id: &str, params: Value) -> Result<Value, String> {
        self.session.execute(id, params).map_err(|e| e.to_string())
    }

    fn commands(&mut self) -> Result<Vec<Value>, String> {
        match self.session.execute("command.list", json!({})) {
            Ok(Value::Array(a)) => Ok(a),
            Ok(other) => Err(format!("command.list returned {other}")),
            Err(e) => Err(e.to_string()),
        }
    }

    fn disabled_reason(&mut self, id: &str) -> Option<String> {
        let spec = filmcraft_engine::find_command(id)?;
        (spec.enabled)(&self.session).err()
    }
}

/// The desktop app's control channel. One TCP connection, re-opened once when it drops.
pub struct Live {
    addr: String,
    conn: Option<(BufReader<TcpStream>, TcpStream)>,
    next_id: u64,
}

/// The control server answers each request within about a minute; a request that takes longer is
/// polled as a job instead (see [`Live::exec`]).
const REPLY_TIMEOUT: Duration = Duration::from_secs(90);
const JOB_POLL: Duration = Duration::from_millis(250);
/// A blocking export that runs longer than this is reported as an error (the job is stopped).
const JOB_LIMIT: Duration = Duration::from_secs(6 * 3600);

impl Live {
    /// `addr` such as `127.0.0.1:9876`. The control channel has no authentication, so only a
    /// loopback address is accepted.
    pub fn connect(addr: &str) -> Result<Self, String> {
        let host = addr.rsplit_once(':').map_or(addr, |(h, _)| h);
        if !matches!(host, "127.0.0.1" | "localhost" | "[::1]" | "::1") {
            return Err(format!("--live address must be loopback (127.0.0.1:PORT), got `{addr}`"));
        }
        let mut live = Self { addr: addr.to_string(), conn: None, next_id: 1 };
        live.open().map_err(|e| format!("can't reach the app at {addr}: {e} (start it with `filmcraft --control PORT`)"))?;
        Ok(live)
    }

    fn open(&mut self) -> std::io::Result<()> {
        let stream = TcpStream::connect(&self.addr)?;
        stream.set_read_timeout(Some(REPLY_TIMEOUT))?;
        stream.set_write_timeout(Some(Duration::from_secs(30)))?;
        stream.set_nodelay(true)?;
        let reader = BufReader::new(stream.try_clone()?);
        self.conn = Some((reader, stream));
        Ok(())
    }

    fn round_trip(&mut self, method: &str, params: &Value) -> Result<Value, String> {
        if self.conn.is_none() {
            self.open().map_err(|e| format!("the app is not reachable at {}: {e}", self.addr))?;
        }
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        let line = json!({"id": id, "method": method, "params": params}).to_string();
        let (reader, writer) = self.conn.as_mut().ok_or("no connection")?;
        let io = (|| -> std::io::Result<String> {
            writer.write_all(line.as_bytes())?;
            writer.write_all(b"\n")?;
            writer.flush()?;
            let buf = match rpc::read_line(reader, rpc::MAX_LINE)? {
                rpc::Line::Text(t) => t,
                rpc::Line::TooLong => return Err(std::io::Error::other("reply too long")),
                rpc::Line::Eof => return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "the app closed the connection")),
            };
            String::from_utf8(buf).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
        })();
        let text = match io {
            Ok(t) => t,
            Err(e) => {
                // a broken connection can't be trusted for the next request either
                self.conn = None;
                return Err(format!("control channel: {e}"));
            }
        };
        let v: Value = serde_json::from_str(&text).map_err(|e| format!("control channel sent invalid JSON: {e}"))?;
        if v.get("ok").and_then(Value::as_bool) == Some(true) {
            Ok(v.get("result").cloned().unwrap_or(Value::Null))
        } else {
            Err(v.get("error").and_then(Value::as_str).unwrap_or("the app reported an error").to_string())
        }
    }

    /// `round_trip` that reconnects once when the connection had gone away.
    fn call(&mut self, method: &str, params: &Value) -> Result<Value, String> {
        match self.round_trip(method, params) {
            Err(e) if self.conn.is_none() && e.starts_with("control channel:") => self.round_trip(method, params),
            r => r,
        }
    }

    /// A blocking export is an app job here: the control channel answers within a minute and the
    /// app keeps repainting while the job runs. Polls `jobs.list` until the job is done.
    fn exec_blocking_export(&mut self, id: &str, mut params: Value) -> Result<Value, String> {
        if let Some(o) = params.as_object_mut() {
            o.insert("wait".into(), json!(false));
        }
        let start = self.call("engine.execute", &json!({"command": id, "params": params}))?;
        let Some(job) = start.get("job").and_then(Value::as_i64) else { return Ok(start) };
        let t0 = Instant::now();
        loop {
            std::thread::sleep(JOB_POLL);
            let jobs = self.call("engine.execute", &json!({"command": "jobs.list", "params": {}}))?;
            let this = jobs.as_array().into_iter().flatten().find(|j| j.get("id").and_then(Value::as_i64) == Some(job));
            match this {
                None => return Ok(start),
                Some(j) if j.get("finished").and_then(Value::as_bool) == Some(true) => {
                    if let Some(err) = j.get("error").and_then(Value::as_str).filter(|e| !e.is_empty()) {
                        return Err(format!("export failed: {err}"));
                    }
                    return Ok(json!({"job": job, "path": start.get("path"), "result": j}));
                }
                Some(_) if t0.elapsed() > JOB_LIMIT => {
                    let _ = self.call("engine.execute", &json!({"command": "jobs.cancel", "params": {"job": job}}));
                    return Err("export stopped: it ran longer than six hours".into());
                }
                Some(_) => {}
            }
        }
    }
}

impl Backend for Live {
    fn kind(&self) -> &'static str {
        "live"
    }

    fn exec(&mut self, id: &str, params: Value) -> Result<Value, String> {
        if id == "file.exportMedia" && params.get("wait").and_then(Value::as_bool) == Some(true) {
            return self.exec_blocking_export(id, params);
        }
        self.call("engine.execute", &json!({"command": id, "params": params}))
    }

    fn commands(&mut self) -> Result<Vec<Value>, String> {
        match self.call("engine.commands", &json!({}))? {
            Value::Array(a) => Ok(a),
            other => Err(format!("engine.commands returned {other}")),
        }
    }

    fn ui_call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        if !method.starts_with("ui.") && !matches!(method, "perf.stats" | "engine.commands") {
            return Err(format!("`{method}` is not a UI control method (ui.*)"));
        }
        self.call(method, &params)
    }
}

/// A scripted backend for unit tests: replies are looked up by command id.
#[cfg(test)]
pub struct Fake {
    pub replies: std::collections::HashMap<String, Result<Value, String>>,
    pub calls: Vec<(String, Value)>,
}

#[cfg(test)]
impl Backend for Fake {
    fn kind(&self) -> &'static str {
        "headless"
    }
    fn exec(&mut self, id: &str, params: Value) -> Result<Value, String> {
        self.calls.push((id.to_string(), params));
        self.replies.get(id).cloned().unwrap_or_else(|| Err(format!("unscripted {id}")))
    }
    fn commands(&mut self) -> Result<Vec<Value>, String> {
        Ok(Vec::new())
    }
}
