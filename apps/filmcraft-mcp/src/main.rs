//! `filmcraft-mcp`: an MCP server that lets an agent do all of the editing in FilmCraft, and
//! leaves a person only the timeline to review.
//!
//! * Every engine command (every menu item and panel control) is reachable: `commands`,
//!   `command_describe`, `edit`.
//! * Edits are all-or-nothing and verified: see `tools.rs`.
//! * With `--live`, the running desktop app's UI controls (tools, panels, monitor settings) are
//!   reachable too.
//!
//! stdout carries only protocol messages; diagnostics go to stderr.

#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable))]

mod b64;
mod backend;
mod check;
mod review;
mod rpc;
mod schema;
mod server;
mod tools;

use std::io::{BufReader, Write};

use backend::{Backend, Live, Local};
use filmcraft_engine::Session;
use serde_json::{Value, json};

const HELP: &str = "\
filmcraft-mcp: MCP server (stdio) for editing in FilmCraft

USAGE
  filmcraft-mcp [--demo | --project <p.fcproj>] [--data-dir <dir>]    headless engine session
  filmcraft-mcp --live 127.0.0.1:9876                                 the running app (`filmcraft --control 9876`)
  filmcraft-mcp --version | --help

Register it with an MCP client, for example:
  claude mcp add filmcraft-agent -- /abs/path/to/filmcraft-mcp --demo
";

fn diag(msg: impl std::fmt::Display) {
    let _ = writeln!(std::io::stderr(), "filmcraft-mcp: {msg}");
}

fn opt<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).map(String::as_str)
}

fn open_backend(args: &[String]) -> Result<Box<dyn Backend>, String> {
    if let Some(addr) = opt(args, "--live") {
        if args.iter().any(|a| a == "--demo" || a == "--project") {
            return Err("--live drives the running app; open projects there (the `project` tool)".into());
        }
        return Ok(Box::new(Live::connect(addr)?));
    }
    // OS hardware decoders in front of ours, as in the desktop app
    filmcraft_platform::register();
    let mut s = Session::default();
    if let Some(dir) = opt(args, "--data-dir").map(std::path::PathBuf::from).or_else(filmcraft_engine::autosave::default_data_dir) {
        s.export_presets.set_dir(&dir);
    }
    if let Some(p) = opt(args, "--project") {
        s.execute("file.open", json!({"path": p})).map_err(|e| format!("can't open {p}: {e}"))?;
    } else if args.iter().any(|a| a == "--demo") {
        s.execute("file.openDemoProject", json!({})).map_err(|e| e.to_string())?;
    }
    Ok(Box::new(Local::new(s)))
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        let _ = writeln!(std::io::stdout(), "filmcraft-mcp {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    if args.iter().any(|a| a == "--help" || a == "-h") {
        let _ = write!(std::io::stdout(), "{HELP}");
        return;
    }
    let backend = match open_backend(&args) {
        Ok(b) => b,
        Err(e) => {
            diag(e);
            std::process::exit(1);
        }
    };
    diag(format_args!("ready ({} session)", backend.kind()));
    let mut mcp = server::Mcp::new(tools::Server::new(backend));
    let mut input = BufReader::new(std::io::stdin().lock());
    let mut stdout = std::io::stdout().lock();
    loop {
        let incoming = match rpc::read_line(&mut input, rpc::MAX_LINE) {
            Ok(rpc::Line::Eof) => break,
            Ok(rpc::Line::Text(bytes)) if bytes.iter().all(u8::is_ascii_whitespace) => continue,
            Ok(rpc::Line::Text(bytes)) => rpc::decode(&bytes),
            Ok(rpc::Line::TooLong) => {
                rpc::Incoming::Invalid { id: Value::Null, code: rpc::INVALID_REQUEST, message: format!("input line longer than {} bytes", rpc::MAX_LINE) }
            }
            Err(e) => {
                diag(format_args!("can't read stdin: {e}"));
                break;
            }
        };
        if let Some(reply) = mcp.handle(incoming) {
            // the client went away: nothing left to serve
            if writeln!(stdout, "{reply}").and_then(|()| stdout.flush()).is_err() {
                break;
            }
        }
    }
}
