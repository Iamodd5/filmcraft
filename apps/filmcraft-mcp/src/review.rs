//! What a person reads to review the timeline, and the change report an edit returns.
//!
//! Frame counts are derived from exact tick positions; seconds appear only as display values.

use serde_json::{Value, json};

use crate::check::{self, ClipInfo, Issue, MediaDurations, Severity, TICKS_PER_SECOND};

/// Clips listed in full in a review; beyond this the list is cut and `clipsTruncated` says so.
const MAX_CLIPS: usize = 500;
const MAP_WIDTH: usize = 72;

fn seconds(ticks: i64) -> f64 {
    ticks as f64 / TICKS_PER_SECOND as f64
}

fn frame_rate_text(seq: &Value) -> String {
    let fr = seq.get("settings").and_then(|s| s.get("frame_rate"));
    match (fr.and_then(|f| f.get("num")).and_then(Value::as_f64), fr.and_then(|f| f.get("den")).and_then(Value::as_f64)) {
        (Some(n), Some(d)) if d > 0.0 => format!("{:.3}", n / d),
        _ => "unknown".into(),
    }
}

pub fn verdict(issues: &[Issue]) -> &'static str {
    if issues.iter().any(|i| i.severity == Severity::Error) {
        "errors"
    } else if issues.iter().any(|i| i.severity == Severity::Warning) {
        "warnings"
    } else {
        "clean"
    }
}

pub fn counts(issues: &[Issue]) -> Value {
    let n = |s: Severity| issues.iter().filter(|i| i.severity == s).count();
    json!({"errors": n(Severity::Error), "warnings": n(Severity::Warning), "info": n(Severity::Info)})
}

/// The full review: sequence facts, verdict, issues, tracks, clips, markers and a text map.
pub fn review(seq: &Value, project: &Value) -> Value {
    let media: MediaDurations = check::media_durations(project);
    let issues = check::check(seq, &media);
    let ft = check::frame_ticks(seq);
    let to_frames = |t: i64| ft.map(|f| t / f);
    let duration = seq.get("duration").and_then(Value::as_i64).unwrap_or(0);
    let settings = seq.get("settings");

    let mut tracks = Vec::new();
    for (key, kind) in [("video", "video"), ("audio", "audio")] {
        for t in seq.get(key).and_then(Value::as_array).into_iter().flatten() {
            let items: Vec<&Value> = t.get("items").and_then(Value::as_array).into_iter().flatten().collect();
            let first = items.iter().filter_map(|i| i.get("start").and_then(Value::as_i64)).min();
            let last = items.iter().filter_map(|i| i.get("end").and_then(Value::as_i64)).max();
            tracks.push(json!({
                "name": t.get("name"), "kind": kind, "clips": items.len(),
                "locked": t.get("locked"), "muted": t.get("muted"), "enabled": t.get("enabled"),
                "firstFrame": first.and_then(to_frames), "endFrame": last.and_then(to_frames),
            }));
        }
    }
    let mut all = check::clips(seq);
    all.sort_by(|a, b| (!a.video, &a.track, a.start).cmp(&(!b.video, &b.track, b.start)));
    let truncated = all.len() > MAX_CLIPS;
    let clips: Vec<Value> = all.iter().take(MAX_CLIPS).map(|c| clip_json(c, ft)).collect();
    let markers: Vec<Value> = seq
        .get("markers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|m| {
            let at = m.get("start").and_then(Value::as_i64).unwrap_or(0);
            json!({"name": m.get("name"), "comment": m.get("comment"), "frame": to_frames(at), "seconds": seconds(at)})
        })
        .collect();

    json!({
        "sequence": {
            "id": seq.get("id"), "name": seq.get("name"),
            "durationFrames": to_frames(duration), "durationSeconds": seconds(duration),
            "frameRate": frame_rate_text(seq),
            "width": settings.and_then(|s| s.get("width")), "height": settings.and_then(|s| s.get("height")),
            "sampleRate": settings.and_then(|s| s.get("sample_rate")),
            "playheadFrame": seq.get("playhead").and_then(Value::as_i64).and_then(to_frames),
        },
        "verdict": verdict(&issues),
        "counts": counts(&issues),
        "issues": issues.iter().map(Issue::to_json).collect::<Vec<_>>(),
        "tracks": tracks,
        "clips": clips,
        "clipsTruncated": truncated,
        "markers": markers,
        "map": timeline_map(seq),
    })
}

fn clip_json(c: &ClipInfo, ft: Option<i64>) -> Value {
    let f = |t: i64| ft.map(|f| t / f);
    json!({
        "id": c.id, "track": c.track, "name": c.name, "item": c.item,
        "startFrame": f(c.start), "endFrame": f(c.end), "lengthFrames": f(c.end - c.start),
        "startSeconds": seconds(c.start), "endSeconds": seconds(c.end),
        "sourceInFrame": f(c.source_in), "sourceOutFrame": f(c.source_out),
        "enabled": c.enabled, "linkedTo": c.link,
    })
}

const GLYPHS: &[u8] = b"123456789abcdefghijklmnopqrstuvwxyz";

/// One text row per track: each clip is drawn with its own character (see the legend line), empty
/// time as `.`. A quick way to see where the gaps and overlaps in a timeline are.
pub fn timeline_map(seq: &Value) -> String {
    let duration = seq.get("duration").and_then(Value::as_i64).unwrap_or(0);
    if duration <= 0 {
        return "(empty timeline)".into();
    }
    let all = check::clips(seq);
    let mut tracks: Vec<(String, bool)> = Vec::new();
    for c in &all {
        if !tracks.iter().any(|(n, _)| *n == c.track) {
            tracks.push((c.track.clone(), c.video));
        }
    }
    tracks.sort_by_key(|(n, v)| (!*v, n.clone()));
    let mut out = String::new();
    let label_w = tracks.iter().map(|(n, _)| n.chars().count()).max().unwrap_or(0).max(4);
    for (name, _) in &tracks {
        let mut row = vec![b'.'; MAP_WIDTH];
        let mut clips: Vec<&ClipInfo> = all.iter().filter(|c| &c.track == name).collect();
        clips.sort_by_key(|c| c.start);
        for (n, c) in clips.iter().enumerate() {
            let glyph = GLYPHS.get(n % GLYPHS.len()).copied().unwrap_or(b'#');
            for (col, cell) in row.iter_mut().enumerate() {
                let mid = (i128::from(duration) * (2 * col as i128 + 1)) / (2 * MAP_WIDTH as i128);
                if i128::from(c.start) <= mid && mid < i128::from(c.end) {
                    *cell = glyph;
                }
            }
        }
        out.push_str(&format!("{name:<label_w$} |{}|\n", String::from_utf8_lossy(&row)));
    }
    let end = format!("{:.1}s", seconds(duration));
    out.push_str(&format!("{:<label_w$}  0s{}{end}\n", "", " ".repeat(MAP_WIDTH.saturating_sub(2 + end.len()))));
    out.push_str("(each track numbers its clips 1, 2, 3 … in time order; . = nothing there)");
    out
}

/// What an edit changed: clips added, removed, moved, trimmed or re-tracked, and the length.
pub fn diff(before: &Value, after: &Value) -> Value {
    let (b, a) = (check::by_id(before), check::by_id(after));
    let ft = check::frame_ticks(after).or_else(|| check::frame_ticks(before));
    let f = |t: i64| ft.map(|f| t / f);
    let added: Vec<Value> = a.values().filter(|c| !b.contains_key(&c.id)).map(|c| clip_json(c, ft)).collect();
    let removed: Vec<Value> = b
        .values()
        .filter(|c| !a.contains_key(&c.id))
        .map(|c| json!({"id": c.id, "track": c.track, "name": c.name, "startFrame": f(c.start), "endFrame": f(c.end)}))
        .collect();
    let mut changed = Vec::new();
    for (id, new) in &a {
        let Some(old) = b.get(id) else { continue };
        if old.start != new.start
            || old.end != new.end
            || old.track != new.track
            || old.source_in != new.source_in
            || old.source_out != new.source_out
            || old.enabled != new.enabled
        {
            changed.push(json!({
                "id": id, "name": new.name,
                "from": {"track": old.track, "startFrame": f(old.start), "endFrame": f(old.end), "sourceInFrame": f(old.source_in), "sourceOutFrame": f(old.source_out), "enabled": old.enabled},
                "to": {"track": new.track, "startFrame": f(new.start), "endFrame": f(new.end), "sourceInFrame": f(new.source_in), "sourceOutFrame": f(new.source_out), "enabled": new.enabled},
            }));
        }
    }
    let dur = |s: &Value| s.get("duration").and_then(Value::as_i64).and_then(f);
    json!({
        "clipsBefore": b.len(), "clipsAfter": a.len(),
        "durationFramesBefore": dur(before), "durationFramesAfter": dur(after),
        "added": added, "removed": removed, "changed": changed,
        "unchanged": b.len().saturating_sub(removed.len()).saturating_sub(changed.len()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const F: i64 = 10_594_584_000;

    fn seq() -> Value {
        let it = |clip: i64, s: i64, e: i64| json!({"clip": clip, "start": s * F, "end": e * F, "duration": (e - s) * F, "sourceIn": 0, "sourceOut": (e - s) * F, "name": format!("c{clip}")});
        json!({"id": 21, "name": "Main", "duration": 20 * F, "playhead": 0,
            "settings": {"frame_rate": {"num": 24000, "den": 1001}, "width": 1920, "height": 1080, "sample_rate": 48000},
            "markers": [{"name": "m", "comment": "", "start": 5 * F}],
            "video": [{"name": "Video 1", "id": 1, "items": [it(1, 0, 10), it(2, 10, 20)], "transitions": []}], "audio": []})
    }

    #[test]
    fn review_reports_frames_verdict_and_a_map() {
        let r = review(&seq(), &json!({}));
        assert_eq!(r["verdict"], "clean");
        assert_eq!(r["sequence"]["durationFrames"], 20);
        assert_eq!(r["clips"][1]["startFrame"], 10);
        assert_eq!(r["markers"][0]["frame"], 5);
        let map = r["map"].as_str().unwrap_or_default();
        assert!(map.contains("Video 1 |111111111111111111111111111111111111222222222222222222222222222222222222|"), "{map}");
    }

    #[test]
    fn diff_names_added_removed_and_changed_clips() {
        let before = seq();
        let mut after = seq();
        after["video"][0]["items"] = json!([
            {"clip": 1, "start": 0, "end": 5 * F, "duration": 5 * F, "sourceIn": 0, "sourceOut": 5 * F, "name": "c1"},
            {"clip": 9, "start": 5 * F, "end": 15 * F, "duration": 10 * F, "sourceIn": 0, "sourceOut": 10 * F, "name": "c9"}]);
        after["duration"] = json!(15 * F);
        let d = diff(&before, &after);
        assert_eq!(d["added"][0]["id"], 9);
        assert_eq!(d["removed"][0]["id"], 2);
        assert_eq!(d["changed"][0]["id"], 1);
        assert_eq!((d["durationFramesBefore"].clone(), d["durationFramesAfter"].clone()), (json!(20), json!(15)));
    }

    #[test]
    fn empty_and_malformed_sequences_do_not_panic() {
        assert_eq!(timeline_map(&json!({})), "(empty timeline)");
        let _ = review(&json!(null), &json!(null));
        let _ = diff(&json!(null), &json!([]));
    }
}
