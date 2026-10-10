//! End-to-end tests: the real `filmcraft-mcp` binary, spoken to over stdio as an MCP client would,
//! against the demo project. Covers the protocol, hostile input, every tool and the all-or-nothing
//! edit guarantees (a failed edit leaves the timeline exactly as it was).

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

const TICKS: i64 = 254_016_000_000;

struct Client {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next: i64,
}

impl Client {
    fn start(args: &[&str]) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_filmcraft-mcp"))
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn filmcraft-mcp");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));
        let mut c = Client { child, stdin, stdout, next: 0 };
        let r = c.rpc("initialize", json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}}));
        assert_eq!(r["result"]["protocolVersion"], "2025-06-18", "{r}");
        c
    }

    fn demo() -> Self {
        Self::start(&["--demo"])
    }

    fn raw(&mut self, line: &str) -> Value {
        writeln!(self.stdin, "{line}").expect("write");
        self.stdin.flush().expect("flush");
        let mut reply = String::new();
        self.stdout.read_line(&mut reply).expect("read");
        serde_json::from_str(&reply).unwrap_or_else(|e| panic!("reply is not JSON ({e}): {reply:?}"))
    }

    fn rpc(&mut self, method: &str, params: Value) -> Value {
        self.next += 1;
        let line = json!({"jsonrpc": "2.0", "id": self.next, "method": method, "params": params}).to_string();
        self.raw(&line)
    }

    /// A tool call: (structured result, the whole MCP result).
    fn tool(&mut self, name: &str, args: Value) -> (Value, Value) {
        let r = self.rpc("tools/call", json!({"name": name, "arguments": args}));
        let res = r["result"].clone();
        assert!(res.is_object(), "{name} {args}: {r}");
        (res["structuredContent"].clone(), res)
    }

    fn ok(&mut self, name: &str, args: Value) -> Value {
        let (d, raw) = self.tool(name, args.clone());
        assert_eq!(raw["isError"], json!(false), "{name} {args}: {d}");
        d
    }

    fn review(&mut self) -> Value {
        self.ok("review", json!({}))
    }

    fn sequence(&mut self) -> Value {
        self.ok("inspect", json!({"scope": "sequence"}))["sequence"].clone()
    }

    fn clip_count(&mut self) -> usize {
        self.review()["clips"].as_array().map_or(0, Vec::len)
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn temp(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("fcmcp-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).expect("temp dir");
    d
}

fn razor(track: &str, seconds: i64) -> Value {
    json!({"id": "timeline.razor", "params": {"time": seconds * TICKS, "track": track}})
}

#[test]
fn handshake_lists_the_tools_with_schemas_and_annotations() {
    let mut c = Client::demo();
    let r = c.rpc("tools/list", json!({}));
    let tools = r["result"]["tools"].as_array().expect("tools");
    let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    for want in ["commands", "command_describe", "query", "inspect", "review", "edit", "frames", "checkpoint", "project", "import", "export", "ui"] {
        assert!(names.contains(&want), "missing tool {want}: {names:?}");
    }
    for t in tools {
        assert_eq!(t["inputSchema"]["additionalProperties"], json!(false), "{t}");
        assert!(t["annotations"]["readOnlyHint"].is_boolean(), "{t}");
    }
    assert!(c.rpc("ping", json!({}))["result"].is_object());
    assert!(c.rpc("resources/list", json!({}))["result"]["resources"].as_array().is_some_and(|r| r.len() == 3));
    assert!(c.rpc("prompts/get", json!({"name": "edit_protocol"}))["result"]["messages"][0]["content"]["text"].as_str().is_some_and(|t| t.contains("expect")));
    let init = c.rpc("initialize", json!({"protocolVersion": "1999-01-01"}));
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18", "an unknown revision is answered with one we speak");
}

#[test]
fn newest_revision_gets_result_type_and_cache_hints_only_when_asked() {
    let mut c = Client::demo();
    let meta = json!({"io.modelcontextprotocol/protocolVersion": "2026-07-28"});
    let d = c.rpc("server/discover", json!({"_meta": meta}));
    assert!(d["result"]["supportedVersions"].as_array().is_some_and(|v| v.contains(&json!("2026-07-28"))), "{d}");
    for (m, p) in [
        ("tools/list", json!({"_meta": meta})),
        ("resources/list", json!({"_meta": meta})),
        ("resources/read", json!({"_meta": meta, "uri": "filmcraft://protocol"})),
    ] {
        let r = c.rpc(m, p);
        assert_eq!(r["result"]["resultType"], "complete", "{m}: {r}");
        assert!(r["result"]["ttlMs"].is_u64() && r["result"]["cacheScope"] == "private", "{m}: {r}");
    }
    let r = c.rpc("tools/list", json!({}));
    assert!(r["result"].get("resultType").is_none() && r["result"].get("ttlMs").is_none(), "{r}");
}

#[test]
fn hostile_input_is_answered_with_errors_and_the_server_keeps_serving() {
    let mut c = Client::demo();
    assert_eq!(c.raw("this is not json")["error"]["code"], -32700);
    assert_eq!(c.raw("[1,2,3]")["error"]["code"], -32600, "batches are rejected");
    assert_eq!(c.raw("42")["error"]["code"], -32600);
    assert_eq!(c.raw(r#"{"jsonrpc":"2.0","id":9}"#)["error"]["code"], -32600);
    assert_eq!(c.raw(r#"{"jsonrpc":"2.0","id":{"x":1},"method":"ping"}"#)["error"]["code"], -32600);
    assert_eq!(c.rpc("no/such/method", json!({}))["error"]["code"], -32601);
    assert_eq!(c.rpc("tools/call", json!({"name": "nope"}))["error"]["code"], -32602);
    assert_eq!(c.rpc("tools/call", json!({}))["error"]["code"], -32602);
    assert_eq!(c.rpc("tools/call", json!({"name": "review", "arguments": 5}))["error"]["code"], -32602);
    assert_eq!(c.rpc("resources/read", json!({"uri": "filmcraft://nope"}))["error"]["code"], -32002);
    // a notification has no reply: the next request is the one that answers
    writeln!(c.stdin, r#"{{"jsonrpc":"2.0","method":"notifications/initialized"}}"#).expect("write");
    let long = format!(r#"{{"jsonrpc":"2.0","id":1,"method":"ping","pad":"{}"}}"#, "x".repeat(200_000));
    assert!(c.raw(&long)["result"].is_object(), "a long but legal line is fine");
    assert_eq!(c.review()["verdict"], "clean", "still serving");
}

#[test]
fn a_misspelled_or_mistyped_argument_is_an_error_that_names_it() {
    let mut c = Client::demo();
    let r = c.rpc("tools/call", json!({"name": "commands", "arguments": {"qeury": "razor"}}));
    let msg = r["error"]["message"].as_str().unwrap_or_default();
    assert_eq!(r["error"]["code"], -32602);
    assert!(msg.contains("unknown argument \"qeury\"") && msg.contains("query"), "{msg}");
    let r = c.rpc("tools/call", json!({"name": "edit", "arguments": {"steps": [{"id": "x", "parms": {}}]}}));
    assert!(r["error"]["message"].as_str().unwrap_or_default().contains("unknown argument \"parms\""), "{r}");
    let r = c.rpc("tools/call", json!({"name": "frames", "arguments": {"count": 99}}));
    assert!(r["error"]["message"].as_str().unwrap_or_default().contains("at most"), "{r}");
    let r = c.rpc("tools/call", json!({"name": "checkpoint", "arguments": {"action": "explode"}}));
    assert!(r["error"]["message"].as_str().unwrap_or_default().contains("must be one of"), "{r}");
}

#[test]
fn every_command_is_discoverable_and_describable() {
    let mut c = Client::demo();
    let groups = c.ok("commands", json!({}));
    assert!(groups["total"].as_u64().unwrap_or(0) >= 600, "{groups}");
    let razor = c.ok("commands", json!({"query": "razor"}));
    assert!(razor["commands"].as_array().is_some_and(|l| l.iter().any(|x| x["id"] == "timeline.razor")), "{razor}");
    let d = c.ok("command_describe", json!({"id": "timeline.razor"}));
    assert!(d["params"].as_str().is_some_and(|p| p.contains("time")), "{d}");
    let (d, raw) = c.tool("command_describe", json!({"id": "timeline.razr"}));
    assert_eq!(raw["isError"], json!(true));
    assert!(d["similar"].as_array().is_some_and(|s| s.contains(&json!("timeline.razor"))), "{d}");
    let (d, _) = c.tool("command_describe", json!({"id": "edit.undo"}));
    assert_eq!(d["enabled"], json!(false));
    assert!(d["disabledReason"].as_str().is_some_and(|w| w.contains("nothing to undo")), "{d}");
    // the resource carries the same catalog
    let r = c.rpc("resources/read", json!({"uri": "filmcraft://commands"}));
    let text = r["result"]["contents"][0]["text"].as_str().unwrap_or_default();
    assert!(text.contains("timeline.razor"), "{}", &text[..text.len().min(200)]);
}

#[test]
fn the_demo_timeline_reviews_clean_and_reads_in_frames() {
    let mut c = Client::demo();
    let (r, raw) = c.tool("review", json!({}));
    assert_eq!(r["verdict"], "clean", "{}", r["issues"]);
    assert_eq!(r["sequence"]["durationFrames"], 642);
    assert_eq!(r["clips"].as_array().map(Vec::len), Some(14));
    assert!(r["map"].as_str().is_some_and(|m| m.contains("Video 1 |")), "{r}");
    let first = raw["content"][0]["text"].as_str().unwrap_or_default();
    assert!(first.contains("verdict clean") && first.contains("Audio 2"), "{first}");
}

#[test]
fn an_edit_that_holds_is_kept_and_reports_exactly_what_changed() {
    let mut c = Client::demo();
    // cutting picture and its linked sound together keeps them in sync
    let d = c.ok("edit", json!({"steps": [razor("V1", 1), razor("A1", 1)], "expect": {"noWarnings": true, "maxAddedClips": 2, "clipsPresent": [29, 30]}}));
    assert_eq!((d["ok"].clone(), d["committed"].clone(), d["rolledBack"].clone()), (json!(true), json!(true), json!(false)), "{d}");
    assert_eq!(d["changes"]["added"].as_array().map(Vec::len), Some(2), "{d}");
    assert_eq!(d["changes"]["changed"].as_array().map(Vec::len), Some(2), "{d}");
    assert_eq!(d["changes"]["clipsAfter"], 16);
    assert_eq!(d["changes"]["durationFramesBefore"], d["changes"]["durationFramesAfter"]);
    assert_eq!(c.clip_count(), 16);
    // and it is one undoable step per command, so the history shows it
    let h = c.ok("inspect", json!({"scope": "history"}));
    assert!(h["history"]["undo"].as_array().is_some_and(|u| !u.is_empty()), "{h}");
}

#[test]
fn cutting_only_the_picture_of_a_linked_clip_is_caught_as_out_of_sync() {
    let mut c = Client::demo();
    let (d, raw) = c.tool("edit", json!({"steps": [razor("V1", 1)], "expect": {"noWarnings": true}}));
    assert_eq!(raw["isError"], json!(true), "{d}");
    assert_eq!(d["rolledBack"], json!(true));
    assert!(d["reasons"][0].as_str().is_some_and(|r| r.contains("expected no warnings")), "{d}");
    assert!(d["newIssues"].as_array().is_some_and(|i| i.iter().any(|x| x["code"] == "av_out_of_sync")), "{d}");
    assert_eq!(c.clip_count(), 14, "nothing was kept");
    assert!(d.get("rollbackWarning").is_none(), "the rollback was verified: {d}");
}

#[test]
fn a_false_expectation_undoes_every_step_and_the_timeline_is_proven_unchanged() {
    let mut c = Client::demo();
    let before = c.sequence();
    let (d, raw) = c.tool("edit", json!({"steps": [razor("V1", 2), razor("A1", 2)], "expect": {"durationFrames": 1}}));
    assert_eq!(raw["isError"], json!(true));
    assert_eq!((d["ok"].clone(), d["committed"].clone(), d["rolledBack"].clone()), (json!(false), json!(false), json!(true)), "{d}");
    assert!(d["reasons"][0].as_str().is_some_and(|r| r.contains("642")), "{d}");
    assert!(d.get("rollbackWarning").is_none(), "{d}");
    let after = c.sequence();
    let strip = |mut s: Value| {
        s.as_object_mut().map(|o| o.remove("playhead"));
        s
    };
    assert_eq!(strip(before), strip(after), "byte-for-byte what it was");
}

#[test]
fn a_failing_step_undoes_the_steps_before_it() {
    let mut c = Client::demo();
    let (d, raw) = c.tool("edit", json!({"steps": [razor("V1", 3), {"id": "no.such.command"}]}));
    assert_eq!(raw["isError"], json!(true));
    assert_eq!(d["rolledBack"], json!(true));
    assert!(d["reasons"][0].as_str().is_some_and(|r| r.contains("step 1") && r.contains("no.such.command")), "{d}");
    assert_eq!(c.clip_count(), 14);
    // with atomic false the first step is kept and the failure is reported
    let (d, _) = c.tool("edit", json!({"steps": [razor("V1", 3), {"id": "no.such.command"}], "atomic": false}));
    assert_eq!((d["rolledBack"].clone(), d["ok"].clone()), (json!(false), json!(false)), "{d}");
    assert_eq!(c.clip_count(), 15);
}

#[test]
fn a_dry_run_shows_the_change_and_keeps_nothing() {
    let mut c = Client::demo();
    let d = c.ok("edit", json!({"steps": [razor("V1", 3), razor("A1", 3)], "dry_run": true}));
    assert_eq!((d["committed"].clone(), d["rolledBack"].clone(), d["dryRun"].clone()), (json!(false), json!(true), json!(true)), "{d}");
    assert_eq!(d["changes"]["added"].as_array().map(Vec::len), Some(2));
    assert_eq!(c.clip_count(), 14);
}

#[test]
fn edit_refuses_commands_that_replace_or_write_the_project() {
    let mut c = Client::demo();
    for id in ["file.openDemoProject", "file.open", "file.save", "file.exportMedia", "file.newProject"] {
        let (d, raw) = c.tool("edit", json!({"steps": [{"id": id}]}));
        assert_eq!(raw["isError"], json!(true), "{id}");
        assert!(d["error"].as_str().is_some_and(|e| e.contains("not allowed in `edit`")), "{id}: {d}");
    }
    assert_eq!(c.clip_count(), 14);
}

#[test]
fn the_second_half_can_be_cut_as_one_verified_edit() {
    let mut c = Client::demo();
    let mid = c.sequence()["duration"].as_i64().expect("duration") / 2;
    let d = c.ok("edit", json!({"steps": [{"id": "timeline.razor", "params": {"time": mid, "track": "A2"}}]}));
    assert_eq!(d["ok"], json!(true), "{d}");
    let seq = c.sequence();
    let ids: Vec<i64> = ["video", "audio"]
        .iter()
        .flat_map(|k| seq[k].as_array().cloned().unwrap_or_default())
        .flat_map(|t| t["items"].as_array().cloned().unwrap_or_default())
        .filter(|i| i["start"].as_i64().is_some_and(|s| s >= mid))
        .filter_map(|i| i["clip"].as_i64())
        .collect();
    assert_eq!(ids.len(), 7, "{ids:?}");
    let d = c.ok("edit", json!({"steps": [{"id": "edit.rippleDelete", "params": {"clips": ids}}], "expect": {"durationFrames": 321, "noGaps": true, "clipsAbsent": [38, 41, 44]}}));
    assert_eq!((d["committed"].clone(), d["verdictAfter"].clone()), (json!(true), json!("clean")), "{d}");
    assert_eq!((d["changes"]["clipsBefore"].clone(), d["changes"]["clipsAfter"].clone()), (json!(15), json!(8)));
    assert_eq!(c.review()["sequence"]["durationFrames"], 321);
}

#[test]
fn checkpoints_return_to_a_mark_and_say_so_when_they_cannot() {
    let mut c = Client::demo();
    c.ok("checkpoint", json!({"action": "create", "name": "start"}));
    c.ok("edit", json!({"steps": [razor("V1", 1), razor("A1", 1)]}));
    c.ok("edit", json!({"steps": [razor("V1", 2), razor("A1", 2)]}));
    assert_eq!(c.clip_count(), 18);
    let d = c.ok("checkpoint", json!({"action": "restore", "name": "start"}));
    assert_eq!(d["stepsUndone"], 4, "{d}");
    assert_eq!(c.clip_count(), 14);
    assert!(c.ok("checkpoint", json!({"action": "list"}))["checkpoints"].as_array().is_some_and(|l| l.len() == 1));
    let (_, raw) = c.tool("checkpoint", json!({"action": "restore", "name": "nope"}));
    assert_eq!(raw["isError"], json!(true));
    c.ok("checkpoint", json!({"action": "drop", "name": "start"}));
    assert!(c.ok("checkpoint", json!({"action": "list"}))["checkpoints"].as_array().is_some_and(Vec::is_empty));

    // Mark after two steps, go on, undo past the mark and edit differently: the history no longer
    // contains the marked state. Restoring must refuse and leave the timeline as it is.
    c.ok("edit", json!({"steps": [razor("V1", 1), razor("A1", 1)]}));
    c.ok("checkpoint", json!({"action": "create", "name": "m"}));
    c.ok("edit", json!({"steps": [razor("V1", 2), razor("A1", 2)]}));
    c.ok("edit", json!({"steps": [{"id": "edit.undo"}, {"id": "edit.undo"}, {"id": "edit.undo"}]}));
    c.ok("edit", json!({"steps": [razor("V1", 3), razor("A1", 3)]}));
    let before = c.clip_count();
    let (d, raw) = c.tool("checkpoint", json!({"action": "restore", "name": "m"}));
    assert_eq!(raw["isError"], json!(true), "{d}");
    assert!(d["error"].as_str().is_some_and(|e| e.contains("can't be restored") && e.contains("Nothing was changed")), "{d}");
    assert_eq!(c.clip_count(), before, "a refused restore changes nothing");
}

#[test]
fn a_query_that_turns_out_to_change_the_project_is_undone() {
    let mut c = Client::demo();
    let ok = c.ok("query", json!({"id": "history.list"}));
    assert!(ok["result"]["undo"].is_array());
    let (d, raw) = c.tool("query", json!({"id": "timeline.razor", "params": {"time": TICKS, "track": "V1"}}));
    assert_eq!(raw["isError"], json!(true), "{d}");
    assert!(d["error"].as_str().is_some_and(|e| e.contains("not a query")), "{d}");
    assert_eq!(c.clip_count(), 14);
    let (_, raw) = c.tool("query", json!({"id": "file.save"}));
    assert_eq!(raw["isError"], json!(true));
}

#[test]
fn frames_come_back_as_inline_png_images_and_files() {
    let mut c = Client::demo();
    let (d, raw) = c.tool("frames", json!({"seconds": [0.0, 10.0, 1.0e6], "inline": true}));
    assert_eq!(raw["isError"], json!(true), "a time past the end is refused, not clamped: {d}");
    assert!(d["error"].as_str().is_some_and(|e| e.contains("past the end")), "{d}");
    let (d, raw) = c.tool("frames", json!({"count": 3}));
    assert_eq!(raw["isError"], json!(false), "{d}");
    let images: Vec<&Value> = raw["content"].as_array().expect("content").iter().filter(|b| b["type"] == "image").collect();
    assert_eq!(images.len(), 3);
    assert!(images.iter().all(|i| i["mimeType"] == "image/png" && i["data"].as_str().is_some_and(|s| s.starts_with("iVBOR"))), "PNG signature in base64");
    for f in d["frames"].as_array().expect("frames") {
        let p = f["path"].as_str().expect("path");
        assert!(std::fs::metadata(p).is_ok_and(|m| m.len() > 1000), "{p}");
        assert_eq!(f["width"], 1920);
    }
    let (d, _) = c.tool("frames", json!({"seconds": [4.0], "inline": false}));
    assert_eq!(d["inlineImages"], 0);
}

#[test]
fn projects_save_reopen_and_unsaved_work_is_protected() {
    let dir = temp("project");
    let path = dir.join("a.fcproj").to_string_lossy().to_string();
    let mut c = Client::demo();
    c.ok("edit", json!({"steps": [razor("V1", 1), razor("A1", 1)]}));
    let (d, raw) = c.tool("project", json!({"action": "open", "path": dir.join("none.fcproj")}));
    assert_eq!(raw["isError"], json!(true));
    assert!(d["error"].as_str().is_some_and(|e| e.contains("no project file")), "missing files are reported first: {d}");
    let (d, raw) = c.tool("project", json!({"action": "demo"}));
    assert_eq!(raw["isError"], json!(true));
    assert!(d["error"].as_str().is_some_and(|e| e.contains("unsaved changes")), "{d}");
    let (d, _) = c.tool("project", json!({"action": "save_as", "path": path}));
    assert_eq!(d["saved"], json!(true), "{d}");
    assert!(std::fs::metadata(&path).is_ok_and(|m| m.len() > 0));
    let (d, raw) = c.tool("project", json!({"action": "save_as", "path": dir.join("no/such/dir/x.fcproj")}));
    assert_eq!(raw["isError"], json!(true), "{d}");
    // opening the demo again must not leave the saved project's path behind (Save would overwrite it)
    let d = c.ok("project", json!({"action": "demo"}));
    assert_eq!(d["project"]["path"], Value::Null, "{d}");
    let saved = std::fs::read(&path).expect("saved project");
    let (_, raw) = c.tool("project", json!({"action": "save"}));
    assert_eq!(raw["isError"], json!(true), "a save with nowhere to write fails");
    assert_eq!(std::fs::read(&path).expect("still there"), saved);
    c.ok("project", json!({"action": "open", "path": path}));
    assert_eq!(c.clip_count(), 16, "the edit survived the round trip");
}

#[test]
fn export_writes_a_file_and_checks_it_and_refuses_bad_destinations() {
    let dir = temp("export");
    let out = dir.join("mix.wav").to_string_lossy().to_string();
    let mut c = Client::demo();
    let d = c.ok("export", json!({"path": out, "format": "wav"}));
    assert_eq!(d["exported"], json!(true), "{d}");
    assert!(std::fs::metadata(&out).is_ok_and(|m| m.len() > 10_000));
    let (d, raw) = c.tool("export", json!({"path": dir.join("no/such/folder/x.wav"), "format": "wav"}));
    assert_eq!(raw["isError"], json!(true));
    assert!(d["error"].as_str().is_some_and(|e| e.contains("does not exist")), "{d}");
}

#[test]
fn import_checks_files_before_touching_the_project() {
    let dir = temp("import");
    let mut c = Client::demo();
    let (d, raw) = c.tool("import", json!({"paths": [dir.join("missing.mov")]}));
    assert_eq!(raw["isError"], json!(true));
    assert!(d["error"].as_str().is_some_and(|e| e.contains("can't read")), "{d}");
    std::fs::write(dir.join("empty.wav"), b"").expect("write");
    let (_, raw) = c.tool("import", json!({"paths": [dir.join("empty.wav")]}));
    assert_eq!(raw["isError"], json!(true));
}

#[test]
fn ui_tool_explains_that_it_needs_the_live_app() {
    let mut c = Client::demo();
    let (d, raw) = c.tool("ui", json!({"action": "elements"}));
    assert_eq!(raw["isError"], json!(true));
    assert!(d["error"].as_str().is_some_and(|e| e.contains("--live")), "{d}");
}

#[test]
fn an_empty_session_says_what_to_do_instead_of_failing_obscurely() {
    let mut c = Client::start(&[]);
    let (d, raw) = c.tool("review", json!({}));
    assert_eq!(raw["isError"], json!(true));
    assert!(d["error"].as_str().is_some_and(|e| e.contains("no active sequence")), "{d}");
    let (_, raw) = c.tool("edit", json!({"steps": [razor("V1", 1)]}));
    assert_eq!(raw["isError"], json!(true));
    c.ok("project", json!({"action": "new", "name": "Fresh"}));
}

#[test]
fn live_mode_drives_the_apps_control_channel_and_stays_on_ui_methods() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr").to_string();
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let log = seen.clone();
    std::thread::spawn(move || {
        let Ok((stream, _)) = listener.accept() else { return };
        let mut w = stream.try_clone().expect("clone");
        for line in BufReader::new(stream).lines().map_while(Result::ok) {
            let req: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
            let method = req["method"].as_str().unwrap_or("").to_string();
            let command = req["params"]["command"].as_str().unwrap_or("").to_string();
            log.lock().expect("log").push(if command.is_empty() { method.clone() } else { format!("{method}:{command}") });
            let result = match (method.as_str(), command.as_str()) {
                ("engine.commands", _) => json!([{"id": "tool.razor", "label": "Razor Tool", "menu": [], "params": "{}", "enabled": true, "shortcut": "C"}]),
                ("engine.execute", "history.list") => json!({"undo": [], "redo": []}),
                ("ui.click", _) => json!({"clicked": req["params"]["id"]}),
                ("ui.elements", _) => json!([{"id": "tools.Razor", "label": "Razor", "rect": [0, 0, 10, 10]}]),
                _ => Value::Null,
            };
            let reply = json!({"id": req["id"], "ok": true, "result": result});
            if writeln!(w, "{reply}").is_err() {
                break;
            }
        }
    });
    let mut c = Client::start(&["--live", &addr]);
    let found = c.ok("commands", json!({"query": "razor"}));
    assert_eq!(found["commands"][0]["id"], "tool.razor", "UI-only commands come from the app: {found}");
    let d = c.ok("ui", json!({"action": "click", "id": "tools.Razor"}));
    assert_eq!(d["result"]["clicked"], "tools.Razor");
    let d = c.ok("ui", json!({"action": "elements", "prefix": "tools."}));
    assert_eq!(d["result"][0]["id"], "tools.Razor");
    let (_, raw) = c.tool("ui", json!({"action": "call", "method": "app.quit"}));
    assert_eq!(raw["isError"], json!(true), "only ui.* methods are reachable");
    let log = seen.lock().expect("log").clone();
    assert!(log.contains(&"ui.click".to_string()) && log.contains(&"engine.commands".to_string()), "{log:?}");
    assert!(!log.iter().any(|m| m.contains("app.quit")), "{log:?}");
}

#[test]
fn live_mode_refuses_a_non_loopback_address_and_an_unreachable_app() {
    for addr in ["8.8.8.8:9876", "example.com:1"] {
        let out = Command::new(env!("CARGO_BIN_EXE_filmcraft-mcp")).args(["--live", addr]).stdin(Stdio::null()).output().expect("run");
        assert_eq!(out.status.code(), Some(1), "{addr}");
        assert!(String::from_utf8_lossy(&out.stderr).contains("loopback"), "{addr}");
    }
    let closed = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = closed.local_addr().expect("addr").to_string();
    drop(closed);
    let out = Command::new(env!("CARGO_BIN_EXE_filmcraft-mcp")).args(["--live", &addr]).stdin(Stdio::null()).output().expect("run");
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("can't reach the app"));
    assert!(String::from_utf8_lossy(&out.stdout).is_empty(), "stdout carries only protocol messages");
}
