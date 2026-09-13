//! One generation-owned usage task on the shared fixed background worker.
use super::{
    budgeted_source::BudgetedSource,
    usage::{Observation, ObservationSource, ScanLimits, ScanPhase, UsageScanner},
};
use crate::{
    domain::controller::Cancellation,
    domain::{
        controller::Scope,
        index::{IndexError, SharedDirectoryIndex},
        model::{
            DirectoryUsage, EventKind, FsIssue, IssueScope, Issues, Kind, Measure, Operation,
            Phase, Reason, ScanState, WaitReason, WorkRecord,
        },
    },
    runtime::{
        clock::Clock,
        events::{EventSink, TaskEvents},
    },
    scheduler::{HandleBudget, Job, JobStep, Lane, Scheduler, Status, Ticket, Wait},
};
use std::sync::{
    Arc, Mutex, Weak,
    atomic::{AtomicU64, Ordering},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageError {
    InvalidArgument,
    SessionClosed,
    TaskExpired,
    EntryExpired,
    ResourceLimit,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelStatus {
    Accepted,
    AlreadyTerminal,
}
struct State {
    work: Option<WorkRecord>,
    ticket: Option<Ticket>,
    closed: bool,
    cancel_requested: bool,
}
pub struct UsageService {
    scope: Scope,
    root_id: String,
    index: SharedDirectoryIndex,
    scheduler: Arc<Scheduler>,
    clock: Arc<dyn Clock>,
    sink: Arc<dyn EventSink>,
    state: Arc<Mutex<State>>,
    admission: Mutex<()>,
}
static NEXT_TASK: AtomicU64 = AtomicU64::new(1);
fn issue(code: &str) -> FsIssue {
    FsIssue {
        code: code.into(),
        operation: "scanUsage".into(),
        scope: IssueScope::Root,
        entry_id: None,
        native_code: None,
    }
}
fn unknown(id: &str, now: &str) -> DirectoryUsage {
    DirectoryUsage {
        directory_id: id.into(),
        usage_revision: 0,
        scan_state: ScanState::NotRequested,
        logical: Measure::Unknown,
        allocated: Measure::Unknown,
        observed_at: now.into(),
    }
}
fn partial(measure: Measure, reason: Reason) -> Measure {
    match measure {
        Measure::Complete { bytes } => Measure::Partial {
            observed_bytes: bytes,
            reasons: vec![reason],
        },
        Measure::Partial {
            observed_bytes,
            mut reasons,
        } => {
            if !reasons.contains(&reason) {
                reasons.push(reason);
                reasons.sort();
            }
            Measure::Partial {
                observed_bytes,
                reasons,
            }
        }
        Measure::Unavailable { reason: old } => {
            let mut reasons = vec![old, reason];
            reasons.sort();
            reasons.dedup();
            Measure::Partial {
                observed_bytes: 0_u64.into(),
                reasons,
            }
        }
        Measure::Unknown => Measure::Partial {
            observed_bytes: 0_u64.into(),
            reasons: vec![reason],
        },
    }
}
fn mark_root(
    index: &SharedDirectoryIndex,
    root: &str,
    state: ScanState,
    reason: Reason,
    now: &str,
) {
    let mut usage = index
        .usage(root)
        .ok()
        .flatten()
        .unwrap_or_else(|| unknown(root, now));
    usage.usage_revision += 1;
    usage.scan_state = state;
    usage.observed_at = now.into();
    usage.logical = partial(usage.logical, reason);
    usage.allocated = partial(usage.allocated, reason);
    let _ = index.set_usage(usage);
}
impl UsageService {
    pub fn new(
        scope: Scope,
        root_id: String,
        index: SharedDirectoryIndex,
        scheduler: Arc<Scheduler>,
        clock: Arc<dyn Clock>,
        sink: Arc<dyn EventSink>,
    ) -> Result<Self, UsageError> {
        if scope.session_id.is_empty()
            || scope.session_id.len() > 128
            || scope.generation == 0
            || scope.generation > 9_007_199_254_740_991
        {
            return Err(UsageError::InvalidArgument);
        }
        if index
            .key(&root_id)
            .map_err(|_| UsageError::EntryExpired)?
            .parent_id
            .is_some()
        {
            return Err(UsageError::InvalidArgument);
        }
        index.bind_scope(&scope).map_err(|e| {
            if e == IndexError::ResourceLimit {
                UsageError::ResourceLimit
            } else {
                UsageError::InvalidArgument
            }
        })?;
        Ok(Self {
            scope,
            root_id,
            index,
            scheduler,
            clock,
            sink,
            state: Arc::new(Mutex::new(State {
                work: None,
                ticket: None,
                closed: false,
                cancel_requested: false,
            })),
            admission: Mutex::new(()),
        })
    }
    /// Source construction and root observation are supplied by the trusted
    /// adapter. Opening its cursor is deferred to the background worker.
    pub fn start<S>(
        &self,
        root: Observation,
        source: S,
        limits: ScanLimits,
    ) -> Result<WorkRecord, UsageError>
    where
        S: ObservationSource + Send + 'static,
        S::Cursor: Send,
    {
        let _admission = self.admission.lock().unwrap();
        {
            let state = self.state.lock().unwrap();
            if state.closed {
                return Err(UsageError::SessionClosed);
            }
            if let Some(work) = &state.work {
                return Ok(work.clone());
            }
        }
        if root.kind != Kind::Directory
            || root.raw_name
                != self
                    .index
                    .key(&self.root_id)
                    .map_err(|_| UsageError::EntryExpired)?
                    .raw_name
        {
            return Err(UsageError::InvalidArgument);
        }
        let number = NEXT_TASK
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .map_err(|_| UsageError::ResourceLimit)?;
        let id = format!("usage-task-{number}");
        let time = self.clock.sample();
        let work = WorkRecord {
            session_id: self.scope.session_id.clone(),
            generation: self.scope.generation,
            task_id: id.clone(),
            operation: Operation::ScanUsage,
            target_id: self.root_id.clone(),
            phase: Phase::Queued,
            wait_reason: Some(WaitReason::Slot),
            sequence: 0,
            processed_entries: 0_u64.into(),
            processed_directories: 0_u64.into(),
            observed_at: time.observed_at,
            issues: Issues::default(),
        };
        {
            let mut state = self.state.lock().unwrap();
            if state.closed {
                return Err(UsageError::SessionClosed);
            }
            state.work = Some(work);
        }
        let job = UsageJob {
            source: Some(source),
            root: Some(root),
            scanner: None,
            limits,
            index: self.index.clone(),
            root_id: self.root_id.clone(),
            state: Arc::downgrade(&self.state),
            clock: self.clock.clone(),
            sink: self.sink.clone(),
            events: TaskEvents::default(),
            failure: None,
        };
        match self
            .scheduler
            .submit(id, self.scope.clone(), Lane::Background, job)
        {
            Ok(ticket) => {
                let mut state = self.state.lock().unwrap();
                if state.closed {
                    drop(state);
                    self.scheduler.cancel_task(&ticket);
                    return Err(UsageError::SessionClosed);
                }
                state.ticket = Some(ticket);
                Ok(state.work.as_ref().unwrap().clone())
            }
            Err(_) => {
                let time = self.clock.sample();
                let work = {
                    let mut state = self.state.lock().unwrap();
                    if state.closed {
                        return Err(UsageError::SessionClosed);
                    }
                    mark_root(
                        &self.index,
                        &self.root_id,
                        ScanState::Failed,
                        Reason::ResourceLimit,
                        &time.observed_at,
                    );
                    state.work.as_mut().map(|work| {
                        work.phase = Phase::Failed;
                        work.wait_reason = None;
                        work.sequence += 1;
                        work.issues.record(issue("RESOURCE_LIMIT"));
                        work.clone()
                    })
                };
                if let Some(work) = work {
                    TaskEvents::default().publish(
                        &work,
                        EventKind::Terminal,
                        time.monotonic_ms,
                        self.sink.as_ref(),
                    );
                }
                Err(UsageError::ResourceLimit)
            }
        }
    }
    pub fn work(&self, id: &str) -> Result<WorkRecord, UsageError> {
        let ticket = {
            let state = self.state.lock().unwrap();
            if state.closed {
                return Err(UsageError::SessionClosed);
            }
            state.ticket.clone()
        };
        let wait = ticket
            .as_ref()
            .and_then(|ticket| self.scheduler.wait_reason(ticket));
        let mut state = self.state.lock().unwrap();
        if state.closed {
            return Err(UsageError::SessionClosed);
        }
        let cancel_requested = state.cancel_requested;
        let work = state
            .work
            .as_mut()
            .filter(|w| w.task_id == id)
            .ok_or(UsageError::TaskExpired)?;
        if !work.phase.terminal() && ticket.as_ref().is_none_or(|t| !t.status().terminal()) {
            let mut new_wait = wait.map(|w| match w {
                Wait::Slot => WaitReason::Slot,
                Wait::Draining => WaitReason::Draining,
                Wait::PageDemand => WaitReason::PageDemand,
            });
            if cancel_requested {
                new_wait = Some(WaitReason::Draining);
            }
            let phase = if ticket
                .as_ref()
                .is_some_and(|t| t.status() == Status::Queued)
            {
                Phase::Queued
            } else {
                Phase::Running
            };
            if work.phase != phase || work.wait_reason != new_wait {
                work.phase = phase;
                work.wait_reason = new_wait;
                work.sequence += 1;
            }
        }
        Ok(work.clone())
    }
    pub fn read_usage(&self, ids: &[String]) -> Result<Vec<DirectoryUsage>, UsageError> {
        if ids.len() > 1000 {
            return Err(UsageError::InvalidArgument);
        }
        if self.state.lock().unwrap().closed {
            return Err(UsageError::SessionClosed);
        }
        // Validate every ID before returning any data; this path never starts I/O.
        for id in ids {
            self.index.key(id).map_err(|_| UsageError::EntryExpired)?;
        }
        let time = self.clock.sample();
        let values: Vec<_> = ids
            .iter()
            .map(|id| {
                self.index
                    .usage(id)
                    .unwrap()
                    .unwrap_or_else(|| unknown(id, &time.observed_at))
            })
            .collect();
        if serde_json::to_vec(&values).map_or(true, |v| v.len() + 128 > 1_048_576) {
            return Err(UsageError::ResourceLimit);
        }
        if self.state.lock().unwrap().closed {
            return Err(UsageError::SessionClosed);
        }
        Ok(values)
    }
    pub fn cancel(&self, id: &str) -> Result<(CancelStatus, WorkRecord), UsageError> {
        let ticket = {
            let mut state = self.state.lock().unwrap();
            if state.closed {
                return Err(UsageError::SessionClosed);
            }
            let work = state
                .work
                .as_ref()
                .filter(|w| w.task_id == id)
                .ok_or(UsageError::TaskExpired)?;
            if work.phase.terminal() {
                return Ok((CancelStatus::AlreadyTerminal, work.clone()));
            }
            let ticket = state.ticket.clone().ok_or(UsageError::TaskExpired)?;
            state.cancel_requested = true;
            ticket
        };
        let accepted = self.scheduler.cancel_task(&ticket);
        let work = self.work(id)?;
        // The scheduler can have retired its slot while the finalizer is still
        // publishing cached partial results. AlreadyTerminal describes the
        // published work, not that internal transition; never wait here.
        Ok((
            if accepted || !work.phase.terminal() {
                CancelStatus::Accepted
            } else {
                CancelStatus::AlreadyTerminal
            },
            work,
        ))
    }
    pub fn close(&self) {
        let ticket = {
            let mut state = self.state.lock().unwrap();
            state.closed = true;
            state.work = None;
            state.ticket.take()
        };
        if let Some(ticket) = ticket {
            self.scheduler.cancel_task(&ticket);
        }
    }
}
impl Drop for UsageService {
    fn drop(&mut self) {
        self.close();
    }
}
struct UsageJob<S: ObservationSource> {
    source: Option<S>,
    root: Option<Observation>,
    scanner: Option<UsageScanner<BudgetedSource<S>>>,
    limits: ScanLimits,
    index: SharedDirectoryIndex,
    root_id: String,
    state: Weak<Mutex<State>>,
    clock: Arc<dyn Clock>,
    sink: Arc<dyn EventSink>,
    events: TaskEvents,
    failure: Option<FsIssue>,
}
impl<S: ObservationSource> UsageJob<S> {
    fn publish(&mut self, terminal: Option<Phase>) {
        let Some(state) = self.state.upgrade() else {
            return;
        };
        let time = self.clock.sample();
        let work = {
            let mut state = state.lock().unwrap();
            if state.closed {
                return;
            }
            let cancelled = state.cancel_requested;
            let Some(work) = &mut state.work else {
                return;
            };
            if work.phase.terminal() {
                return;
            }
            if let Some(scanner) = &self.scanner {
                work.processed_entries = scanner.stats.processed_entries.into();
                work.processed_directories = scanner.stats.processed_directories.into();
                work.issues = scanner.issues.clone();
            }
            if let Some(error) = self.failure.take() {
                work.issues.record(error);
            }
            work.phase = terminal.unwrap_or(Phase::Running);
            work.wait_reason = if terminal.is_none() && cancelled {
                Some(WaitReason::Draining)
            } else {
                None
            };
            work.sequence += 1;
            work.observed_at = time.observed_at;
            work.clone()
        };
        self.events.publish(
            &work,
            EventKind::UsageChanged,
            time.monotonic_ms,
            self.sink.as_ref(),
        );
    }
}
impl<S> Job for UsageJob<S>
where
    S: ObservationSource + Send + 'static,
    S::Cursor: Send,
{
    fn step(&mut self, cancel: &Cancellation, handles: &HandleBudget) -> JobStep {
        if self
            .state
            .upgrade()
            .is_none_or(|s| s.lock().unwrap().closed)
        {
            return JobStep::Completed;
        }
        if self.scanner.is_none() {
            match UsageScanner::new(
                BudgetedSource::new(self.source.take().unwrap(), handles.clone()),
                self.root.take().unwrap(),
                self.index.clone(),
                self.limits.clone(),
                cancel.clone(),
            ) {
                Ok(scanner) => self.scanner = Some(scanner),
                Err(code) => {
                    self.failure = Some(issue(code));
                    return if code == "RESOURCE_LIMIT" {
                        JobStep::ResourceLimit
                    } else {
                        JobStep::Failed
                    };
                }
            }
        }
        let time = self.clock.sample();
        let scanner = self.scanner.as_mut().unwrap();
        let phase = scanner.step(128, &time.observed_at);
        let failed = scanner.root_failed();
        self.publish(None);
        match phase {
            ScanPhase::Running => JobStep::Yield,
            ScanPhase::Completed if failed => JobStep::Failed,
            _ => JobStep::Completed,
        }
    }
    fn stopped(&mut self, status: Status) {
        if self
            .state
            .upgrade()
            .is_none_or(|s| s.lock().unwrap().closed)
        {
            return;
        }
        let time = self.clock.sample();
        let (phase, reason, scan_state) = match status {
            Status::Cancelled => (
                Phase::Cancelled,
                Some(Reason::Cancelled),
                ScanState::Cancelled,
            ),
            Status::Failed => (Phase::Failed, Some(Reason::ReadError), ScanState::Failed),
            Status::ResourceLimit => (
                Phase::Completed,
                Some(Reason::ResourceLimit),
                ScanState::Settled,
            ),
            _ => (Phase::Completed, None, ScanState::Settled),
        };
        if let Some(reason) = reason {
            if let Some(scanner) = &mut self.scanner {
                scanner.abort(reason, &time.observed_at);
            }
            mark_root(
                &self.index,
                &self.root_id,
                scan_state,
                reason,
                &time.observed_at,
            );
            if self.failure.is_none()
                && self
                    .scanner
                    .as_ref()
                    .is_none_or(|s| s.issues.counts.is_empty())
            {
                self.failure = match status {
                    Status::Failed => Some(issue("INTERNAL")),
                    Status::ResourceLimit => Some(issue("RESOURCE_LIMIT")),
                    _ => None,
                };
            }
        }
        self.publish(Some(phase));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        domain::index::{DirectoryIndex, DirectoryKey, Origin},
        runtime::{clock::SystemClock, events::EventChannel},
        scan::usage::{FileIdentity, Metric},
    };
    use std::{
        collections::{BTreeMap, VecDeque},
        sync::{atomic::AtomicUsize, mpsc},
        thread,
        time::{Duration, Instant},
    };
    #[derive(Default)]
    struct Counts {
        opened: AtomicUsize,
        next: AtomicUsize,
        closed: AtomicUsize,
    }
    struct Cursor {
        rows: VecDeque<Observation>,
        counts: Arc<Counts>,
    }
    impl Drop for Cursor {
        fn drop(&mut self) {
            self.counts.closed.fetch_add(1, Ordering::SeqCst);
        }
    }
    struct Source {
        rows: BTreeMap<u64, Vec<Observation>>,
        counts: Arc<Counts>,
        block_open: Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>,
        block_next: Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>,
        open_error: Option<FsIssue>,
        panic_next: bool,
    }
    impl ObservationSource for Source {
        type Cursor = Cursor;
        fn open(&mut self, directory: &Observation) -> Result<Cursor, FsIssue> {
            self.counts.opened.fetch_add(1, Ordering::SeqCst);
            if let Some((ready, release)) = self.block_open.take() {
                ready.send(()).unwrap();
                release.recv().unwrap();
            }
            if let Some(error) = self.open_error.take() {
                return Err(error);
            }
            Ok(Cursor {
                rows: self
                    .rows
                    .get(&directory.node)
                    .cloned()
                    .unwrap_or_default()
                    .into(),
                counts: self.counts.clone(),
            })
        }
        fn next(&mut self, cursor: &mut Cursor) -> Result<Option<Observation>, FsIssue> {
            self.counts.next.fetch_add(1, Ordering::SeqCst);
            assert!(!self.panic_next, "injected fixture failure");
            if let Some((ready, release)) = self.block_next.take() {
                ready.send(()).unwrap();
                release.recv().unwrap();
            }
            Ok(cursor.rows.pop_front())
        }
    }
    fn observation(node: u64, name: &str, kind: Kind, bytes: u64) -> Observation {
        Observation {
            node,
            raw_name: name.as_bytes().to_vec(),
            kind,
            identity: Some(FileIdentity {
                device: 1,
                inode: node,
            }),
            link_count: 1,
            mount: 1,
            logical: Metric::Known(bytes),
            allocated: Metric::Known(bytes * 8),
        }
    }
    fn source(rows: Vec<Observation>) -> (Source, Arc<Counts>) {
        let counts = Arc::new(Counts::default());
        (
            Source {
                rows: BTreeMap::from([(1, rows)]),
                counts: counts.clone(),
                block_open: None,
                block_next: None,
                open_error: None,
                panic_next: false,
            },
            counts,
        )
    }
    fn setup(
        scheduler: Arc<Scheduler>,
        capacity: usize,
        generation: u64,
    ) -> (
        UsageService,
        mpsc::Receiver<crate::domain::model::WorkEvent>,
    ) {
        let mut index =
            DirectoryIndex::new(format!("g{generation}"), 48 * 1024 * 1024, 16 * 1024 * 1024);
        let root = index
            .register(
                DirectoryKey {
                    parent_id: None,
                    raw_name: b"root".to_vec(),
                },
                Origin::Foreground,
            )
            .unwrap();
        let (sink, receiver) = EventChannel::bounded(capacity);
        (
            UsageService::new(
                Scope {
                    session_id: "fixture".into(),
                    generation,
                },
                root,
                index.into(),
                scheduler,
                Arc::new(SystemClock::default()),
                Arc::new(sink),
            )
            .unwrap(),
            receiver,
        )
    }
    fn start(service: &UsageService, source: Source) -> WorkRecord {
        service
            .start(
                observation(1, "root", Kind::Directory, 1),
                source,
                ScanLimits::default(),
            )
            .unwrap()
    }
    fn until(mut predicate: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !predicate() {
            assert!(Instant::now() < deadline, "usage fixture timed out");
            thread::yield_now();
        }
    }
    fn wait(service: &UsageService, id: &str) -> WorkRecord {
        until(|| service.work(id).unwrap().phase.terminal());
        service.work(id).unwrap()
    }
    #[test]
    fn blocked_root_open_is_off_control_thread_and_duplicate_start_does_not_reopen() {
        let scheduler = Arc::new(Scheduler::default());
        let (service, _) = setup(scheduler.clone(), 16, 1);
        let (mut input, counts) = source(vec![]);
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        input.block_open = Some((ready_tx, release_rx));
        let before = Instant::now();
        let work = start(&service, input);
        assert!(before.elapsed() < Duration::from_millis(100));
        ready_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        let (unused, unused_counts) = source(vec![]);
        assert_eq!(start(&service, unused).task_id, work.task_id);
        assert_eq!(unused_counts.opened.load(Ordering::SeqCst), 0);
        let unknown = service.read_usage(&[service.root_id.clone()]).unwrap();
        assert_eq!(unknown[0].logical, Measure::Unknown);
        assert_eq!(counts.next.load(Ordering::SeqCst), 0);
        let before = Instant::now();
        let (status, accepted) = service.cancel(&work.task_id).unwrap();
        assert!(before.elapsed() < Duration::from_millis(100));
        assert_eq!(status, CancelStatus::Accepted);
        assert_eq!(accepted.phase, Phase::Running);
        assert_eq!(accepted.wait_reason, Some(WaitReason::Draining));
        assert_eq!(scheduler.handles().open(), 1);
        release_tx.send(()).unwrap();
        assert_eq!(wait(&service, &work.task_id).phase, Phase::Cancelled);
        until(|| scheduler.handles().open() == 0);
        assert_eq!(counts.next.load(Ordering::SeqCst), 0);
        let usage = service.read_usage(&[service.root_id.clone()]).unwrap();
        assert_eq!(usage[0].scan_state, ScanState::Cancelled);
        assert!(
            matches!(&usage[0].logical,Measure::Partial{reasons,..}if reasons.contains(&Reason::Cancelled))
        );
    }
    #[test]
    fn late_metadata_after_cancel_is_discarded_and_terminal_event_follows_cached_partial() {
        let scheduler = Arc::new(Scheduler::default());
        let (service, rx) = setup(scheduler.clone(), 16, 1);
        let (mut input, counts) =
            source(vec![observation(2, "late", Kind::RegularFile, 1_000_000)]);
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        input.block_next = Some((ready_tx, release_rx));
        let work = start(&service, input);
        ready_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        service.cancel(&work.task_id).unwrap();
        release_tx.send(()).unwrap();
        let done = wait(&service, &work.task_id);
        assert_eq!(done.phase, Phase::Cancelled);
        assert_eq!(done.processed_entries, 0_u64.into());
        let usage = service.read_usage(&[service.root_id.clone()]).unwrap();
        assert!(
            matches!(&usage[0].logical,Measure::Partial{observed_bytes,reasons}if *observed_bytes==1_u64.into()&&reasons.contains(&Reason::Cancelled))
        );
        let mut terminal = None;
        until(|| {
            for event in rx.try_iter() {
                if event.kind == EventKind::Terminal {
                    terminal = Some(event);
                }
            }
            terminal.is_some()
        });
        assert_eq!(terminal.unwrap().sequence, done.sequence);
        until(|| counts.closed.load(Ordering::SeqCst) == 1 && scheduler.handles().open() == 0);
        assert_eq!(
            service.cancel(&work.task_id).unwrap().0,
            CancelStatus::AlreadyTerminal
        );
    }
    #[test]
    fn missing_events_recover_from_cached_work_and_usage_without_new_io() {
        let scheduler = Arc::new(Scheduler::default());
        let (service, rx) = setup(scheduler, 0, 1);
        let (input, counts) = source(
            (2..1002)
                .map(|n| observation(n, &format!("f{n}"), Kind::RegularFile, 1))
                .collect(),
        );
        let work = start(&service, input);
        let done = wait(&service, &work.task_id);
        assert_eq!(done.phase, Phase::Completed);
        assert_eq!(done.processed_entries, 1000_u64.into());
        assert!(rx.try_recv().is_err());
        let calls = counts.next.load(Ordering::SeqCst);
        for _ in 0..100 {
            assert_eq!(service.work(&work.task_id).unwrap(), done);
            assert_eq!(
                service.read_usage(&[service.root_id.clone()]).unwrap()[0].logical,
                Measure::Complete {
                    bytes: 1001_u64.into()
                }
            );
        }
        assert_eq!(counts.next.load(Ordering::SeqCst), calls);
        let (unused, unused_counts) = source(vec![]);
        assert_eq!(start(&service, unused), done);
        assert_eq!(unused_counts.opened.load(Ordering::SeqCst), 0);
    }
    #[test]
    fn root_permission_error_is_failed_with_native_code_but_cancel_discards_late_error() {
        for cancelled in [false, true] {
            let scheduler = Arc::new(Scheduler::default());
            let (service, _) = setup(scheduler.clone(), 16, 1);
            let (mut input, _) = source(vec![]);
            let mut error = issue("PERMISSION_DENIED");
            error.native_code = Some(13);
            input.open_error = Some(error);
            let (ready_tx, ready_rx) = mpsc::channel();
            let (release_tx, release_rx) = mpsc::channel();
            input.block_open = Some((ready_tx, release_rx));
            let work = start(&service, input);
            ready_rx.recv_timeout(Duration::from_secs(3)).unwrap();
            if cancelled {
                service.cancel(&work.task_id).unwrap();
            }
            release_tx.send(()).unwrap();
            let done = wait(&service, &work.task_id);
            if cancelled {
                assert_eq!(done.phase, Phase::Cancelled);
                assert!(done.issues.counts.is_empty());
            } else {
                assert_eq!(done.phase, Phase::Failed);
                assert_eq!(done.issues.samples[0].native_code, Some(13));
                assert_eq!(
                    service.read_usage(&[service.root_id.clone()]).unwrap()[0].scan_state,
                    ScanState::Failed
                );
            }
            until(|| scheduler.handles().open() == 0);
        }
    }
    #[test]
    fn generation_close_suppresses_old_events_and_new_scan_waits_for_same_worker() {
        let scheduler = Arc::new(Scheduler::default());
        let (old, rx) = setup(scheduler.clone(), 16, 1);
        let (mut input, _) = source(vec![observation(2, "late", Kind::RegularFile, 100)]);
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        input.block_next = Some((ready_tx, release_rx));
        let work = start(&old, input);
        ready_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        old.close();
        assert_eq!(old.work(&work.task_id), Err(UsageError::SessionClosed));
        let (new, _) = setup(scheduler.clone(), 16, 2);
        let (input, counts) = source(vec![observation(3, "new", Kind::RegularFile, 9)]);
        let next = start(&new, input);
        assert_eq!(
            new.work(&next.task_id).unwrap().wait_reason,
            Some(WaitReason::Draining)
        );
        assert_eq!(counts.opened.load(Ordering::SeqCst), 0);
        release_tx.send(()).unwrap();
        assert_eq!(wait(&new, &next.task_id).phase, Phase::Completed);
        assert_eq!(
            new.read_usage(&[new.root_id.clone()]).unwrap()[0].logical,
            Measure::Complete {
                bytes: 10_u64.into()
            }
        );
        assert!(rx.try_recv().is_err());
        until(|| scheduler.handles().open() == 0);
    }
    #[test]
    fn queued_cancel_opens_nothing_and_batch_validation_never_starts_a_scan() {
        struct Block {
            ready: mpsc::Sender<()>,
            release: mpsc::Receiver<()>,
        }
        impl Job for Block {
            fn step(&mut self, _: &Cancellation, _: &HandleBudget) -> JobStep {
                self.ready.send(()).unwrap();
                self.release.recv().unwrap();
                JobStep::Completed
            }
        }
        let scheduler = Arc::new(Scheduler::default());
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        scheduler
            .submit(
                "occupy".into(),
                Scope {
                    session_id: "other".into(),
                    generation: 1,
                },
                Lane::Background,
                Block {
                    ready: ready_tx,
                    release: release_rx,
                },
            )
            .unwrap();
        ready_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        let (service, _) = setup(scheduler.clone(), 16, 1);
        assert_eq!(
            service.read_usage(&vec![service.root_id.clone(); 1001]),
            Err(UsageError::InvalidArgument)
        );
        assert_eq!(
            service.read_usage(&[service.root_id.clone(), "other-generation".into()]),
            Err(UsageError::EntryExpired)
        );
        assert_eq!(
            service
                .read_usage(&vec![service.root_id.clone(); 1000])
                .unwrap()
                .len(),
            1000
        );
        let (input, counts) = source(vec![]);
        let work = start(&service, input);
        assert_eq!(
            service.cancel(&work.task_id).unwrap().0,
            CancelStatus::Accepted
        );
        assert_eq!(wait(&service, &work.task_id).phase, Phase::Cancelled);
        assert_eq!(counts.opened.load(Ordering::SeqCst), 0);
        assert_eq!(
            service.read_usage(&[service.root_id.clone()]).unwrap()[0].scan_state,
            ScanState::Cancelled
        );
        release_tx.send(()).unwrap();
    }
    #[test]
    fn a_shared_handle_limit_is_partial_resource_limit_without_opening_the_source() {
        let scheduler = Arc::new(Scheduler::new(64, 64, 0));
        let (service, _) = setup(scheduler, 16, 1);
        let (input, counts) = source(vec![]);
        let work = start(&service, input);
        let done = wait(&service, &work.task_id);
        assert_eq!(done.phase, Phase::Completed);
        assert!(done.issues.counts.contains_key("RESOURCE_LIMIT"));
        assert_eq!(counts.opened.load(Ordering::SeqCst), 0);
        assert!(
            matches!(&service.read_usage(&[service.root_id.clone()]).unwrap()[0].logical,Measure::Partial{reasons,..}if reasons.contains(&Reason::ResourceLimit))
        );
    }
    #[test]
    fn queue_admission_failure_preserves_failure_without_opening_a_source() {
        let scheduler = Arc::new(Scheduler::new(0, 64, 320));
        let (service, receiver) = setup(scheduler, 16, 1);
        let (input, counts) = source(vec![]);
        assert_eq!(
            service.start(
                observation(1, "root", Kind::Directory, 1),
                input,
                ScanLimits::default()
            ),
            Err(UsageError::ResourceLimit)
        );
        assert_eq!(counts.opened.load(Ordering::SeqCst), 0);
        let usage = service.read_usage(&[service.root_id.clone()]).unwrap();
        assert_eq!(usage[0].scan_state, ScanState::Failed);
        assert!(
            matches!(&usage[0].logical, Measure::Partial { reasons, .. } if reasons.contains(&Reason::ResourceLimit))
        );
        assert_eq!(receiver.try_recv().unwrap().kind, EventKind::Terminal);
    }
    #[test]
    fn retirement_before_final_publication_is_not_reported_as_already_terminal() {
        struct GatedClock {
            calls: AtomicUsize,
            ready: mpsc::Sender<()>,
            release: Mutex<mpsc::Receiver<()>>,
            inner: SystemClock,
        }
        impl Clock for GatedClock {
            fn sample(&self) -> crate::runtime::clock::TimeSample {
                if self.calls.fetch_add(1, Ordering::SeqCst) == 3 {
                    self.ready.send(()).unwrap();
                    self.release.lock().unwrap().recv().unwrap();
                }
                self.inner.sample()
            }
        }
        let scheduler = Arc::new(Scheduler::default());
        let (mut service, _) = setup(scheduler, 16, 1);
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        service.clock = Arc::new(GatedClock {
            calls: AtomicUsize::new(0),
            ready: ready_tx,
            release: Mutex::new(release_rx),
            inner: SystemClock::default(),
        });
        let (input, _) = source(vec![]);
        let work = start(&service, input);
        ready_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        let before = Instant::now();
        let (status, pending) = service.cancel(&work.task_id).unwrap();
        assert!(before.elapsed() < Duration::from_millis(100));
        assert_eq!(status, CancelStatus::Accepted);
        assert_eq!(pending.phase, Phase::Running);
        release_tx.send(()).unwrap();
        assert_eq!(wait(&service, &work.task_id).phase, Phase::Completed);
        assert_eq!(
            service.cancel(&work.task_id).unwrap().0,
            CancelStatus::AlreadyTerminal
        );
    }
    #[test]
    fn a_panicking_source_finishes_failed_and_the_same_worker_runs_the_next_generation() {
        let scheduler = Arc::new(Scheduler::default());
        let (service, receiver) = setup(scheduler.clone(), 16, 1);
        let (mut input, _) = source(vec![]);
        input.panic_next = true;
        let work = start(&service, input);
        let failed = wait(&service, &work.task_id);
        assert_eq!(failed.phase, Phase::Failed);
        assert!(failed.issues.counts.contains_key("INTERNAL"));
        assert_eq!(
            service.read_usage(&[service.root_id.clone()]).unwrap()[0].scan_state,
            ScanState::Failed
        );
        let mut saw_terminal = false;
        until(|| {
            saw_terminal |= receiver.try_iter().any(|e| e.kind == EventKind::Terminal);
            saw_terminal
        });
        let (next, _) = setup(scheduler.clone(), 16, 2);
        let (input, _) = source(vec![]);
        let work = start(&next, input);
        assert_eq!(wait(&next, &work.task_id).phase, Phase::Completed);
        until(|| scheduler.handles().open() == 0);
    }
    #[test]
    fn a_batch_read_does_not_hold_the_control_lock_and_rechecks_close_before_returning() {
        struct BlockReadClock {
            ready: mpsc::Sender<()>,
            release: Mutex<mpsc::Receiver<()>>,
        }
        impl Clock for BlockReadClock {
            fn sample(&self) -> crate::runtime::clock::TimeSample {
                self.ready.send(()).unwrap();
                self.release.lock().unwrap().recv().unwrap();
                crate::runtime::clock::TimeSample {
                    monotonic_ms: 0,
                    observed_at: "2026-09-13T00:00:00Z".into(),
                }
            }
        }
        let scheduler = Arc::new(Scheduler::default());
        let (mut service, _) = setup(scheduler, 16, 1);
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        service.clock = Arc::new(BlockReadClock {
            ready: ready_tx,
            release: Mutex::new(release_rx),
        });
        let service = Arc::new(service);
        let reader_service = service.clone();
        let reader = thread::spawn(move || {
            reader_service.read_usage(&vec![reader_service.root_id.clone(); 1000])
        });
        ready_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        let closer_service = service.clone();
        let (closed_tx, closed_rx) = mpsc::channel();
        let closer = thread::spawn(move || {
            closer_service.close();
            closed_tx.send(()).unwrap();
        });
        let closed_without_wait = closed_rx.recv_timeout(Duration::from_millis(100)).is_ok();
        release_tx.send(()).unwrap();
        let response = reader.join().unwrap();
        closer.join().unwrap();
        assert!(closed_without_wait, "batch read held the control lock");
        assert_eq!(response, Err(UsageError::SessionClosed));
    }
    #[test]
    fn an_index_cannot_be_reused_for_another_session_or_generation() {
        let scheduler = Arc::new(Scheduler::default());
        let (service, _) = setup(scheduler.clone(), 16, 1);
        for scope in [
            Scope {
                session_id: "fixture".into(),
                generation: 2,
            },
            Scope {
                session_id: "other".into(),
                generation: 1,
            },
        ] {
            let result = UsageService::new(
                scope,
                service.root_id.clone(),
                service.index.clone(),
                scheduler.clone(),
                Arc::new(SystemClock::default()),
                Arc::new(crate::runtime::events::DiscardEvents),
            );
            assert!(matches!(result, Err(UsageError::InvalidArgument)));
        }
    }
}
