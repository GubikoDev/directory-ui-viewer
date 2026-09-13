//! Bounded, best-effort notifications. Work/cache state is authoritative.
use crate::domain::model::{EventKind, WorkEvent, WorkRecord};
use std::sync::mpsc::{self, Receiver, SyncSender};
pub trait EventSink: Send + Sync {
    /// Must not wait for a consumer, perform I/O, or call back into a service.
    fn try_emit(&self, event: WorkEvent) -> bool;
}
pub struct DiscardEvents;
impl EventSink for DiscardEvents {
    fn try_emit(&self, _: WorkEvent) -> bool {
        false
    }
}
pub struct EventChannel {
    sender: SyncSender<WorkEvent>,
}
impl EventChannel {
    pub fn bounded(capacity: usize) -> (Self, Receiver<WorkEvent>) {
        let (sender, receiver) = mpsc::sync_channel(capacity);
        (Self { sender }, receiver)
    }
}
impl EventSink for EventChannel {
    fn try_emit(&self, event: WorkEvent) -> bool {
        self.sender.try_send(event).is_ok()
    }
}
/// One instance per task; no global completed-task map or retry queue.
#[derive(Default)]
pub struct TaskEvents {
    last_attempt_ms: Option<u64>,
    last_sequence: Option<u64>,
    terminal: bool,
}
impl TaskEvents {
    pub fn publish(
        &mut self,
        work: &WorkRecord,
        kind: EventKind,
        now: u64,
        sink: &dyn EventSink,
    ) -> bool {
        if self.terminal || self.last_sequence.is_some_and(|s| s >= work.sequence) {
            return false;
        }
        let terminal = work.phase.terminal();
        if !terminal
            && self
                .last_attempt_ms
                .is_some_and(|last| now.saturating_sub(last) < 200)
        {
            return false;
        }
        self.last_attempt_ms = Some(now);
        self.last_sequence = Some(work.sequence);
        self.terminal = terminal;
        sink.try_emit(WorkEvent {
            protocol_version: 1,
            session_id: work.session_id.clone(),
            generation: work.generation,
            task_id: work.task_id.clone(),
            sequence: work.sequence,
            kind: if terminal { EventKind::Terminal } else { kind },
            observed_at: work.observed_at.clone(),
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{Issues, Operation, Phase};
    fn work(sequence: u64, phase: Phase) -> WorkRecord {
        WorkRecord {
            session_id: "s".into(),
            generation: 1,
            task_id: "t".into(),
            operation: Operation::ScanUsage,
            target_id: "root".into(),
            phase,
            wait_reason: None,
            sequence,
            processed_entries: sequence.into(),
            processed_directories: 0_u64.into(),
            observed_at: "2026-09-13T00:00:00Z".into(),
            issues: Issues::default(),
        }
    }
    #[test]
    fn progress_is_coalesced_to_five_per_second_and_terminal_bypasses_throttle() {
        let (sink, rx) = EventChannel::bounded(10);
        let mut events = TaskEvents::default();
        for ms in 0..1000 {
            events.publish(
                &work(ms + 1, Phase::Running),
                EventKind::UsageChanged,
                ms,
                &sink,
            );
        }
        assert_eq!(
            rx.try_iter().map(|e| e.sequence).collect::<Vec<_>>(),
            vec![1, 201, 401, 601, 801]
        );
        assert!(events.publish(
            &work(1001, Phase::Completed),
            EventKind::Progress,
            999,
            &sink
        ));
        assert_eq!(rx.try_recv().unwrap().kind, EventKind::Terminal);
        assert!(!events.publish(
            &work(1002, Phase::Running),
            EventKind::UsageChanged,
            1200,
            &sink
        ));
    }
    #[test]
    fn full_channel_drops_notifications_without_waiting_or_unbounded_retries() {
        let (sink, rx) = EventChannel::bounded(1);
        let mut events = TaskEvents::default();
        assert!(events.publish(&work(1, Phase::Running), EventKind::UsageChanged, 0, &sink));
        for sequence in 2..10_000 {
            assert!(!events.publish(
                &work(sequence, Phase::Running),
                EventKind::Progress,
                sequence * 200,
                &sink
            ));
        }
        assert!(!events.publish(
            &work(10_000, Phase::Completed),
            EventKind::Progress,
            2_000_000,
            &sink
        ));
        assert_eq!(rx.try_iter().count(), 1);
        assert!(!events.publish(
            &work(10_000, Phase::Completed),
            EventKind::Terminal,
            2_000_001,
            &sink
        ));
    }
}
