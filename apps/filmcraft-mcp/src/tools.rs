//! The tools. Each is a thin, checked wrapper over engine commands. The ones that change the
//! timeline go through [`Server::transact`], which makes an edit all-or-nothing:
//!
//! 1. read the timeline and the issues it already has,
//! 2. run the steps,
//! 3. read it again, and compare: new invariant errors (overlaps, empty clips, dangling
//!    transitions…) or a failed `expect` condition undo every step,
//! 4. after an undo, check the timeline is byte-for-byte what it was.
//!
//! A failed edit therefore leaves the project as it found it, and says why.

use std::collections::BTreeSet;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use crate::backend::Backend;
use crate::check::{self, Issue, Severity, TICKS_PER_SECOND};
use crate::review;

/// Commands `edit` refuses: they replace the whole project (an undo can't bring it back) or write
/// the project to disk (use `project`), or render (use `export`).
const NOT_IN_EDIT: &[&str] = &[
    "file.newProject",
    "file.openDemoProject",
    "file.newProjectFromTemplate",
    "file.open",
    "file.close",
    "file.closeProject",
    "file.closeAllProjects",
    "file.closeAllOtherProjects",
    "file.revert",
    "file.recover",
    "file.projectManager",
    "file.save",
    "file.saveAs",
    "file.saveCopy",
    "file.saveAll",
    "file.saveAsTemplate",
    "file.exportMedia",
    "app.quit",
];

const MAX_STEPS: usize = 200;
const MAX_FRAMES: usize = 8;
/// Largest PNG sent inline as an image block.
const MAX_INLINE_IMAGE: u64 = 4 * 1024 * 1024;
const UNDO_LIMIT: usize = 10_000;

pub struct Output {
    pub data: Value,
    pub texts: Vec<String>,
    pub images: Vec<Vec<u8>>,
    pub is_error: bool,
}

impl Output {
    fn ok(data: Value) -> Self {
        Self { data, texts: Vec::new(), images: Vec::new(), is_error: false }
    }
    pub fn err(message: impl Into<String>) -> Self {
        Self { data: json!({"error": message.into()}), texts: Vec::new(), images: Vec::new(), is_error: true }
    }
}

struct Checkpoint {
    name: String,
    depth: usize,
    fingerprint: u64,
}

pub struct Server {
    pub backend: Box<dyn Backend>,
    checkpoints: Vec<Checkpoint>,
    scratch: PathBuf,
}

fn str_arg<'a>(a: &'a Value, k: &str) -> Option<&'a str> {
    a.get(k).and_then(Value::as_str)
}

fn bool_arg(a: &Value, k: &str, default: bool) -> bool {
    a.get(k).and_then(Value::as_bool).unwrap_or(default)
}

fn obj_arg(a: &Value, k: &str) -> Value {
    a.get(k).filter(|v| v.is_object()).cloned().unwrap_or_else(|| json!({}))
}

/// Levenshtein distance between two short strings (command ids).
fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            let sub = prev.get(j).copied().unwrap_or(0) + usize::from(ca != *cb);
            let del = prev.get(j + 1).copied().unwrap_or(0) + 1;
            let ins = cur.get(j).copied().unwrap_or(0) + 1;
            cur.push(sub.min(del).min(ins));
        }
        prev = cur;
    }
    prev.last().copied().unwrap_or(0)
}

/// Seconds → ticks, exact to the millisecond. `None` for NaN, negative or absurd values.
pub fn seconds_to_ticks(s: f64) -> Option<i64> {
    if !s.is_finite() || !(0.0..=1.0e7).contains(&s) {
        return None;
    }
    ((s * 1000.0).round() as i64).checked_mul(TICKS_PER_SECOND / 1000)
}

fn fingerprint(seq: &Value, project: &Value) -> u64 {
    let mut s = seq.clone();
    if let Some(o) = s.as_object_mut() {
        for k in ["playhead", "selection", "transitionSelection", "in", "out"] {
            o.remove(k);
        }
    }
    let mut h = DefaultHasher::new();
    s.to_string().hash(&mut h);
    project.get("root").map(Value::to_string).unwrap_or_default().hash(&mut h);
    h.finish()
}

impl Server {
    pub fn new(backend: Box<dyn Backend>) -> Self {
        let scratch = std::env::temp_dir().join(format!("filmcraft-mcp-{}", std::process::id()));
        Self { backend, checkpoints: Vec::new(), scratch }
    }

    // ---- reading the timeline ----

    fn inspect_pair(&mut self) -> Result<(Value, Value), String> {
        let seq = self.backend.exec("sequence.inspect", json!({}))?;
        if !seq.is_object() || seq.get("video").is_none() {
            return Err("there is no active sequence: open a project and a sequence first (`project` with action demo, open or new)".into());
        }
        let project = self.backend.exec("project.inspect", json!({}))?;
        Ok((seq, project))
    }

    fn depth(&mut self) -> Result<usize, String> {
        let h = self.backend.exec("history.list", json!({}))?;
        Ok(h.get("undo").and_then(Value::as_array).map_or(0, Vec::len))
    }

    /// Undo until the history is `target` steps deep. Returns how many steps were undone.
    fn undo_to(&mut self, target: usize) -> Result<usize, String> {
        let mut undone = 0;
        while undone < UNDO_LIMIT {
            let d = self.depth()?;
            if d <= target {
                return Ok(undone);
            }
            self.backend.exec("edit.undo", json!({}))?;
            undone += 1;
        }
        Err("undo did not reach the starting point".into())
    }

    fn redo_n(&mut self, n: usize) {
        for _ in 0..n.min(UNDO_LIMIT) {
            if self.backend.exec("edit.redo", json!({})).is_err() {
                break;
            }
        }
    }

    // ---- dispatch ----

    pub fn call(&mut self, name: &str, a: &Value) -> Output {
        match name {
            "commands" => self.tool_commands(a),
            "command_describe" => self.tool_describe(a),
            "query" => self.tool_query(a),
            "inspect" => self.tool_inspect(a),
            "review" => self.tool_review(),
            "edit" => self.tool_edit(a),
            "frames" => self.tool_frames(a),
            "checkpoint" => self.tool_checkpoint(a),
            "project" => self.tool_project(a),
            "import" => self.tool_import(a),
            "export" => self.tool_export(a),
            "ui" => self.tool_ui(a),
            other => Output::err(format!("unknown tool `{other}`")),
        }
    }

    // ---- discovery ----

    fn tool_commands(&mut self, a: &Value) -> Output {
        let all = match self.backend.commands() {
            Ok(v) => v,
            Err(e) => return Output::err(e),
        };
        let query = str_arg(a, "query").unwrap_or("").to_lowercase();
        let menu = str_arg(a, "menu").unwrap_or("").to_lowercase();
        let enabled_only = bool_arg(a, "enabled_only", false);
        let limit = a.get("limit").and_then(Value::as_u64).map_or(50, |l| l.clamp(1, 1000)) as usize;
        let with_params = bool_arg(a, "include_params", true);
        if query.is_empty() && menu.is_empty() && !enabled_only {
            // discovery: where the commands are, instead of a wall of them
            let mut menus: std::collections::BTreeMap<String, usize> = Default::default();
            let mut prefixes: std::collections::BTreeMap<String, usize> = Default::default();
            for c in &all {
                let top = c.get("menu").and_then(Value::as_array).and_then(|m| m.first()).and_then(Value::as_str).unwrap_or("(no menu)");
                *menus.entry(top.to_string()).or_default() += 1;
                let id = c.get("id").and_then(Value::as_str).unwrap_or("");
                *prefixes.entry(id.split('.').next().unwrap_or("").to_string()).or_default() += 1;
            }
            return Output::ok(json!({
                "total": all.len(), "byMenu": menus, "byIdPrefix": prefixes,
                "hint": "pass `query` (words in the id, label or menu path), `menu`, or `enabled_only` to list commands; `command_describe` shows one in full",
            }));
        }
        let words: Vec<&str> = query.split_whitespace().collect();
        let mut hits: Vec<Value> = Vec::new();
        let mut matched = 0usize;
        for c in &all {
            let id = c.get("id").and_then(Value::as_str).unwrap_or("");
            let label = c.get("label").and_then(Value::as_str).unwrap_or("");
            let path: Vec<&str> = c.get("menu").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
            let hay = format!("{id} {label} {}", path.join(" ")).to_lowercase();
            if !words.iter().all(|w| hay.contains(w)) {
                continue;
            }
            if !menu.is_empty() && !path.iter().any(|p| p.to_lowercase().contains(&menu)) {
                continue;
            }
            if enabled_only && c.get("enabled").and_then(Value::as_bool) != Some(true) {
                continue;
            }
            matched += 1;
            if hits.len() < limit {
                let mut o = Map::new();
                for k in ["id", "label", "menu", "shortcut", "enabled"] {
                    if let Some(v) = c.get(k) {
                        o.insert(k.into(), v.clone());
                    }
                }
                if with_params && let Some(p) = c.get("params") {
                    o.insert("params".into(), p.clone());
                }
                hits.push(Value::Object(o));
            }
        }
        Output::ok(json!({"matched": matched, "shown": hits.len(), "commands": hits}))
    }

    fn tool_describe(&mut self, a: &Value) -> Output {
        let Some(id) = str_arg(a, "id") else { return Output::err("`id` is required") };
        let all = match self.backend.commands() {
            Ok(v) => v,
            Err(e) => return Output::err(e),
        };
        if let Some(c) = all.iter().find(|c| c.get("id").and_then(Value::as_str) == Some(id)) {
            let mut c = c.clone();
            if c.get("enabled").and_then(Value::as_bool) != Some(true)
                && let Some(why) = self.backend.disabled_reason(id)
                && let Some(o) = c.as_object_mut()
            {
                o.insert("disabledReason".into(), json!(why));
            }
            return Output::ok(c);
        }
        let needle = id.to_lowercase();
        let tail = needle.rsplit('.').next().unwrap_or(&needle).to_string();
        let mut scored: Vec<(usize, &str)> = all
            .iter()
            .filter_map(|c| c.get("id").and_then(Value::as_str))
            .filter_map(|i| {
                let lower = i.to_lowercase();
                let last = lower.rsplit('.').next().unwrap_or(&lower);
                let d = edit_distance(&needle, &lower).min(edit_distance(&tail, last) + 1);
                let near = d <= 3 || (tail.len() >= 3 && lower.contains(&tail));
                near.then_some((d.min(if lower.contains(&tail) { 2 } else { d }), i))
            })
            .collect();
        scored.sort();
        let similar: Vec<&str> = scored.into_iter().map(|(_, i)| i).take(8).collect();
        Output { data: json!({"error": format!("no command `{id}`"), "similar": similar}), texts: Vec::new(), images: Vec::new(), is_error: true }
    }

    fn tool_query(&mut self, a: &Value) -> Output {
        let Some(id) = str_arg(a, "id") else { return Output::err("`id` is required") };
        if NOT_IN_EDIT.contains(&id) {
            return Output::err(format!("`{id}` is not a query; use `project` or `export`"));
        }
        let before = match self.depth() {
            Ok(d) => d,
            Err(e) => return Output::err(e),
        };
        let r = self.backend.exec(id, obj_arg(a, "params"));
        let after = self.depth().unwrap_or(before);
        if after != before {
            // it changed the project: not a query. Put it back and say so.
            let _ = self.undo_to(before);
            return Output::err(format!("`{id}` changes the project, so it is not a query (it was undone); run it with `edit`"));
        }
        match r {
            Ok(v) => Output::ok(json!({"id": id, "result": v})),
            Err(e) => Output::err(e),
        }
    }

    fn tool_inspect(&mut self, a: &Value) -> Output {
        let scope = str_arg(a, "scope").unwrap_or("all");
        let mut out = Map::new();
        let mut run = |s: &mut Self, key: &str, id: &str, params: Value| -> Result<(), String> {
            out.insert(key.into(), s.backend.exec(id, params)?);
            Ok(())
        };
        let seq_params = a.get("sequence").and_then(Value::as_i64).map_or_else(|| json!({}), |i| json!({"item": i}));
        let r = match scope {
            "project" => run(self, "project", "project.inspect", json!({})),
            "sequence" => run(self, "sequence", "sequence.inspect", seq_params),
            "state" => run(self, "state", "state.inspect", json!({})),
            "history" => run(self, "history", "history.list", json!({})),
            _ => run(self, "project", "project.inspect", json!({})).and_then(|()| run(self, "sequence", "sequence.inspect", seq_params)),
        };
        match r {
            Ok(()) => Output::ok(Value::Object(out)),
            Err(e) => Output::err(e),
        }
    }

    // ---- review ----

    fn tool_review(&mut self) -> Output {
        let (seq, project) = match self.inspect_pair() {
            Ok(p) => p,
            Err(e) => return Output::err(e),
        };
        let r = review::review(&seq, &project);
        let mut o = Output::ok(r.clone());
        let name = r["sequence"]["name"].as_str().unwrap_or("sequence");
        let frames = r["sequence"]["durationFrames"].as_i64().unwrap_or(0);
        let mut text = format!(
            "{name}: {frames} frames at {} fps, verdict {}\n{}",
            r["sequence"]["frameRate"].as_str().unwrap_or("?"),
            r["verdict"].as_str().unwrap_or("?"),
            r["map"].as_str().unwrap_or("")
        );
        for i in r["issues"].as_array().into_iter().flatten().filter(|i| i["severity"] != "info") {
            text.push_str(&format!("\n[{}] {}", i["severity"].as_str().unwrap_or("?"), i["message"].as_str().unwrap_or("")));
        }
        o.texts.push(text);
        o
    }

    // ---- editing ----

    fn tool_edit(&mut self, a: &Value) -> Output {
        let Some(steps) = a.get("steps").and_then(Value::as_array) else { return Output::err("`steps` is required") };
        if steps.is_empty() || steps.len() > MAX_STEPS {
            return Output::err(format!("`steps` takes 1 to {MAX_STEPS} steps"));
        }
        let mut plan = Vec::new();
        for (i, s) in steps.iter().enumerate() {
            let Some(id) = s.get("id").and_then(Value::as_str).filter(|id| !id.is_empty() && id.len() <= 128) else {
                return Output::err(format!("steps[{i}] needs an `id`"));
            };
            if NOT_IN_EDIT.contains(&id) {
                return Output::err(format!(
                    "steps[{i}]: `{id}` is not allowed in `edit` (it replaces or writes the project); use the `project` or `export` tool"
                ));
            }
            plan.push((id.to_string(), obj_arg(s, "params")));
        }
        self.transact(plan, a.get("expect").cloned().unwrap_or(Value::Null), bool_arg(a, "dry_run", false), bool_arg(a, "atomic", true))
    }

    fn transact(&mut self, steps: Vec<(String, Value)>, expect: Value, dry_run: bool, atomic: bool) -> Output {
        let (seq0, proj0) = match self.inspect_pair() {
            Ok(p) => p,
            Err(e) => return Output::err(e),
        };
        let media = check::media_durations(&proj0);
        let issues0 = check::check(&seq0, &media);
        let fp0 = fingerprint(&seq0, &proj0);
        let depth0 = match self.depth() {
            Ok(d) => d,
            Err(e) => return Output::err(e),
        };

        let mut results = Vec::new();
        let mut reasons: Vec<String> = Vec::new();
        let mut irreversible: Vec<String> = Vec::new();
        for (i, (id, params)) in steps.iter().enumerate() {
            match self.backend.exec(id, params.clone()) {
                Ok(v) => results.push(json!({"step": i, "id": id, "ok": true, "result": v})),
                Err(e) => {
                    results.push(json!({"step": i, "id": id, "ok": false, "error": e}));
                    reasons.push(format!("step {i} (`{id}`) failed: {e}"));
                    break;
                }
            }
            if id.starts_with("file.export") {
                irreversible.push(id.clone());
            }
        }
        let steps_ok = reasons.is_empty();

        // look at what it did
        let (seq1, proj1) = match self.inspect_pair() {
            Ok(p) => p,
            Err(e) => {
                let _ = self.undo_to(depth0);
                return Output::err(format!("the timeline could not be read after the edit, so it was undone: {e}"));
            }
        };
        let issues1 = check::check(&seq1, &check::media_durations(&proj1));
        let new: Vec<&Issue> = check::new_issues(&issues0, &issues1);
        for i in new.iter().filter(|i| i.severity == Severity::Error) {
            reasons.push(format!("the edit broke the timeline: {}", i.message));
        }
        let diff = review::diff(&seq0, &seq1);
        if steps_ok {
            reasons.extend(evaluate_expect(&expect, &seq0, &seq1, &issues1, &diff));
        }

        let must_undo = dry_run || (atomic && !reasons.is_empty());
        let mut rolled_back = false;
        let mut rollback_note = Value::Null;
        if must_undo {
            match self.undo_to(depth0) {
                Ok(_) => {
                    rolled_back = true;
                    // prove it: the timeline is exactly what it was
                    match self.inspect_pair() {
                        Ok((s, p)) if fingerprint(&s, &p) == fp0 => {}
                        Ok(_) => {
                            rollback_note = json!(
                                "WARNING: after undoing the steps the timeline still differs from the start (a step changed something undo does not cover, such as an export or a file). Run `review` and check"
                            );
                        }
                        Err(e) => rollback_note = json!(format!("WARNING: could not re-read the timeline after the undo: {e}")),
                    }
                }
                Err(e) => rollback_note = json!(format!("WARNING: the undo failed: {e}. The edit may be partly applied; run `review`")),
            }
        }
        let failed = !reasons.is_empty();
        let committed = !rolled_back && (steps_ok || !atomic);
        let mut data = json!({
            "ok": !failed,
            "committed": committed && !failed,
            "dryRun": dry_run,
            "rolledBack": rolled_back,
            "reasons": reasons,
            "steps": results,
            "changes": diff,
            "verdictAfter": review::verdict(&issues1),
            "newIssues": new.iter().map(|i| i.to_json()).collect::<Vec<_>>(),
        });
        if !irreversible.is_empty()
            && let Some(o) = data.as_object_mut()
        {
            o.insert("notUndoable".into(), json!(irreversible));
        }
        if !rollback_note.is_null()
            && let Some(o) = data.as_object_mut()
        {
            o.insert("rollbackWarning".into(), rollback_note);
        }
        if dry_run && let Some(o) = data.as_object_mut() {
            o.insert("note".into(), json!("dry run: nothing was kept; `changes` is what the steps would do"));
        }
        let mut out = Output::ok(data);
        out.is_error = failed && !dry_run;
        out
    }

    // ---- seeing ----

    fn tool_frames(&mut self, a: &Value) -> Output {
        let (seq, _) = match self.inspect_pair() {
            Ok(p) => p,
            Err(e) => return Output::err(e),
        };
        let duration = seq.get("duration").and_then(Value::as_i64).unwrap_or(0);
        if duration <= 0 {
            return Output::err("the sequence is empty, there is nothing to render");
        }
        let ft = check::frame_ticks(&seq);
        let mut times: Vec<i64> = Vec::new();
        if let Some(list) = a.get("seconds").and_then(Value::as_array) {
            for s in list {
                match s.as_f64().and_then(seconds_to_ticks) {
                    Some(t) if t < duration => times.push(t),
                    Some(_) => {
                        return Output::err(format!(
                            "{s} seconds is past the end of the sequence, which is {:.3} seconds long",
                            duration as f64 / TICKS_PER_SECOND as f64
                        ));
                    }
                    None => return Output::err(format!("`seconds` has an unusable value: {s}")),
                }
            }
        } else {
            let n = a.get("count").and_then(Value::as_u64).map_or(4, |n| n.clamp(1, MAX_FRAMES as u64)) as i128;
            for k in 0..n {
                let t = (i128::from(duration) * (2 * k + 1)) / (2 * n);
                times.push(i64::try_from(t).unwrap_or(0));
            }
        }
        if times.is_empty() || times.len() > MAX_FRAMES {
            return Output::err(format!("ask for 1 to {MAX_FRAMES} frames"));
        }
        if let Err(e) = std::fs::create_dir_all(&self.scratch) {
            return Output::err(format!("can't create {}: {e}", self.scratch.display()));
        }
        let inline = bool_arg(a, "inline", true);
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis());
        let mut frames = Vec::new();
        let mut out = Output::ok(Value::Null);
        for (n, t) in times.iter().enumerate() {
            let path = self.scratch.join(format!("frame-{stamp}-{n}.png"));
            let params = json!({"path": path.to_string_lossy(), "target": "program", "time": t, "format": "png"});
            match self.backend.exec("file.exportFrame", params) {
                Ok(r) => {
                    let bytes = std::fs::metadata(&path).map_or(0, |m| m.len());
                    if bytes == 0 {
                        return Output::err(format!("rendering frame {n} produced no file at {}", path.display()));
                    }
                    if inline
                        && bytes <= MAX_INLINE_IMAGE
                        && let Ok(b) = std::fs::read(&path)
                    {
                        out.images.push(b);
                    }
                    frames.push(json!({
                        "path": path.to_string_lossy(), "bytes": bytes,
                        "seconds": *t as f64 / TICKS_PER_SECOND as f64, "frame": ft.map(|f| t / f),
                        "width": r.get("width"), "height": r.get("height"),
                    }));
                }
                Err(e) => return Output::err(format!("rendering frame {n} failed: {e}")),
            }
        }
        out.data = json!({"frames": frames, "inlineImages": out.images.len()});
        out
    }

    // ---- checkpoints ----

    fn tool_checkpoint(&mut self, a: &Value) -> Output {
        let action = str_arg(a, "action").unwrap_or("list");
        let name = str_arg(a, "name").unwrap_or("");
        if matches!(action, "create" | "restore" | "drop") && (name.is_empty() || name.len() > 64) {
            return Output::err("`name` (1 to 64 characters) is required");
        }
        match action {
            "list" => Output::ok(json!({"checkpoints": self.checkpoints.iter().map(|c| json!({"name": c.name, "historyDepth": c.depth})).collect::<Vec<_>>()})),
            "drop" => {
                let before = self.checkpoints.len();
                self.checkpoints.retain(|c| c.name != name);
                if self.checkpoints.len() == before { Output::err(format!("no checkpoint `{name}`")) } else { Output::ok(json!({"dropped": name})) }
            }
            "create" => {
                let (seq, project) = match self.inspect_pair() {
                    Ok(p) => p,
                    Err(e) => return Output::err(e),
                };
                let depth = match self.depth() {
                    Ok(d) => d,
                    Err(e) => return Output::err(e),
                };
                self.checkpoints.retain(|c| c.name != name);
                self.checkpoints.push(Checkpoint { name: name.to_string(), depth, fingerprint: fingerprint(&seq, &project) });
                Output::ok(json!({"created": name, "historyDepth": depth}))
            }
            "restore" => {
                let Some(cp) = self.checkpoints.iter().find(|c| c.name == name) else { return Output::err(format!("no checkpoint `{name}`")) };
                let (depth, want) = (cp.depth, cp.fingerprint);
                let now = match self.depth() {
                    Ok(d) => d,
                    Err(e) => return Output::err(e),
                };
                if now < depth {
                    return Output::err(format!("the history is only {now} steps deep, shallower than checkpoint `{name}` ({depth}): it can't be restored"));
                }
                let undone = match self.undo_to(depth) {
                    Ok(n) => n,
                    Err(e) => return Output::err(e),
                };
                match self.inspect_pair() {
                    Ok((s, p)) if fingerprint(&s, &p) == want => Output::ok(json!({"restored": name, "stepsUndone": undone})),
                    _ => {
                        // the history was rewritten since: don't leave the project half-way
                        self.redo_n(undone);
                        Output::err(format!("checkpoint `{name}` can't be restored: edits made after an undo replaced its history. Nothing was changed"))
                    }
                }
            }
            _ => Output::err("`action` is create, restore, list or drop"),
        }
    }

    // ---- project, import, export ----

    fn project_info(&mut self) -> Value {
        let p = self.backend.exec("project.inspect", json!({})).unwrap_or(Value::Null);
        json!({"name": p.get("name"), "path": p.get("path"), "dirty": p.get("dirty"), "activeSequence": p.get("activeSequence"), "revision": p.get("revision")})
    }

    fn tool_project(&mut self, a: &Value) -> Output {
        let action = str_arg(a, "action").unwrap_or("info");
        let path = str_arg(a, "path");
        let replaces = matches!(action, "open" | "new" | "demo");
        if action == "open" {
            match path {
                Some(p) if Path::new(p).is_file() => {}
                Some(p) => return Output::err(format!("no project file at {p}")),
                None => return Output::err("`path` is required"),
            }
        }
        if replaces {
            let info = self.project_info();
            if info["dirty"] == json!(true) && !bool_arg(a, "discard_changes", false) {
                return Output::err("the open project has unsaved changes: save it first (`project` action save), or pass `discard_changes: true`");
            }
        }
        let r = match action {
            "info" => return Output::ok(self.project_info()),
            "demo" => self.backend.exec("file.openDemoProject", json!({})),
            "new" => match str_arg(a, "name").filter(|n| !n.is_empty()) {
                Some(n) => self.backend.exec("file.newProject", json!({"name": n})),
                None => return Output::err("`name` is required"),
            },
            "open" => match path {
                Some(p) if Path::new(p).is_file() => self.backend.exec("file.open", json!({"path": p})),
                Some(p) => return Output::err(format!("no project file at {p}")),
                None => return Output::err("`path` is required"),
            },
            "save" => self.backend.exec("file.save", path.map_or_else(|| json!({}), |p| json!({"path": p}))),
            "save_as" => match path {
                Some(p) if Path::new(p).parent().is_some_and(|d| d.as_os_str().is_empty() || d.is_dir()) => {
                    self.backend.exec("file.saveAs", json!({"path": p}))
                }
                Some(p) => return Output::err(format!("the folder for {p} does not exist")),
                None => return Output::err("`path` is required"),
            },
            _ => return Output::err("`action` is info, demo, new, open, save or save_as"),
        };
        if let Err(e) = r {
            return Output::err(e);
        }
        if replaces {
            self.checkpoints.clear();
        }
        let info = self.project_info();
        if matches!(action, "save" | "save_as") {
            let saved = info["path"].as_str().map(PathBuf::from);
            let ok = saved.as_ref().is_some_and(|p| std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.len() > 0));
            if !ok {
                return Output::err(format!(
                    "the save reported success but no project file was found at {}",
                    saved.map_or_else(|| "(no path)".into(), |p| p.display().to_string())
                ));
            }
            return Output::ok(json!({"saved": true, "project": info, "bytes": saved.and_then(|p| std::fs::metadata(p).ok()).map(|m| m.len())}));
        }
        Output::ok(json!({"project": info}))
    }

    fn tool_import(&mut self, a: &Value) -> Output {
        let Some(paths) = a.get("paths").and_then(Value::as_array) else { return Output::err("`paths` is required") };
        let mut list = Vec::new();
        for p in paths {
            let Some(p) = p.as_str() else { return Output::err("`paths` must be strings") };
            match std::fs::metadata(p) {
                Ok(m) if m.is_file() && m.len() > 0 => list.push(p.to_string()),
                Ok(_) => return Output::err(format!("{p} is empty or not a file")),
                Err(e) => return Output::err(format!("can't read {p}: {e}")),
            }
        }
        let mut params = json!({"paths": list});
        if let Some(bin) = a.get("bin")
            && let Some(o) = params.as_object_mut()
        {
            o.insert("bin".into(), bin.clone());
        }
        self.transact(vec![("file.import".into(), params)], Value::Null, false, true)
    }

    fn tool_export(&mut self, a: &Value) -> Output {
        let Some(path) = str_arg(a, "path") else { return Output::err("`path` is required") };
        let parent = Path::new(path).parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
        if !parent.is_dir() {
            return Output::err(format!("the folder {} does not exist", parent.display()));
        }
        let (seq, project) = match self.inspect_pair() {
            Ok(p) => p,
            Err(e) => return Output::err(e),
        };
        let issues = check::check(&seq, &check::media_durations(&project));
        if !bool_arg(a, "allow_errors", false)
            && let Some(i) = issues.iter().find(|i| i.severity == Severity::Error)
        {
            return Output::err(format!("not exporting a broken timeline: {} (fix it, or pass `allow_errors: true`)", i.message));
        }
        let mut params = obj_arg(a, "options");
        let Some(o) = params.as_object_mut() else { return Output::err("`options` must be an object") };
        o.insert("path".into(), json!(path));
        o.insert("wait".into(), json!(true));
        for (arg, key) in [("preset", "preset"), ("format", "format"), ("range", "range"), ("start_seconds", "startSeconds"), ("end_seconds", "endSeconds")] {
            if let Some(v) = a.get(arg) {
                o.insert(key.into(), v.clone());
            }
        }
        let before = std::fs::metadata(path).ok().map(|m| (m.len(), m.modified().ok()));
        let r = match self.backend.exec("file.exportMedia", params) {
            Ok(r) => r,
            Err(e) => return Output::err(format!("export failed: {e}")),
        };
        let after = std::fs::metadata(path).ok().map(|m| (m.len(), m.modified().ok()));
        let verified = after.is_some_and(|(len, _)| len > 0) && after != before;
        let mut out = Output::ok(
            json!({"exported": verified, "path": path, "bytes": after.map(|(l, _)| l), "result": r, "timelineWarnings": issues.iter().filter(|i| i.severity == Severity::Warning).count()}),
        );
        if !verified {
            out.is_error = true;
            out.data["error"] = json!(
                "the export finished but no new, non-empty file was found at the path (an image or frame sequence may write numbered files next to it: check the folder)"
            );
        }
        out
    }

    // ---- the UI ----

    fn tool_ui(&mut self, a: &Value) -> Output {
        let Some(action) = str_arg(a, "action") else { return Output::err("`action` is required") };
        let pick = |keys: &[&str]| -> Value {
            let mut o = Map::new();
            for k in keys {
                if let Some(v) = a.get(*k) {
                    o.insert((*k).into(), v.clone());
                }
            }
            Value::Object(o)
        };
        let (method, params): (String, Value) = match action {
            "elements" => ("ui.elements".into(), pick(&["prefix"])),
            "inspect" => ("ui.inspect".into(), json!({})),
            "click" => ("ui.click".into(), pick(&["id", "x", "y", "button", "count", "modifiers"])),
            "move" => ("ui.move".into(), pick(&["id", "x", "y"])),
            "drag" => ("ui.drag".into(), pick(&["from", "to", "steps", "modifiers"])),
            "scroll" => ("ui.scroll".into(), pick(&["id", "x", "y", "dx", "dy", "modifiers"])),
            "key" => ("ui.key".into(), pick(&["key"])),
            "type" => ("ui.type".into(), pick(&["text"])),
            "playback" => (
                "ui.playback".into(),
                pick(&["playback_action", "speed"]).as_object().map_or_else(
                    || json!({}),
                    |o| {
                        let mut o = o.clone();
                        if let Some(v) = o.remove("playback_action") {
                            o.insert("action".into(), v);
                        }
                        Value::Object(o)
                    },
                ),
            ),
            "panel" => (if bool_arg(a, "show", true) { "ui.panel.show" } else { "ui.panel.close" }.into(), pick(&["panel"])),
            "set" => ("ui.set".into(), obj_arg(a, "state")),
            "screenshot" => {
                if let Err(e) = std::fs::create_dir_all(&self.scratch) {
                    return Output::err(format!("can't create {}: {e}", self.scratch.display()));
                }
                let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis());
                let path = self.scratch.join(format!("ui-{stamp}.png"));
                let mut p = pick(&["panel"]);
                if let Some(o) = p.as_object_mut() {
                    o.insert("path".into(), json!(path.to_string_lossy()));
                }
                match self.backend.ui_call("ui.screenshot", p) {
                    Ok(r) => {
                        let mut out = Output::ok(json!({"path": path.to_string_lossy(), "result": r}));
                        if std::fs::metadata(&path).is_ok_and(|m| m.len() <= MAX_INLINE_IMAGE)
                            && let Ok(b) = std::fs::read(&path)
                        {
                            out.images.push(b);
                        }
                        return out;
                    }
                    Err(e) => return Output::err(e),
                }
            }
            "call" => match str_arg(a, "method") {
                Some(m) => (m.to_string(), obj_arg(a, "params")),
                None => return Output::err("`method` is required for action call"),
            },
            _ => return Output::err("unknown `action`"),
        };
        match self.backend.ui_call(&method, params) {
            Ok(v) => Output::ok(json!({"method": method, "result": v})),
            Err(e) => Output::err(e),
        }
    }
}

/// Failed conditions of an `expect` object, as sentences. Empty when it holds.
fn evaluate_expect(expect: &Value, before: &Value, after: &Value, issues: &[Issue], diff: &Value) -> Vec<String> {
    let mut out = Vec::new();
    let Some(e) = expect.as_object() else { return out };
    let ft = check::frame_ticks(after);
    let now = check::by_id(after);
    let dur_frames = after.get("duration").and_then(Value::as_i64).and_then(|d| ft.map(|f| d / f));
    let ids = |k: &str| -> Vec<i64> { e.get(k).and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_i64).collect() };

    match e.get("durationFrames") {
        Some(Value::Number(n)) => {
            if dur_frames != n.as_i64() {
                out.push(format!("expected the sequence to be {n} frames long, it is {}", dur_frames.map_or("unknown".into(), |d| d.to_string())));
            }
        }
        Some(Value::Object(r)) => {
            let (min, max) = (r.get("min").and_then(Value::as_i64), r.get("max").and_then(Value::as_i64));
            if let Some(d) = dur_frames
                && (min.is_some_and(|m| d < m) || max.is_some_and(|m| d > m))
            {
                out.push(format!(
                    "expected a length of {}..{} frames, it is {d}",
                    min.map_or("-".into(), |v| v.to_string()),
                    max.map_or("-".into(), |v| v.to_string())
                ));
            }
        }
        _ => {}
    }
    if let Some(n) = e.get("clipCount").and_then(Value::as_u64)
        && now.len() as u64 != n
    {
        out.push(format!("expected {n} clips in the sequence, found {}", now.len()));
    }
    if let Some(tc) = e.get("trackClipCount").and_then(Value::as_object) {
        for (track, want) in tc {
            let have = now.values().filter(|c| &c.track == track).count() as u64;
            if want.as_u64() != Some(have) {
                out.push(format!("expected {want} clips on {track}, found {have}"));
            }
        }
    }
    for id in ids("clipsPresent") {
        if !now.contains_key(&id) {
            out.push(format!("expected clip {id} to exist, it does not"));
        }
    }
    for id in ids("clipsAbsent") {
        if now.contains_key(&id) {
            out.push(format!("expected clip {id} to be gone, it is still there"));
        }
    }
    let was = check::by_id(before);
    let moved: BTreeSet<i64> = diff["changed"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| c["id"].as_i64())
        .chain(diff["removed"].as_array().into_iter().flatten().filter_map(|c| c["id"].as_i64()))
        .collect();
    for id in ids("unchangedClips") {
        if was.contains_key(&id) && moved.contains(&id) {
            out.push(format!("clip {id} had to stay as it was, but the edit changed or removed it"));
        }
    }
    if e.contains_key("onlyChanging") {
        let allowed: BTreeSet<i64> = ids("onlyChanging").into_iter().collect();
        let stray: Vec<String> = moved.difference(&allowed).map(i64::to_string).collect();
        if !stray.is_empty() {
            out.push(format!("the edit also changed clips it was not meant to touch: {}", stray.join(", ")));
        }
    }
    if let Some(max) = e.get("maxAddedClips").and_then(Value::as_u64) {
        let added = diff["added"].as_array().map_or(0, Vec::len) as u64;
        if added > max {
            out.push(format!("the edit added {added} clips, at most {max} were expected"));
        }
    }
    if e.get("noGaps") == Some(&json!(true))
        && let Some(g) = issues.iter().find(|i| matches!(i.code, "gap" | "leading_gap"))
    {
        out.push(format!("expected no gaps: {}", g.message));
    }
    if e.get("noWarnings") == Some(&json!(true))
        && let Some(w) = issues.iter().find(|i| i.severity == Severity::Warning)
    {
        out.push(format!("expected no warnings: {}", w.message));
    }
    out
}

/// The tool list with JSON Schemas (what `tools/list` returns).
pub fn definitions() -> Vec<Value> {
    let step = json!({"type": "object", "properties": {
        "id": {"type": "string", "description": "command id, e.g. timeline.razor (find ids with `commands`)"},
        "params": {"type": "object", "description": "the command's parameters, see `command_describe`"}},
        "required": ["id"], "additionalProperties": false});
    let expect = json!({"type": "object", "description": "conditions that must hold after the steps, or every step is undone", "properties": {
        "durationFrames": {"type": ["integer", "object"], "description": "exact frame count, or {min, max}"},
        "clipCount": {"type": "integer"},
        "trackClipCount": {"type": "object", "description": "track name -> clip count, e.g. {\"Video 1\": 3}"},
        "clipsPresent": {"type": "array", "items": {"type": "integer"}},
        "clipsAbsent": {"type": "array", "items": {"type": "integer"}},
        "unchangedClips": {"type": "array", "items": {"type": "integer"}, "description": "clips that must not move, trim, change track or disappear"},
        "onlyChanging": {"type": "array", "items": {"type": "integer"}, "description": "the only existing clips the edit may change or remove"},
        "maxAddedClips": {"type": "integer", "minimum": 0},
        "noGaps": {"type": "boolean"}, "noWarnings": {"type": "boolean"}},
        "additionalProperties": false});
    let t = |name: &str, title: &str, desc: &str, schema: Value, read_only: bool, destructive: bool| {
        json!({"name": name, "title": title, "description": desc, "inputSchema": schema,
               "annotations": {"title": title, "readOnlyHint": read_only, "destructiveHint": destructive, "idempotentHint": read_only, "openWorldHint": false}})
    };
    vec![
        t(
            "commands",
            "Find commands",
            "Search every FilmCraft command (menu items, panel buttons, shortcuts): id, label, menu path, shortcut, parameters, enabled now. With no arguments, shows how the commands are grouped.",
            json!({"type": "object", "properties": {"query": {"type": "string"}, "menu": {"type": "string"}, "enabled_only": {"type": "boolean"}, "limit": {"type": "integer", "minimum": 1, "maximum": 1000}, "include_params": {"type": "boolean"}}, "additionalProperties": false}),
            true,
            false,
        ),
        t(
            "command_describe",
            "Describe a command",
            "One command in full: label, menu, shortcut, parameter syntax, whether it can run now and, if not, why.",
            json!({"type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"], "additionalProperties": false}),
            true,
            false,
        ),
        t(
            "query",
            "Run a read-only command",
            "Run a command that only reads (inspect, list, scopes.read…). If it turns out to change the project it is undone and reported.",
            json!({"type": "object", "properties": {"id": {"type": "string"}, "params": {"type": "object"}}, "required": ["id"], "additionalProperties": false}),
            true,
            false,
        ),
        t(
            "inspect",
            "Inspect the project",
            "The project tree, the active sequence (tracks, clips with ids, ticks and frames, effects, transitions, markers), editor state or undo history. Ids from here go into command params.",
            json!({"type": "object", "properties": {"scope": {"enum": ["all", "project", "sequence", "state", "history"]}, "sequence": {"type": "integer", "description": "a sequence's item id (default: the active one)"}}, "additionalProperties": false}),
            true,
            false,
        ),
        t(
            "review",
            "Review the timeline",
            "The timeline as a person reviews it: verdict (clean, warnings, errors), every issue (overlaps, gaps, picture and sound out of sync, clips running past their media), tracks, clips in frames, markers and a text map. Run it before and after every edit.",
            json!({"type": "object", "properties": {}, "additionalProperties": false}),
            true,
            false,
        ),
        t(
            "edit",
            "Edit the timeline (all or nothing)",
            "Run one or more commands as ONE verified edit. Afterwards the timeline is checked; if a step fails, the edit breaks the timeline (overlap, empty clip, dangling transition…) or an `expect` condition is false, every step is undone and the timeline is verified to be what it was. `dry_run` previews. Returns exactly what changed (clips added, removed, moved, trimmed).",
            json!({"type": "object", "properties": {"steps": {"type": "array", "items": step, "minItems": 1, "maxItems": MAX_STEPS}, "expect": expect, "dry_run": {"type": "boolean"}, "atomic": {"type": "boolean", "description": "default true: undo everything when anything fails"}}, "required": ["steps"], "additionalProperties": false}),
            false,
            true,
        ),
        t(
            "frames",
            "See frames",
            "Render frames of the program output as PNG images (inline) so you can look at the result: given `seconds`, or `count` evenly spaced samples across the sequence.",
            json!({"type": "object", "properties": {"seconds": {"type": "array", "items": {"type": "number", "minimum": 0}, "maxItems": MAX_FRAMES}, "count": {"type": "integer", "minimum": 1, "maximum": MAX_FRAMES}, "inline": {"type": "boolean"}}, "additionalProperties": false}),
            true,
            false,
        ),
        t(
            "checkpoint",
            "Checkpoints",
            "Mark the current state and return to it later: create, restore, list, drop. Restoring undoes back to the mark and verifies the timeline matches it.",
            json!({"type": "object", "properties": {"action": {"enum": ["create", "restore", "list", "drop"]}, "name": {"type": "string"}}, "required": ["action"], "additionalProperties": false}),
            false,
            true,
        ),
        t(
            "project",
            "Open, create and save projects",
            "info, demo (open the built-in demo), new, open, save, save_as. Refuses to replace a project with unsaved changes unless `discard_changes`. A save is checked: the file must exist and be non-empty.",
            json!({"type": "object", "properties": {"action": {"enum": ["info", "demo", "new", "open", "save", "save_as"]}, "path": {"type": "string"}, "name": {"type": "string"}, "discard_changes": {"type": "boolean"}}, "required": ["action"], "additionalProperties": false}),
            false,
            true,
        ),
        t(
            "import",
            "Import media",
            "Import media files by absolute path. Every file is checked first; the import is one verified, undoable edit.",
            json!({"type": "object", "properties": {"paths": {"type": "array", "items": {"type": "string"}, "minItems": 1, "maxItems": 500}, "bin": {"type": ["string", "integer"]}}, "required": ["paths"], "additionalProperties": false}),
            false,
            false,
        ),
        t(
            "export",
            "Export the sequence",
            "Render the active sequence to a file and wait for it. Refuses a timeline with errors, and checks that a new, non-empty file was written.",
            json!({"type": "object", "properties": {"path": {"type": "string"}, "preset": {"type": "string"}, "format": {"type": "string"}, "range": {"enum": ["entire", "inOut", "workArea", "custom"]}, "start_seconds": {"type": "number", "minimum": 0}, "end_seconds": {"type": "number", "minimum": 0}, "options": {"type": "object", "description": "any other file.exportMedia parameters"}, "allow_errors": {"type": "boolean"}}, "required": ["path"], "additionalProperties": false}),
            false,
            false,
        ),
        t(
            "ui",
            "Drive the app UI",
            "Press any control of the running desktop app (needs `--live`): elements (list every clickable id), click, drag, scroll, key, type, move, playback, panel, set (UI state), screenshot, inspect, or call (any ui.* control method).",
            json!({"type": "object", "properties": {
                "action": {"enum": ["elements", "inspect", "click", "move", "drag", "scroll", "key", "type", "playback", "panel", "set", "screenshot", "call"]},
                "prefix": {"type": "string"}, "id": {"type": "string"}, "x": {"type": "number"}, "y": {"type": "number"},
                "button": {"type": "string"}, "count": {"type": "integer"}, "modifiers": {"type": "array", "items": {"type": "string"}},
                "from": {"type": ["string", "object", "array"]}, "to": {"type": ["string", "object", "array"]}, "steps": {"type": "integer"},
                "dx": {"type": "number"}, "dy": {"type": "number"}, "key": {"type": "string"}, "text": {"type": "string"},
                "playback_action": {"enum": ["play", "stop", "toggle"]}, "speed": {"type": "number"},
                "panel": {"type": "string"}, "show": {"type": "boolean"}, "state": {"type": "object"},
                "method": {"type": "string"}, "params": {"type": "object"}},
                "required": ["action"], "additionalProperties": false}),
            false,
            true,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Fake;
    use std::collections::HashMap;

    #[test]
    fn seconds_convert_exactly_and_reject_nonsense() {
        assert_eq!(seconds_to_ticks(1.0), Some(TICKS_PER_SECOND));
        assert_eq!(seconds_to_ticks(0.5), Some(TICKS_PER_SECOND / 2));
        for bad in [f64::NAN, f64::INFINITY, -1.0, 1.0e9] {
            assert_eq!(seconds_to_ticks(bad), None);
        }
    }

    #[test]
    fn edit_distance_is_levenshtein() {
        assert_eq!(edit_distance("razr", "razor"), 1);
        assert_eq!(edit_distance("", "abc"), 3);
        assert_eq!(edit_distance("kitten", "sitting"), 3);
        assert_eq!(edit_distance("same", "same"), 0);
    }

    #[test]
    fn every_tool_has_a_schema_that_rejects_unknown_arguments() {
        for d in definitions() {
            let name = d["name"].as_str().unwrap_or("?");
            assert_eq!(d["inputSchema"]["additionalProperties"], json!(false), "{name}");
            assert!(d["annotations"]["readOnlyHint"].is_boolean(), "{name}");
            assert!(d["title"].is_string() && d["description"].is_string(), "{name}");
        }
    }

    #[test]
    fn unknown_tool_and_missing_arguments_are_errors_not_panics() {
        let mut s = Server::new(Box::new(Fake { replies: HashMap::new(), calls: Vec::new() }));
        assert!(s.call("nope", &json!({})).is_error);
        for t in ["command_describe", "query", "edit", "export", "import", "ui"] {
            assert!(s.call(t, &json!({})).is_error, "{t}");
        }
    }

    #[test]
    fn expectations_report_each_broken_condition() {
        let it = |c: i64, s: i64, e: i64| json!({"clip": c, "start": s, "end": e, "duration": e - s, "sourceIn": 0, "sourceOut": e - s});
        let f = 10_594_584_000i64;
        let mk = |items: Vec<Value>, dur: i64| json!({"duration": dur, "settings": {"frame_rate": {"num": 24000, "den": 1001}}, "audio": [], "video": [{"name": "V1", "items": items, "transitions": []}]});
        let before = mk(vec![it(1, 0, 10 * f), it(2, 10 * f, 20 * f)], 20 * f);
        let after = mk(vec![it(1, 0, 5 * f)], 5 * f);
        let diff = review::diff(&before, &after);
        let issues = check::check(&after, &Default::default());
        let expect = json!({"durationFrames": 20, "clipCount": 2, "clipsAbsent": [1], "clipsPresent": [2], "unchangedClips": [1], "onlyChanging": [], "trackClipCount": {"V1": 2}});
        let r = evaluate_expect(&expect, &before, &after, &issues, &diff);
        assert_eq!(r.len(), 7, "{r:#?}");
        let ok = json!({"durationFrames": {"min": 1, "max": 5}, "clipCount": 1, "clipsPresent": [1], "clipsAbsent": [2], "onlyChanging": [1, 2]});
        assert!(evaluate_expect(&ok, &before, &after, &issues, &diff).is_empty());
    }
}
