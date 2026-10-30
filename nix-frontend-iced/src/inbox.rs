//! Where backend events wait for the next frame.
//!
//! A build produces hundreds of events a second, but only the newest of most
//! of them matters: every event carries a full snapshot (progress, what is
//! running, the status line), so an older one is out of date the moment a
//! newer one exists. Sending each as a message would make the UI work through
//! them all in order, and once it fell behind it would be showing the past.
//!
//! So the stream task *writes into this inbox* instead, and the UI *reads it
//! once per frame*:
//!
//! * the snapshot, phase and status line are overwritten, so only the newest
//!   survives;
//! * output lines are appended, since a log is a sequence, and arrive in one
//!   piece (the UI adds them to its view in one go);
//! * an operation ending is never overwritten by the next one starting.
//!
//! Writing is cheap and never blocks the UI, however fast events come.

use std::sync::Mutex;

use nix_backend_client::{BuildItem, BuildStats, EventPhase, OperationEvent, OperationKind, RebuildMode};

/// The most output lines kept for one operation between two frames. A frame
/// that long is already unreadable; the rest is counted instead.
const MAX_LINES: usize = 5_000;

/// How an operation ended.
#[derive(Debug, Clone, PartialEq)]
pub enum End {
    Finished,
    /// With the full error text.
    Failed(String),
}

/// Everything that arrived for one operation since the UI last looked.
#[derive(Debug, Clone)]
pub struct Segment {
    pub kind: OperationKind,
    pub mode: RebuildMode,
    /// Where the operation had got to.
    pub phase: EventPhase,
    /// New output, oldest first.
    pub lines: Vec<String>,
    /// Output lines dropped because there were more than [`MAX_LINES`].
    pub skipped: usize,
    /// The newest status line, if any arrived.
    pub status: Option<String>,
    /// The newest complete picture of what is running.
    pub snapshot: Option<(BuildStats, Vec<BuildItem>)>,
    /// The furthest progress reported (0..1): nix's totals only grow, so it
    /// never goes backwards.
    pub fraction: Option<f64>,
    pub end: Option<End>,
}

impl Segment {
    fn new(kind: OperationKind, mode: RebuildMode) -> Self {
        Self {
            kind,
            mode,
            phase: EventPhase::Started,
            lines: Vec::new(),
            skipped: 0,
            status: None,
            snapshot: None,
            fraction: None,
            end: None,
        }
    }

    fn absorb(&mut self, op: OperationEvent) {
        let Some(event) = op.event else { return };

        self.phase = event.phase();

        let message = nix_backend_client::strip_ansi(&event.message);
        if !message.is_empty() {
            self.status = message.lines().next().map(str::to_owned);
            self.lines.extend(message.lines().map(str::to_owned));
            // Trimmed with some slack, so a flood costs one move per MAX_LINES lines, not one per line.
            if self.lines.len() > MAX_LINES * 2 {
                let excess = self.lines.len() - MAX_LINES;
                self.lines.drain(..excess);
                self.skipped += excess;
            }
        }

        if let Some(stats) = event.stats {
            self.snapshot = Some((stats, event.active_items));
        }
        if event.fraction_done >= 0.0 {
            self.fraction = Some(self.fraction.map_or(event.fraction_done, |seen| seen.max(event.fraction_done)));
        }

        match self.phase {
            EventPhase::Finished => self.end = Some(End::Finished),
            EventPhase::Failed => self.end = Some(End::Failed(message)),
            _ => {}
        }
    }

    /// Keeps only the newest [`MAX_LINES`] lines.
    fn trimmed(mut self) -> Self {
        if self.lines.len() > MAX_LINES {
            let excess = self.lines.len() - MAX_LINES;
            self.lines.drain(..excess);
            self.skipped += excess;
        }
        self
    }
}

/// Shared between the task that reads the backend's stream and the UI.
#[derive(Debug, Default)]
pub struct Inbox {
    segments: Mutex<Vec<Segment>>,
}

impl Inbox {
    fn segments(&self) -> std::sync::MutexGuard<'_, Vec<Segment>> {
        self.segments.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Files an event. Returns whether the inbox was empty before it, which
    /// is when the UI needs waking: if it wasn't, the UI has already been told.
    pub fn push(&self, op: OperationEvent) -> bool {
        if op.event.is_none() {
            return false;
        }
        let mut segments = self.segments();
        let was_empty = segments.is_empty();

        // The operation before this one is over, or this is a different
        // operation altogether: start a new segment so neither is lost.
        let continues =
            segments.last().is_some_and(|last| last.end.is_none() && last.kind == op.kind() && last.mode == op.mode());
        if !continues {
            segments.push(Segment::new(op.kind(), op.mode()));
        }
        segments.last_mut().expect("a segment was just ensured").absorb(op);
        was_empty
    }

    /// Everything that arrived since the last call, oldest operation first.
    pub fn take(&self) -> Vec<Segment> {
        std::mem::take(&mut *self.segments()).into_iter().map(Segment::trimmed).collect()
    }

    pub fn has_pending(&self) -> bool {
        !self.segments().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use nix_backend_client::proto::ProgressEvent;

    use super::*;

    fn event(
        kind: OperationKind,
        phase: EventPhase,
        message: &str,
        fraction: f64,
        snapshot: Option<i64>,
    ) -> OperationEvent {
        OperationEvent {
            kind: kind as i32,
            mode: RebuildMode::BuildOnly as i32,
            event: Some(ProgressEvent {
                phase: phase as i32,
                message: message.to_owned(),
                is_error: false,
                fraction_done: fraction,
                active_items: snapshot.map(|_| vec![BuildItem::default()]).unwrap_or_default(),
                stats: snapshot.map(|builds_done| BuildStats { builds_done, ..Default::default() }),
            }),
        }
    }

    fn build(message: &str, fraction: f64, snapshot: Option<i64>) -> OperationEvent {
        event(OperationKind::Rebuild, EventPhase::Build, message, fraction, snapshot)
    }

    #[test]
    fn only_the_newest_snapshot_survives() {
        let inbox = Inbox::default();
        for done in 1..=500 {
            inbox.push(build("", -1.0, Some(done)));
        }
        let segments = inbox.take();
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].snapshot.as_ref().unwrap().0.builds_done, 500);
    }

    #[test]
    fn output_lines_all_arrive_in_order() {
        let inbox = Inbox::default();
        inbox.push(build("one", -1.0, Some(1)));
        inbox.push(build("two\nthree", -1.0, None));
        inbox.push(build("four", -1.0, Some(2)));
        let segment = inbox.take().remove(0);
        assert_eq!(segment.lines, ["one", "two", "three", "four"]);
        assert_eq!(segment.status.as_deref(), Some("four"));
    }

    #[test]
    fn an_event_without_a_snapshot_keeps_the_one_before() {
        let inbox = Inbox::default();
        inbox.push(build("a", -1.0, Some(7)));
        inbox.push(build("b", -1.0, None));
        assert_eq!(inbox.take()[0].snapshot.as_ref().unwrap().0.builds_done, 7);
    }

    #[test]
    fn progress_only_moves_forward() {
        let inbox = Inbox::default();
        inbox.push(build("", 0.5, None));
        inbox.push(build("", 0.3, None));
        inbox.push(build("", -1.0, None));
        assert_eq!(inbox.take()[0].fraction, Some(0.5));
    }

    #[test]
    fn the_ui_is_woken_once_per_batch() {
        let inbox = Inbox::default();
        assert!(inbox.push(build("a", -1.0, None)), "the first event wakes the UI");
        assert!(!inbox.push(build("b", -1.0, None)), "it has already been told");
        assert!(inbox.has_pending());

        inbox.take();
        assert!(!inbox.has_pending());
        assert!(inbox.push(build("c", -1.0, None)), "after a read, the next event wakes it again");
    }

    #[test]
    fn an_operation_ending_is_never_overwritten_by_the_next_starting() {
        let inbox = Inbox::default();
        inbox.push(build("working", 0.5, Some(1)));
        inbox.push(event(OperationKind::Rebuild, EventPhase::Finished, "Rebuild complete", 1.0, None));
        inbox.push(event(OperationKind::UpdateFlake, EventPhase::Started, "Updating", -1.0, None));

        let segments = inbox.take();
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].end, Some(End::Finished));
        assert_eq!(segments[0].kind, OperationKind::Rebuild);
        assert_eq!(segments[1].kind, OperationKind::UpdateFlake);
        assert_eq!(segments[1].end, None);
    }

    #[test]
    fn a_failure_keeps_its_full_text() {
        let inbox = Inbox::default();
        inbox.push(event(OperationKind::Rebuild, EventPhase::Failed, "Build failed:\nline two", -1.0, None));
        assert_eq!(inbox.take()[0].end, Some(End::Failed("Build failed:\nline two".to_owned())));
    }

    #[test]
    fn colour_codes_are_stripped_off_the_ui_thread() {
        let inbox = Inbox::default();
        inbox.push(build("\u{1b}[31;1merror:\u{1b}[0m boom", -1.0, None));
        assert_eq!(inbox.take()[0].lines, ["error: boom"]);
    }

    #[test]
    fn a_flood_keeps_the_newest_lines_and_counts_the_rest() {
        let inbox = Inbox::default();
        let total = MAX_LINES * 3;
        for i in 0..total {
            inbox.push(build(&format!("line {i}"), -1.0, None));
        }
        let segment = inbox.take().remove(0);
        assert_eq!(segment.lines.len(), MAX_LINES);
        assert_eq!(segment.lines.last().unwrap(), &format!("line {}", total - 1));
        assert_eq!(segment.skipped + segment.lines.len(), total);
    }

    #[test]
    fn events_without_a_body_are_ignored() {
        let inbox = Inbox::default();
        assert!(!inbox.push(OperationEvent { kind: 0, mode: 0, event: None }));
        assert!(!inbox.has_pending());
    }
}
