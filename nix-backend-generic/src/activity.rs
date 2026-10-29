use std::time::Instant;

use serde_json::Value;

use crate::proto::{BuildItem, BuildItemKind, BuildItemStatus, BuildStats};

// `nix --log-format internal-json`'s activity/result type codes. Not part of
// any stable public API (nix itself calls this format "internal"), but
// they've been unchanged across the 2.1x series and are confirmed empirically
// against the `nix` binary this daemon shells out to - see the git history
// of this file for how that was verified.
const ACT_FILE_TRANSFER: i64 = 101;
const ACT_REALISE: i64 = 102;
const ACT_COPY_PATHS: i64 = 103;
const ACT_BUILDS: i64 = 104;
const ACT_BUILD: i64 = 105;

const RESULT_BUILD_LOG_LINE: i64 = 101;
const RESULT_PROGRESS: i64 = 105;
const RESULT_SET_EXPECTED: i64 = 106;

// Only recently-finished builds/downloads are kept, so the snapshot stays
// small during a big rebuild instead of accumulating every path ever seen.
const MAX_FINISHED: usize = 6;

#[derive(Clone, Copy, PartialEq, Eq)]
enum TrackedKind {
    Build,
    Download,
    BuildsContainer,
    CopiesContainer,
    Realise,
}

struct TrackedActivity {
    kind: TrackedKind,
    name: String,
    status: BuildItemStatus,
    bytes_done: f64,
    bytes_expected: f64,
    bytes_per_sec: f64,
    last_sample: Option<(Instant, f64)>,
}

impl TrackedActivity {
    fn new(kind: TrackedKind, name: String) -> Self {
        Self {
            kind,
            name,
            status: BuildItemStatus::BuildItemRunning,
            bytes_done: 0.0,
            bytes_expected: 0.0,
            bytes_per_sec: 0.0,
            last_sample: None,
        }
    }

    fn update_bytes(&mut self, done: f64, expected: f64) {
        let now = Instant::now();
        if let Some((t, prev_done)) = self.last_sample {
            let dt = now.duration_since(t).as_secs_f64();
            if dt > 0.05 {
                self.bytes_per_sec = (done - prev_done).max(0.0) / dt;
                self.last_sample = Some((now, done));
            }
        } else {
            self.last_sample = Some((now, done));
        }
        self.bytes_done = done;
        if expected > 0.0 {
            self.bytes_expected = expected;
        }
    }
}

/// Extracts the text a line should contribute to the plain scrolling log,
/// independent of whatever activity bookkeeping it also triggers.
pub fn extract_message(value: &Value) -> Option<String> {
    match value.get("action").and_then(Value::as_str)? {
        "msg" => value.get("msg").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned),
        "start" => value.get("text").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned),
        "result" if value.get("type").and_then(Value::as_i64) == Some(RESULT_BUILD_LOG_LINE) => value
            .get("fields")
            .and_then(Value::as_array)
            .and_then(|f| f.first())
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned),
        _ => None,
    }
}

/// Turns a nix store path or drv path (".../<hash>-name[.drv]") or a URL
/// into just the "name" part a human would recognise.
fn display_name(raw: &str) -> String {
    let base = raw.rsplit('/').next().unwrap_or(raw);
    let base = base.strip_suffix(".drv").unwrap_or(base);
    if base.len() > 33 && base.as_bytes()[32] == b'-' && base[..32].bytes().all(|c| c.is_ascii_alphanumeric()) {
        base[33..].to_owned()
    } else {
        base.to_owned()
    }
}

/// Maintains a live picture of what one `nix` subprocess is currently
/// building/downloading, built entirely from its `--log-format
/// internal-json` activity stream. Scoped to a single subprocess run - nix
/// restarts activity ids each time, so a fresh tracker per `run_streamed`
/// call is both simpler and correct.
#[derive(Default)]
pub struct ActivityTracker {
    activities: std::collections::HashMap<i64, TrackedActivity>,
    finished_order: Vec<i64>,
    stats: BuildStats,
    downloaded_bytes_total: f64,
}

impl ActivityTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn handle_json(&mut self, value: &Value) {
        match value.get("action").and_then(Value::as_str) {
            Some("start") => self.handle_start(value),
            Some("result") => self.handle_result(value),
            Some("stop") => self.handle_stop(value),
            _ => {}
        }
    }

    fn handle_start(&mut self, value: &Value) {
        let Some(id) = value.get("id").and_then(Value::as_i64) else { return };
        let ty = value.get("type").and_then(Value::as_i64).unwrap_or(-1);
        let fields = value.get("fields").and_then(Value::as_array);
        let first_field = fields.and_then(|f| f.first()).and_then(Value::as_str);

        let tracked = match ty {
            ACT_BUILD => TrackedActivity::new(TrackedKind::Build, display_name(first_field.unwrap_or_default())),
            ACT_FILE_TRANSFER => TrackedActivity::new(TrackedKind::Download, display_name(first_field.unwrap_or_default())),
            ACT_BUILDS => TrackedActivity::new(TrackedKind::BuildsContainer, String::new()),
            ACT_COPY_PATHS => TrackedActivity::new(TrackedKind::CopiesContainer, String::new()),
            ACT_REALISE => TrackedActivity::new(TrackedKind::Realise, String::new()),
            _ => return,
        };
        self.activities.insert(id, tracked);
    }

    fn handle_stop(&mut self, value: &Value) {
        let Some(id) = value.get("id").and_then(Value::as_i64) else { return };
        let Some(act) = self.activities.get_mut(&id) else { return };

        match act.kind {
            TrackedKind::Build | TrackedKind::Download => {
                act.status = BuildItemStatus::BuildItemDone;
                if act.bytes_expected > 0.0 {
                    act.bytes_done = act.bytes_expected;
                }
                act.bytes_per_sec = 0.0;
                self.finished_order.push(id);
                while self.finished_order.len() > MAX_FINISHED {
                    let old = self.finished_order.remove(0);
                    self.activities.remove(&old);
                }
            }
            _ => {
                self.activities.remove(&id);
            }
        }
    }

    fn handle_result(&mut self, value: &Value) {
        let Some(id) = value.get("id").and_then(Value::as_i64) else { return };
        let ty = value.get("type").and_then(Value::as_i64).unwrap_or(-1);
        let fields = value.get("fields").and_then(Value::as_array);

        match ty {
            RESULT_PROGRESS => {
                let done = fields.and_then(|f| f.first()).and_then(Value::as_f64).unwrap_or(0.0);
                let expected = fields.and_then(|f| f.get(1)).and_then(Value::as_f64).unwrap_or(0.0);
                let running = fields.and_then(|f| f.get(2)).and_then(Value::as_i64).unwrap_or(0);
                let failed = fields.and_then(|f| f.get(3)).and_then(Value::as_i64).unwrap_or(0);

                let Some(act) = self.activities.get_mut(&id) else { return };
                match act.kind {
                    TrackedKind::Download => {
                        self.downloaded_bytes_total += (done - act.bytes_done).max(0.0);
                        act.update_bytes(done, expected.max(act.bytes_expected));
                    }
                    TrackedKind::Build => act.update_bytes(done, expected.max(act.bytes_expected)),
                    TrackedKind::BuildsContainer => {
                        self.stats.builds_done = done as i64;
                        self.stats.builds_expected = expected as i64;
                        self.stats.builds_running = running;
                        self.stats.builds_failed = failed;
                    }
                    TrackedKind::CopiesContainer => {
                        self.stats.copies_done = done as i64;
                        self.stats.copies_expected = expected as i64;
                        self.stats.copies_running = running;
                        self.stats.copies_failed = failed;
                    }
                    TrackedKind::Realise => {}
                }
            }
            RESULT_SET_EXPECTED => {
                if self.activities.get(&id).map(|a| a.kind) != Some(TrackedKind::Realise) {
                    return;
                }
                let unit = fields.and_then(|f| f.first()).and_then(Value::as_i64).unwrap_or(-1);
                let amount = fields.and_then(|f| f.get(1)).and_then(Value::as_f64).unwrap_or(0.0);
                if unit == ACT_FILE_TRANSFER {
                    self.stats.download_bytes_expected = amount as i64;
                }
            }
            _ => {}
        }
    }

    /// Current state: every running build/download, plus the last few that
    /// just finished, newest-first within each group; and the aggregate
    /// counts nix itself reports for the whole operation.
    pub fn snapshot(&self) -> (Vec<BuildItem>, BuildStats) {
        let mut items: Vec<BuildItem> = self
            .activities
            .iter()
            .filter_map(|(&id, a)| {
                let kind = match a.kind {
                    TrackedKind::Build => BuildItemKind::BuildItemBuild,
                    TrackedKind::Download => BuildItemKind::BuildItemDownload,
                    _ => return None,
                };
                Some(BuildItem {
                    id: id as u64,
                    kind: kind as i32,
                    status: a.status as i32,
                    name: a.name.clone(),
                    bytes_done: a.bytes_done,
                    bytes_expected: a.bytes_expected,
                    bytes_per_sec: a.bytes_per_sec,
                })
            })
            .collect();
        items.sort_by_key(|i| (i.status != BuildItemStatus::BuildItemRunning as i32, i.id));

        let mut stats = self.stats.clone();
        stats.download_bytes_done = self.downloaded_bytes_total as i64;
        (items, stats)
    }
}

/// Overall progress across both builds and substitutions, when nix has told
/// us how many of each to expect; -1 (indeterminate) otherwise.
pub fn fraction_done(stats: &BuildStats) -> f64 {
    let done = stats.builds_done + stats.copies_done;
    let expected = stats.builds_expected + stats.copies_expected;
    if expected > 0 {
        (done as f64 / expected as f64).clamp(0.0, 1.0)
    } else {
        -1.0
    }
}
