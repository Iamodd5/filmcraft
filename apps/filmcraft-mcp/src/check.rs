//! Timeline invariants, checked on the JSON that `sequence.inspect` and `project.inspect` return so
//! the same code runs against the headless engine and the live app.
//!
//! *Errors* are states no editor should leave a timeline in (overlapping clips, zero-length clips,
//! transitions that point at clips that are gone). An edit that introduces one is rolled back.
//! *Warnings* are things a person reviewing the timeline wants to see (a gap that flashes black,
//! picture and sound that drifted apart, a clip running past the end of its media).

use std::collections::{BTreeMap, HashMap, HashSet};

use serde_json::{Value, json};

pub const TICKS_PER_SECOND: i64 = 254_016_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Error,
    Warning,
    Info,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Info => "info",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Issue {
    pub severity: Severity,
    pub code: &'static str,
    pub message: String,
    pub track: Option<String>,
    pub clip: Option<i64>,
    /// Where on the timeline, in sequence frames.
    pub frame: Option<i64>,
}

impl Issue {
    fn new(severity: Severity, code: &'static str, message: String) -> Self {
        Self { severity, code, message, track: None, clip: None, frame: None }
    }
    fn on(mut self, track: &str, clip: Option<i64>, frame: Option<i64>) -> Self {
        self.track = Some(track.to_string());
        self.clip = clip;
        self.frame = frame;
        self
    }
    /// Identity of an issue across edits: the same problem on the same clip is the same issue.
    pub fn fingerprint(&self) -> String {
        format!("{}|{}|{}", self.code, self.track.as_deref().unwrap_or(""), self.clip.map_or(String::new(), |c| c.to_string()))
    }
    pub fn to_json(&self) -> Value {
        json!({"severity": self.severity.as_str(), "code": self.code, "message": self.message, "track": self.track, "clip": self.clip, "frame": self.frame})
    }
}

/// One clip as the review and the change report see it.
#[derive(Clone, Debug, PartialEq)]
pub struct ClipInfo {
    pub id: i64,
    pub track: String,
    pub video: bool,
    pub name: String,
    pub item: Option<i64>,
    pub start: i64,
    pub end: i64,
    pub source_in: i64,
    pub source_out: i64,
    /// Link group id: picture and sound that were linked share it.
    pub link: Option<i64>,
    pub enabled: bool,
}

/// Project item id → (type, duration in ticks), read from `project.inspect`.
pub type MediaDurations = HashMap<i64, (String, i64)>;

pub fn media_durations(project: &Value) -> MediaDurations {
    fn walk(node: &Value, out: &mut MediaDurations, depth: usize) {
        if depth > 64 {
            return;
        }
        if let (Some(item), Some(ty)) = (node.get("item").and_then(Value::as_i64), node.get("type").and_then(Value::as_str)) {
            out.insert(item, (ty.to_string(), node.get("duration").and_then(Value::as_i64).unwrap_or(0)));
        }
        for c in node.get("children").and_then(Value::as_array).into_iter().flatten() {
            walk(c, out, depth + 1);
        }
    }
    let mut out = HashMap::new();
    if let Some(root) = project.get("root") {
        walk(root, &mut out, 0);
    }
    out
}

/// Ticks in one sequence frame (`None` when the frame rate is missing or zero).
pub fn frame_ticks(seq: &Value) -> Option<i64> {
    let fr = seq.get("settings")?.get("frame_rate")?;
    let (num, den) = (fr.get("num")?.as_i64()?, fr.get("den")?.as_i64()?);
    if num <= 0 || den <= 0 {
        return None;
    }
    i64::try_from(i128::from(TICKS_PER_SECOND) * i128::from(den) / i128::from(num)).ok().filter(|t| *t > 0)
}

fn frames(ticks: i64, frame_ticks: Option<i64>) -> Option<i64> {
    frame_ticks.map(|f| ticks / f)
}

/// Every clip of the sequence, video tracks first.
pub fn clips(seq: &Value) -> Vec<ClipInfo> {
    let mut out = Vec::new();
    for (key, video) in [("video", true), ("audio", false)] {
        for t in seq.get(key).and_then(Value::as_array).into_iter().flatten() {
            let track = t.get("name").and_then(Value::as_str).unwrap_or("?").to_string();
            for i in t.get("items").and_then(Value::as_array).into_iter().flatten() {
                let (Some(id), Some(start), Some(end)) =
                    (i.get("clip").and_then(Value::as_i64), i.get("start").and_then(Value::as_i64), i.get("end").and_then(Value::as_i64))
                else {
                    continue;
                };
                out.push(ClipInfo {
                    id,
                    track: track.clone(),
                    video,
                    name: i.get("name").and_then(Value::as_str).unwrap_or("").to_string(),
                    item: i.get("item").and_then(Value::as_i64),
                    start,
                    end,
                    source_in: i.get("sourceIn").and_then(Value::as_i64).unwrap_or(0),
                    source_out: i.get("sourceOut").and_then(Value::as_i64).unwrap_or(0),
                    link: i.get("link").and_then(Value::as_i64),
                    enabled: i.get("enabled").and_then(Value::as_bool).unwrap_or(true),
                });
            }
        }
    }
    out
}

pub fn check(seq: &Value, media: &MediaDurations) -> Vec<Issue> {
    let ft = frame_ticks(seq);
    let mut issues = Vec::new();
    let mut by_id: HashMap<i64, ClipInfo> = HashMap::new();
    for c in clips(seq) {
        by_id.insert(c.id, c);
    }
    let mut max_end = 0i64;
    let mut primary_seen = [false, false];

    for (key, video) in [("video", true), ("audio", false)] {
        for t in seq.get(key).and_then(Value::as_array).into_iter().flatten() {
            let track = t.get("name").and_then(Value::as_str).unwrap_or("?");
            let mut items: Vec<&Value> = t.get("items").and_then(Value::as_array).into_iter().flatten().collect();
            items.sort_by_key(|i| i.get("start").and_then(Value::as_i64).unwrap_or(0));
            let track_ids: HashSet<i64> = items.iter().filter_map(|i| i.get("clip").and_then(Value::as_i64)).collect();
            let primary = !items.is_empty() && !std::mem::replace(&mut primary_seen[usize::from(!video)], true);
            let mut prev_end: Option<i64> = None;
            for i in &items {
                let id = i.get("clip").and_then(Value::as_i64);
                let (Some(start), Some(end)) = (i.get("start").and_then(Value::as_i64), i.get("end").and_then(Value::as_i64)) else {
                    issues.push(Issue::new(Severity::Error, "malformed_clip", format!("a clip on {track} has no readable start and end")).on(track, id, None));
                    continue;
                };
                let at = frames(start, ft);
                if start < 0 {
                    issues.push(
                        Issue::new(Severity::Error, "negative_start", format!("clip {} on {track} starts before the sequence", id.unwrap_or(-1)))
                            .on(track, id, at),
                    );
                }
                if end <= start {
                    issues.push(
                        Issue::new(Severity::Error, "bad_range", format!("clip {} on {track} has no length (start {start}, end {end})", id.unwrap_or(-1)))
                            .on(track, id, at),
                    );
                }
                if let Some(d) = i.get("duration").and_then(Value::as_i64)
                    && d != end.saturating_sub(start)
                {
                    issues.push(
                        Issue::new(Severity::Error, "duration_mismatch", format!("clip {} on {track}: duration {d} is not end - start", id.unwrap_or(-1)))
                            .on(track, id, at),
                    );
                }
                if let (Some(si), Some(so)) = (i.get("sourceIn").and_then(Value::as_i64), i.get("sourceOut").and_then(Value::as_i64))
                    && so <= si
                {
                    issues.push(
                        Issue::new(Severity::Error, "bad_source_range", format!("clip {} on {track} plays no source (in {si}, out {so})", id.unwrap_or(-1)))
                            .on(track, id, at),
                    );
                }
                if let Some(sp) = i.get("speed").and_then(Value::as_f64)
                    && (!sp.is_finite() || sp <= 0.0)
                {
                    issues.push(Issue::new(Severity::Error, "bad_speed", format!("clip {} on {track} has speed {sp}", id.unwrap_or(-1))).on(track, id, at));
                }
                if let Some(pe) = prev_end {
                    if start < pe {
                        issues.push(
                            Issue::new(
                                Severity::Error,
                                "overlap",
                                format!(
                                    "clip {} on {track} overlaps the clip before it by {} frame(s)",
                                    id.unwrap_or(-1),
                                    frames(pe - start, ft).map_or_else(|| "?".into(), |f| f.to_string())
                                ),
                            )
                            .on(track, id, at),
                        );
                    } else if start > pe && primary {
                        let gap = start - pe;
                        let msg = match frames(gap, ft) {
                            Some(0) => format!("sub-frame gap of {gap} ticks before clip {} on {track}", id.unwrap_or(-1)),
                            Some(f) => format!("{f}-frame gap before clip {} on {track}", id.unwrap_or(-1)),
                            None => format!("gap before clip {} on {track}", id.unwrap_or(-1)),
                        };
                        issues.push(Issue::new(Severity::Warning, "gap", msg).on(track, id, frames(pe, ft)));
                    }
                } else if start > 0 && primary {
                    issues.push(Issue::new(Severity::Warning, "leading_gap", format!("{track} is empty until frame {}", frames(start, ft).unwrap_or(0))).on(
                        track,
                        id,
                        Some(0),
                    ));
                }
                prev_end = Some(prev_end.map_or(end, |p| p.max(end)));
                max_end = max_end.max(end);

                if let (Some(item), Some(so)) = (i.get("item").and_then(Value::as_i64), i.get("sourceOut").and_then(Value::as_i64))
                    && let Some((ty, dur)) = media.get(&item)
                    && matches!(ty.as_str(), "Movie" | "Audio")
                    && *dur > 0
                    && so > dur.saturating_add(ft.unwrap_or(0))
                {
                    issues.push(
                        Issue::new(
                            Severity::Warning,
                            "past_media_end",
                            format!(
                                "clip {} on {track} plays past the end of its media ({} frame(s) of nothing)",
                                id.unwrap_or(-1),
                                frames(so - dur, ft).unwrap_or(0)
                            ),
                        )
                        .on(track, id, at),
                    );
                }
                if i.get("enabled").and_then(Value::as_bool) == Some(false) {
                    issues.push(Issue::new(Severity::Info, "disabled_clip", format!("clip {} on {track} is disabled", id.unwrap_or(-1))).on(track, id, at));
                }
            }
            for tr in t.get("transitions").and_then(Value::as_array).into_iter().flatten() {
                for end in ["from", "to"] {
                    if let Some(c) = tr.get(end).and_then(Value::as_i64)
                        && !track_ids.contains(&c)
                    {
                        let tid = tr.get("id").and_then(Value::as_i64).unwrap_or(-1);
                        issues.push(
                            Issue::new(
                                Severity::Error,
                                "dangling_transition",
                                format!("transition {tid} on {track} refers to clip {c}, which is not on the track"),
                            )
                            .on(track, Some(c), None),
                        );
                    }
                }
            }
        }
    }
    if let Some(d) = seq.get("duration").and_then(Value::as_i64)
        && d != max_end
    {
        issues.push(Issue::new(Severity::Error, "sequence_duration", format!("the sequence says it is {d} ticks long but its last clip ends at {max_end}")));
    }
    // picture and sound in one link group (`link` is a group id shared by the partners) must stay together
    let mut groups: BTreeMap<i64, Vec<&ClipInfo>> = BTreeMap::new();
    for c in by_id.values() {
        if let Some(g) = c.link {
            groups.entry(g).or_default().push(c);
        }
    }
    for members in groups.values() {
        let Some(anchor) = members.iter().find(|c| c.video).or(members.first()) else { continue };
        for other in members.iter().filter(|c| c.id != anchor.id && (c.start != anchor.start || c.end != anchor.end)) {
            issues.push(
                Issue::new(
                    Severity::Warning,
                    "av_out_of_sync",
                    format!(
                        "linked clips {} ({}) and {} ({}) no longer line up: start differs by {} frame(s), end by {}",
                        anchor.id,
                        anchor.track,
                        other.id,
                        other.track,
                        frames((anchor.start - other.start).abs(), ft).unwrap_or(0),
                        frames((anchor.end - other.end).abs(), ft).unwrap_or(0)
                    ),
                )
                .on(&other.track, Some(other.id), frames(other.start, ft)),
            );
        }
    }
    issues.sort_by(|a, b| a.severity.cmp(&b.severity).then(a.frame.cmp(&b.frame)).then(a.code.cmp(b.code)));
    issues
}

/// Issues in `after` that were not already in `before` (same code on the same clip and track).
pub fn new_issues<'a>(before: &[Issue], after: &'a [Issue]) -> Vec<&'a Issue> {
    let known: HashSet<String> = before.iter().map(Issue::fingerprint).collect();
    after.iter().filter(|i| !known.contains(&i.fingerprint())).collect()
}

/// Clip id → info, for diffing a timeline before and after an edit.
pub fn by_id(seq: &Value) -> BTreeMap<i64, ClipInfo> {
    clips(seq).into_iter().map(|c| (c.id, c)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seq(video: Value, audio: Value, duration: i64) -> Value {
        json!({"duration": duration, "settings": {"frame_rate": {"num": 24000, "den": 1001}}, "video": video, "audio": audio})
    }
    fn item(clip: i64, start: i64, end: i64) -> Value {
        json!({"clip": clip, "start": start, "end": end, "duration": end - start, "sourceIn": 0, "sourceOut": end - start, "speed": 1.0, "item": 5, "name": "c"})
    }
    fn track(items: Vec<Value>) -> Value {
        json!({"name": "Video 1", "id": 1, "items": items, "transitions": []})
    }
    fn codes(seq: &Value) -> Vec<&'static str> {
        check(seq, &HashMap::new()).iter().map(|i| i.code).collect()
    }
    const F: i64 = 10_594_584_000; // one frame at 23.976

    #[test]
    fn frame_ticks_is_exact_for_ntsc() {
        assert_eq!(frame_ticks(&seq(json!([]), json!([]), 0)), Some(F));
        assert_eq!(frame_ticks(&json!({"settings": {"frame_rate": {"num": 0, "den": 1}}})), None);
    }

    #[test]
    fn a_clean_butted_timeline_has_no_issues() {
        let s = seq(json!([track(vec![item(1, 0, 10 * F), item(2, 10 * F, 20 * F)])]), json!([]), 20 * F);
        assert!(check(&s, &HashMap::new()).is_empty());
    }

    #[test]
    fn finds_overlaps_gaps_and_zero_length() {
        let s = seq(json!([track(vec![item(1, 0, 10 * F), item(2, 8 * F, 20 * F), item(3, 25 * F, 25 * F)])]), json!([]), 25 * F);
        let c = codes(&s);
        assert!(c.contains(&"overlap"), "{c:?}");
        assert!(c.contains(&"bad_range"), "{c:?}");
        let s = seq(json!([track(vec![item(1, 0, 10 * F), item(2, 15 * F, 20 * F)])]), json!([]), 20 * F);
        let issues = check(&s, &HashMap::new());
        assert_eq!(issues.len(), 1);
        assert_eq!((issues[0].code, issues[0].severity, issues[0].frame), ("gap", Severity::Warning, Some(10)));
        assert!(issues[0].message.starts_with("5-frame gap"));
    }

    #[test]
    fn upper_track_gaps_are_not_flagged() {
        let top = json!({"name": "Video 2", "id": 2, "items": [item(9, 5 * F, 8 * F)], "transitions": []});
        let s = seq(json!([track(vec![item(1, 0, 10 * F)]), top]), json!([]), 10 * F);
        assert!(check(&s, &HashMap::new()).is_empty());
    }

    #[test]
    fn sequence_duration_and_dangling_references() {
        let mut t = track(vec![item(1, 0, 10 * F)]);
        t["transitions"] = json!([{"id": 7, "from": 1, "to": 99}]);
        let s = seq(json!([t]), json!([]), 3 * F);
        let c = codes(&s);
        assert!(c.contains(&"sequence_duration") && c.contains(&"dangling_transition"), "{c:?}");
    }

    #[test]
    fn linked_clips_that_drift_are_reported_once() {
        let mut v = item(1, 0, 10 * F);
        v["link"] = json!(77);
        let mut a = item(2, F, 10 * F);
        a["link"] = json!(77);
        let audio = json!([{"name": "Audio 1", "id": 3, "items": [a], "transitions": []}]);
        let s = seq(json!([track(vec![v])]), audio, 10 * F);
        let c = codes(&s);
        assert_eq!(c.iter().filter(|c| **c == "av_out_of_sync").count(), 1, "{c:?}");
    }

    #[test]
    fn media_end_and_new_issue_diffing() {
        let media: MediaDurations = [(5, ("Movie".to_string(), 5 * F))].into_iter().collect();
        let s = seq(json!([track(vec![item(1, 0, 10 * F)])]), json!([]), 10 * F);
        let after = check(&s, &media);
        assert_eq!(after.iter().map(|i| i.code).collect::<Vec<_>>(), vec!["past_media_end"]);
        assert!(new_issues(&after, &after).is_empty());
        assert_eq!(new_issues(&[], &after).len(), 1);
    }

    #[test]
    fn hostile_json_does_not_panic() {
        for v in [json!(null), json!({"video": 5}), json!({"video": [{"items": [{"clip": "x"}]}]}), json!({"video": [null], "audio": [[]], "duration": "z"})] {
            let _ = check(&v, &HashMap::new());
            let _ = clips(&v);
        }
    }
}
