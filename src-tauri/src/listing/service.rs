//! Demand-driven listing integration over an internal, typed source.
//! No Tauri commands or production filesystem implementation are registered here.
use super::cache::{CacheError, ListingCache, ListingHandle};
use crate::{
    domain::{
        controller::{Cancellation, Scope},
        index::{DirectoryKey, IndexError, Origin, SharedDirectoryIndex},
        model::{
            Category, Entry, EventKind, FsIssue, IssueScope, Issues, Kind, ListingPage, Operation,
            Phase, WaitReason, WorkRecord,
        },
    },
    runtime::{
        clock::{Clock, SystemClock},
        events::{DiscardEvents, EventSink, TaskEvents},
    },
    scheduler::{
        HandleBudget, HandlePermit, Job, JobStep, Lane, Scheduler, Status, SubmitError, Ticket,
        Wait,
    },
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Clone)]
pub struct ListedEntry {
    pub raw_name: Vec<u8>,
    /// IDs are overwritten by the service. The source never chooses wire IDs.
    pub entry: Entry,
    /// Per-entry metadata errors preserve the entry and allow later siblings.
    pub issue: Option<FsIssue>,
}
/// Construction must not perform I/O. open/next run only in the fixed worker.
/// Own at most one directory cursor; the service reserves its handle permit.
/// Drop must only release owned resources, never perform another filesystem query.
pub trait ListingSource: Send + 'static {
    type Cursor: Send;
    fn open(&mut self, directory_id: &str) -> Result<Self::Cursor, FsIssue>;
    fn next(&mut self, cursor: &mut Self::Cursor) -> Result<Option<ListedEntry>, FsIssue>;
}
struct State {
    cache: ListingCache,
    tickets: BTreeMap<String, Ticket>,
    cancelled: BTreeSet<String>,
    closed: bool,
    seen_eviction_epoch: u64,
}
impl State {
    fn retired(&mut self) -> Vec<Ticket> {
        let epoch = self.cache.eviction_epoch();
        if epoch == self.seen_eviction_epoch {
            return vec![];
        }
        self.seen_eviction_epoch = epoch;
        let retained = self.cache.retained_task_ids();
        let ids: Vec<_> = self
            .tickets
            .keys()
            .filter(|id| !retained.contains(id.as_str()))
            .cloned()
            .collect();
        ids.into_iter()
            .filter_map(|id| {
                self.cancelled.remove(&id);
                self.tickets.remove(&id)
            })
            .collect()
    }
}
fn cancel_retired(scheduler: &Scheduler, retired: Vec<Ticket>) {
    for ticket in retired {
        scheduler.cancel_task(&ticket);
    }
}
fn issue(code: &str) -> FsIssue {
    FsIssue {
        code: code.into(),
        operation: "list".into(),
        scope: IssueScope::Subtree,
        entry_id: None,
        native_code: None,
    }
}
fn sync_ticket(cache: &mut ListingCache, ticket: &Ticket, wait: Option<Wait>) {
    let (phase, error) = match ticket.status() {
        Status::Completed => (Phase::Completed, None),
        Status::Cancelled => (Phase::Cancelled, None),
        Status::Failed => (Phase::Failed, Some(issue("INTERNAL"))),
        Status::ResourceLimit => (Phase::Completed, Some(issue("RESOURCE_LIMIT"))),
        Status::Queued => (Phase::Queued, None),
        Status::Running | Status::Paused => (Phase::Running, None),
    };
    if phase.terminal() {
        let _ = cache.finish(ticket.id(), phase, error);
    } else {
        cache.update_status(
            ticket.id(),
            phase,
            wait.map(|w| match w {
                Wait::Slot => WaitReason::Slot,
                Wait::Draining => WaitReason::Draining,
                Wait::PageDemand => WaitReason::PageDemand,
            }),
        );
    }
}
/// Generation-owned state sharing the application-wide scheduler and index.
/// Store locks are never held during source calls or scheduler calls.
pub struct ListingService {
    scope: Scope,
    start_gate: Mutex<()>,
    state: Arc<Mutex<State>>,
    pub index: SharedDirectoryIndex,
    scheduler: Arc<Scheduler>,
    clock: Arc<dyn Clock>,
    sink: Arc<dyn EventSink>,
}
static NEXT_TASK: AtomicU64 = AtomicU64::new(1);
impl ListingService {
    pub fn new(
        scope: Scope,
        index: SharedDirectoryIndex,
        scheduler: Arc<Scheduler>,
        cache: ListingCache,
    ) -> Self {
        Self::with_events(
            scope,
            index,
            scheduler,
            cache,
            Arc::new(SystemClock::default()),
            Arc::new(DiscardEvents),
        )
    }
    pub fn with_events(
        scope: Scope,
        index: SharedDirectoryIndex,
        scheduler: Arc<Scheduler>,
        cache: ListingCache,
        clock: Arc<dyn Clock>,
        sink: Arc<dyn EventSink>,
    ) -> Self {
        Self {
            scope,
            clock,
            sink,
            start_gate: Mutex::new(()),
            index,
            scheduler,
            state: Arc::new(Mutex::new(State {
                cache,
                tickets: BTreeMap::new(),
                cancelled: BTreeSet::new(),
                closed: false,
                seen_eviction_epoch: 0,
            })),
        }
    }
    pub fn start<S: ListingSource>(
        &self,
        target_id: &str,
        category: Category,
        limit: usize,
        observed_at: &str,
        now: u64,
        source: S,
    ) -> Result<ListingHandle, CacheError> {
        // Serialize only admission; neither this guard nor the store lock is
        // held by the worker. A duplicate cannot return an unregistered ticket.
        let _admission = self.start_gate.lock().unwrap();
        if !(1..=1000).contains(&limit) || observed_at.len() > 64 {
            return Err(CacheError::InvalidArgument);
        }
        self.index
            .key(target_id)
            .map_err(|_| CacheError::InvalidArgument)?;
        self.index.bind_scope(&self.scope).map_err(|e| {
            if e == IndexError::ResourceLimit {
                CacheError::ResourceLimit
            } else {
                CacheError::InvalidArgument
            }
        })?;
        let number = NEXT_TASK
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .map_err(|_| CacheError::ResourceLimit)?;
        let id = format!("list-task-{number}");
        let work = WorkRecord {
            session_id: self.scope.session_id.clone(),
            generation: self.scope.generation,
            task_id: id.clone(),
            operation: if category == Category::Directories {
                Operation::ListDirectories
            } else {
                Operation::ListFiles
            },
            target_id: target_id.into(),
            phase: Phase::Queued,
            wait_reason: Some(WaitReason::Slot),
            sequence: 0,
            processed_entries: 0_u64.into(),
            processed_directories: 0_u64.into(),
            observed_at: observed_at.into(),
            issues: Issues::default(),
        };
        let (result, retired) = {
            let mut state = self.state.lock().unwrap();
            if state.closed {
                return Err(CacheError::TaskExpired);
            }
            let result = state.cache.start(work, category, now).and_then(|handle| {
                if handle.task_id == id {
                    state.cache.set_initial_demand(&handle.task_id, limit)?;
                }
                Ok(handle)
            });
            (result, state.retired())
        };
        cancel_retired(&self.scheduler, retired);
        let handle = result?;
        if handle.task_id != id {
            return Ok(handle);
        }
        let job = ListingJob {
            source,
            cursor: None,
            permit: None,
            state: Arc::downgrade(&self.state),
            index: self.index.clone(),
            scheduler: Arc::downgrade(&self.scheduler),
            task: id.clone(),
            target: target_id.into(),
            category,
            observed: 0,
            directories: 0,
            clock: self.clock.clone(),
            sink: self.sink.clone(),
            events: TaskEvents::default(),
        };
        // Submission is outside the store lock. A concurrent close/eviction is
        // checked again after registration and by the worker before opening.
        let ticket = self
            .scheduler
            .submit(id.clone(), self.scope.clone(), Lane::Foreground, job);
        let mut state = self.state.lock().unwrap();
        match ticket {
            Ok(ticket) => {
                let keep = !state.closed && state.cache.contains(&id);
                if keep {
                    state.tickets.insert(id, ticket.clone());
                }
                drop(state);
                if !keep {
                    self.scheduler.cancel_task(&ticket);
                    return Err(CacheError::TaskExpired);
                }
                Ok(handle)
            }
            Err(error) => {
                let _ = state
                    .cache
                    .finish(&id, Phase::Failed, Some(issue("RESOURCE_LIMIT")));
                Err(if error == SubmitError::ResourceLimit {
                    CacheError::ResourceLimit
                } else {
                    CacheError::TaskExpired
                })
            }
        }
    }
    fn synchronize(&self, id: &str) {
        let ticket = self.state.lock().unwrap().tickets.get(id).cloned();
        if let Some(ticket) = ticket {
            let wait = self.scheduler.wait_reason(&ticket);
            let mut state = self.state.lock().unwrap();
            if state.cancelled.contains(id) && ticket.status().terminal() {
                let _ = state.cache.finish(id, Phase::Cancelled, None);
            } else {
                let wait = if state.cancelled.contains(id) && ticket.status() == Status::Running {
                    Some(Wait::Draining)
                } else {
                    wait
                };
                sync_ticket(&mut state.cache, &ticket, wait);
            }
        }
    }
    pub fn page(
        &self,
        id: &str,
        cursor: Option<&str>,
        limit: usize,
        now: u64,
    ) -> Result<ListingPage, CacheError> {
        self.synchronize(id);
        let (result, retired, demand) = {
            let mut state = self.state.lock().unwrap();
            let result = state.cache.page(id, cursor, limit, now);
            let demand = result.is_ok() && state.cache.needs_collection(id) == Ok(true);
            (result, state.retired(), demand)
        };
        cancel_retired(&self.scheduler, retired);
        if demand {
            // A full queue is temporary; the next active-task poll retries this
            // demand without restarting or opening a second iterator.
            let _ = self.scheduler.resume(id);
        }
        result
    }
    pub fn work(&self, id: &str, now: u64) -> Result<WorkRecord, CacheError> {
        self.synchronize(id);
        let (result, retired, demand) = {
            let mut state = self.state.lock().unwrap();
            let result = state.cache.work(id, now);
            let demand = state.cache.needs_collection(id) == Ok(true);
            (result, state.retired(), demand)
        };
        cancel_retired(&self.scheduler, retired);
        if demand {
            let _ = self.scheduler.resume(id);
        }
        result
    }
    pub fn cancel(&self, id: &str) -> Result<(), CacheError> {
        let ticket = {
            let mut state = self.state.lock().unwrap();
            let ticket = state
                .tickets
                .get(id)
                .cloned()
                .ok_or(CacheError::TaskExpired)?;
            if state
                .cache
                .peek_work(id)
                .is_some_and(|w| w.phase.terminal())
            {
                return Ok(());
            }
            // Same lock as entry publication: once accepted, no new rows enter
            // this snapshot even between a source return and scheduler signaling.
            state.cancelled.insert(id.into());
            ticket
        };
        self.scheduler.cancel_task(&ticket);
        self.synchronize(id);
        Ok(())
    }
    pub fn expire(&self, now: u64) {
        let retired = {
            let mut state = self.state.lock().unwrap();
            state.cache.expire(now);
            state.retired()
        };
        cancel_retired(&self.scheduler, retired);
    }
    pub fn close(&self) {
        {
            let mut state = self.state.lock().unwrap();
            state.closed = true;
            state.cache.clear();
            state.tickets.clear();
            state.cancelled.clear();
        }
        self.scheduler.cancel_scope(&self.scope);
    }
    pub fn managed_listing_bytes(&self) -> usize {
        self.state.lock().unwrap().cache.managed_bytes()
    }
}
impl Drop for ListingService {
    fn drop(&mut self) {
        self.close();
    }
}

struct ListingJob<S: ListingSource> {
    source: S,
    cursor: Option<S::Cursor>,
    permit: Option<HandlePermit>,
    state: Weak<Mutex<State>>,
    index: SharedDirectoryIndex,
    scheduler: Weak<Scheduler>,
    task: String,
    target: String,
    category: Category,
    observed: u64,
    directories: u64,
    clock: Arc<dyn Clock>,
    sink: Arc<dyn EventSink>,
    events: TaskEvents,
}
impl<S: ListingSource> ListingJob<S> {
    fn publish(&mut self) {
        let work = self
            .state
            .upgrade()
            .and_then(|state| state.lock().unwrap().cache.peek_work(&self.task));
        if let Some(work) = work {
            self.events.publish(
                &work,
                EventKind::ListingAvailable,
                self.clock.sample().monotonic_ms,
                self.sink.as_ref(),
            );
        }
    }
    fn finish(&self, phase: Phase, error: Option<FsIssue>) -> JobStep {
        let mut actual = phase;
        if let Some(state) = self.state.upgrade() {
            let mut state = state.lock().unwrap();
            let error = if state.cancelled.contains(&self.task) {
                actual = Phase::Cancelled;
                None
            } else {
                error
            };
            let _ = state.cache.finish(&self.task, actual, error);
        }
        if actual == Phase::Failed {
            JobStep::Failed
        } else {
            JobStep::Completed
        }
    }
}
impl<S: ListingSource> Job for ListingJob<S> {
    fn step(&mut self, cancel: &Cancellation, handles: &HandleBudget) -> JobStep {
        let Some(state) = self.state.upgrade() else {
            return JobStep::Completed;
        };
        let start = Instant::now();
        for item in 0..128 {
            if item > 0 && start.elapsed() >= Duration::from_millis(20) {
                self.publish();
                return JobStep::Yield;
            }
            if cancel.is_cancelled() {
                return self.finish(Phase::Cancelled, None);
            }
            let demand = {
                let guard = state.lock().unwrap();
                if guard.closed || guard.cancelled.contains(&self.task) {
                    Err(CacheError::TaskExpired)
                } else {
                    guard.cache.needs_collection(&self.task)
                }
            };
            match demand {
                Ok(true) => (),
                Ok(false) => {
                    self.publish();
                    return JobStep::Paused;
                }
                Err(_) => return JobStep::Completed,
            }
            if self.cursor.is_none() {
                self.permit = match handles.acquire() {
                    Ok(permit) => Some(permit),
                    Err(_) => return self.finish(Phase::Completed, Some(issue("RESOURCE_LIMIT"))),
                };
                let opened = self.source.open(&self.target);
                if cancel.is_cancelled() {
                    return self.finish(Phase::Cancelled, None);
                }
                match opened {
                    Ok(cursor) => self.cursor = Some(cursor),
                    Err(error) => return self.finish(Phase::Failed, Some(error)),
                }
                // Opening may block, so cancellation is checked before next().
                if cancel.is_cancelled() {
                    return self.finish(Phase::Cancelled, None);
                }
            }
            let observation = self.source.next(self.cursor.as_mut().unwrap());
            if cancel.is_cancelled() {
                return self.finish(Phase::Cancelled, None);
            }
            let mut guard = state.lock().unwrap();
            if guard.closed
                || guard.cancelled.contains(&self.task)
                || !guard.cache.contains(&self.task)
            {
                return JobStep::Completed;
            }
            let mut row = match observation {
                Ok(Some(row)) => row,
                Ok(None) => {
                    drop(guard);
                    return self.finish(Phase::Completed, None);
                }
                Err(error) => {
                    drop(guard);
                    return self.finish(Phase::Failed, Some(error));
                }
            };
            self.observed += 1;
            self.directories += u64::from(row.entry.kind == Kind::Directory);
            guard
                .cache
                .observations(&self.task, self.observed, self.directories);
            if let Some(error) = row.issue.take() {
                let _ = guard.cache.record_issue(&self.task, error);
            }
            if (row.entry.kind == Kind::Directory) != (self.category == Category::Directories) {
                continue;
            }
            if row.raw_name.is_empty()
                || row.raw_name.len() > 255
                || row.raw_name.contains(&0)
                || row.raw_name.contains(&b'/')
                || row.raw_name == b"."
                || row.raw_name == b".."
            {
                drop(guard);
                return self.finish(Phase::Failed, Some(issue("INVALID_ARGUMENT")));
            }
            row.entry.parent_id = Some(self.target.clone());
            if row.entry.kind == Kind::Directory {
                let index = &self.index;
                let registered = index.register(
                    DirectoryKey {
                        parent_id: Some(self.target.clone()),
                        raw_name: row.raw_name,
                    },
                    Origin::Foreground,
                );
                let result = registered.and_then(|id| {
                    row.entry.entry_id = id.clone();
                    index.expose(&id, row.entry.clone())
                });
                if let Err(error) = result {
                    drop(guard);
                    return self.finish(
                        Phase::Completed,
                        Some(issue(if error == IndexError::ResourceLimit {
                            "RESOURCE_LIMIT"
                        } else {
                            "ENTRY_CHANGED"
                        })),
                    );
                }
            } else {
                row.entry.entry_id = format!("{}-entry-{}", self.task, self.observed);
            }
            let appended = guard.cache.append(&self.task, row.entry);
            let retired = guard.retired();
            drop(guard);
            if let Some(scheduler) = self.scheduler.upgrade() {
                cancel_retired(&scheduler, retired);
            }
            if appended != Ok(true) {
                return JobStep::Completed;
            }
            if start.elapsed() >= Duration::from_millis(20) {
                self.publish();
                return JobStep::Yield;
            }
        }
        self.publish();
        JobStep::Yield
    }
    fn stopped(&mut self, status: Status) {
        let (phase, error) = match status {
            Status::Cancelled => (Phase::Cancelled, None),
            Status::Failed => (Phase::Failed, Some(issue("INTERNAL"))),
            Status::ResourceLimit => (Phase::Completed, Some(issue("RESOURCE_LIMIT"))),
            _ => (Phase::Completed, None),
        };
        self.finish(phase, error);
        self.publish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::index::DirectoryIndex;
    use crate::domain::model::{Coverage, Field, FollowPolicy};
    use std::{
        collections::VecDeque,
        sync::{atomic::AtomicUsize, mpsc},
        thread,
    };
    #[derive(Default)]
    struct Counts {
        opened: AtomicUsize,
        next: AtomicUsize,
        closed: AtomicUsize,
    }
    struct Cursor {
        counts: Arc<Counts>,
    }
    impl Drop for Cursor {
        fn drop(&mut self) {
            self.counts.closed.fetch_add(1, Ordering::SeqCst);
        }
    }
    struct Source {
        rows: VecDeque<ListedEntry>,
        counts: Arc<Counts>,
        block: Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>,
        block_open: Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>,
        open_error: Option<FsIssue>,
        end_error: Option<FsIssue>,
    }
    impl ListingSource for Source {
        type Cursor = Cursor;
        fn open(&mut self, _: &str) -> Result<Cursor, FsIssue> {
            if let Some((ready, release)) = self.block_open.take() {
                ready.send(()).unwrap();
                release.recv().unwrap();
            }
            if let Some(error) = self.open_error.take() {
                return Err(error);
            }
            self.counts.opened.fetch_add(1, Ordering::SeqCst);
            Ok(Cursor {
                counts: self.counts.clone(),
            })
        }
        fn next(&mut self, _: &mut Cursor) -> Result<Option<ListedEntry>, FsIssue> {
            self.counts.next.fetch_add(1, Ordering::SeqCst);
            if let Some((ready, release)) = self.block.take() {
                ready.send(()).unwrap();
                release.recv().unwrap();
            }
            if self.rows.is_empty() {
                if let Some(error) = self.end_error.take() {
                    return Err(error);
                }
            }
            Ok(self.rows.pop_front())
        }
    }
    fn entry(name: &str, kind: Kind) -> ListedEntry {
        ListedEntry {
            raw_name: name.as_bytes().to_vec(),
            issue: None,
            entry: Entry {
                entry_id: "ignored-source-id".into(),
                parent_id: None,
                display_name: name.into(),
                kind,
                hidden: Field::Known {
                    value: name.starts_with('.'),
                },
                special_type: Field::Unsupported,
                modified_at: Field::Unknown,
                own_logical_bytes: Field::Known {
                    value: 1_u64.into(),
                },
                own_allocated_bytes: Field::Known {
                    value: 8_u64.into(),
                },
                observed_at: "2026-09-13T00:00:00Z".into(),
                follow_policy: FollowPolicy::Never,
            },
        }
    }
    fn source(rows: Vec<ListedEntry>) -> (Source, Arc<Counts>) {
        let counts = Arc::new(Counts::default());
        (
            Source {
                rows: rows.into(),
                counts: counts.clone(),
                block: None,
                block_open: None,
                open_error: None,
                end_error: None,
            },
            counts,
        )
    }
    fn setup(
        budget: usize,
        ttl: u64,
        generation: u64,
        scheduler: Arc<Scheduler>,
    ) -> (ListingService, String) {
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
        (
            ListingService::new(
                Scope {
                    session_id: "fixture".into(),
                    generation,
                },
                index.into(),
                scheduler,
                ListingCache::new(budget, ttl, 1_048_576),
            ),
            root,
        )
    }
    fn until(mut predicate: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !predicate() {
            assert!(Instant::now() < deadline, "fixture condition timed out");
            thread::yield_now();
        }
    }
    fn start(
        service: &ListingService,
        target: &str,
        category: Category,
        source: Source,
    ) -> ListingHandle {
        service
            .start(target, category, 2, "2026-09-13T00:00:00Z", 0, source)
            .unwrap()
    }
    #[test]
    fn demand_parks_one_cursor_and_replay_does_not_collect_more() {
        let scheduler = Arc::new(Scheduler::default());
        let (service, root) = setup(2_000_000, 1000, 1, scheduler.clone());
        let (input, counts) = source(
            (0..9)
                .map(|n| entry(&format!("file{n}"), Kind::RegularFile))
                .collect(),
        );
        let handle = start(&service, &root, Category::Files, input);
        until(|| scheduler.counts().2 == 1);
        assert_eq!(counts.next.load(Ordering::SeqCst), 4);
        assert_eq!(scheduler.counts().0, 0);
        assert_eq!(scheduler.handles().open(), 1);
        let first = service.page(&handle.task_id, None, 2, 1).unwrap();
        for _ in 0..100 {
            assert_eq!(
                service.page(&handle.task_id, None, 2, 1).unwrap().entries,
                first.entries
            );
        }
        assert_eq!(counts.next.load(Ordering::SeqCst), 4);
        let second = service
            .page(&handle.task_id, first.next_cursor.as_deref(), 2, 2)
            .unwrap();
        until(|| counts.next.load(Ordering::SeqCst) == 6 && scheduler.counts().2 == 1);
        assert_eq!(second.entries[0].display_name, "file2");
        let mut names: Vec<_> = first
            .entries
            .into_iter()
            .chain(second.entries)
            .map(|e| e.display_name)
            .collect();
        let mut cursor = second.next_cursor;
        while let Some(next) = cursor {
            let page = service.page(&handle.task_id, Some(&next), 2, 3).unwrap();
            names.extend(page.entries.into_iter().map(|e| e.display_name));
            cursor = page.next_cursor;
            if names.len() < 9 {
                thread::yield_now();
            }
        }
        assert_eq!(
            names,
            (0..9).map(|n| format!("file{n}")).collect::<Vec<_>>()
        );
        until(|| scheduler.handles().open() == 0);
        assert_eq!(counts.opened.load(Ordering::SeqCst), 1);
        assert_eq!(counts.closed.load(Ordering::SeqCst), 1);
        assert_eq!(
            service.work(&handle.task_id, 3).unwrap().phase,
            Phase::Completed
        );
    }
    #[test]
    fn duplicate_start_preserves_demand_and_background_directory_identity() {
        let scheduler = Arc::new(Scheduler::default());
        let (service, root) = setup(2_000_000, 1000, 1, scheduler.clone());
        let existing = service
            .index
            .register(
                DirectoryKey {
                    parent_id: Some(root.clone()),
                    raw_name: b"child0".to_vec(),
                },
                Origin::Background,
            )
            .unwrap();
        let (input, counts) = source(
            (0..8)
                .map(|n| entry(&format!("child{n}"), Kind::Directory))
                .collect(),
        );
        let handle = start(&service, &root, Category::Directories, input);
        until(|| scheduler.counts().2 == 1);
        let first = service.page(&handle.task_id, None, 2, 1).unwrap();
        assert_eq!(first.entries[0].entry_id, existing);
        service
            .page(&handle.task_id, first.next_cursor.as_deref(), 2, 1)
            .unwrap();
        let (unused, unused_counts) = source(vec![]);
        assert_eq!(
            start(&service, &root, Category::Directories, unused),
            handle
        );
        until(|| counts.next.load(Ordering::SeqCst) == 6 && scheduler.counts().2 == 1);
        assert_eq!(unused_counts.opened.load(Ordering::SeqCst), 0);
        assert_eq!(service.index.len(), 7);
    }
    #[test]
    fn ttl_and_budget_eviction_cancel_parked_iterators_and_expire_work_and_cursor() {
        let scheduler = Arc::new(Scheduler::default());
        let (service, root) = setup(100_000, 10, 1, scheduler.clone());
        let (input, first_counts) = source(
            (0..8)
                .map(|n| entry(&format!("f{n}"), Kind::RegularFile))
                .collect(),
        );
        let first = start(&service, &root, Category::Files, input);
        until(|| scheduler.counts().2 == 1);
        let page = service.page(&first.task_id, None, 2, 0).unwrap();
        let (input, second_counts) = source(
            (0..8)
                .map(|n| entry(&format!("d{n}"), Kind::Directory))
                .collect(),
        );
        let second = start(&service, &root, Category::Directories, input);
        until(|| first_counts.closed.load(Ordering::SeqCst) == 1 && scheduler.counts().2 == 1);
        assert_eq!(
            service.work(&first.task_id, 0),
            Err(CacheError::TaskExpired)
        );
        assert_eq!(
            service.page(&first.task_id, page.next_cursor.as_deref(), 2, 0),
            Err(CacheError::CursorExpired)
        );
        service.expire(10);
        assert_eq!(
            service.work(&second.task_id, 10),
            Err(CacheError::TaskExpired)
        );
        assert_eq!(second_counts.closed.load(Ordering::SeqCst), 1);
        assert_eq!(scheduler.handles().open(), 0);
        assert_eq!(service.managed_listing_bytes(), 0);
    }
    #[test]
    fn cancellation_during_io_is_prompt_and_late_entry_is_discarded() {
        let scheduler = Arc::new(Scheduler::default());
        let (service, root) = setup(2_000_000, 1000, 1, scheduler.clone());
        let (mut input, counts) = source(vec![entry("late", Kind::Directory)]);
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        input.block = Some((ready_tx, release_rx));
        let handle = start(&service, &root, Category::Directories, input);
        ready_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        let start = Instant::now();
        service.cancel(&handle.task_id).unwrap();
        assert!(start.elapsed() < Duration::from_millis(100));
        let page = service.page(&handle.task_id, None, 2, 1).unwrap();
        assert_eq!(page.work.phase, Phase::Running);
        assert_eq!(page.work.wait_reason, Some(WaitReason::Draining));
        assert!(page.entries.is_empty());
        assert_eq!(scheduler.handles().open(), 1);
        release_tx.send(()).unwrap();
        until(|| counts.closed.load(Ordering::SeqCst) == 1 && scheduler.handles().open() == 0);
        assert_eq!(service.index.len(), 1);
        assert_eq!(
            service.work(&handle.task_id, 1).unwrap().phase,
            Phase::Cancelled
        );
        assert_eq!(scheduler.handles().open(), 0);
    }
    #[test]
    fn close_keeps_old_io_draining_and_new_generation_uses_same_pool() {
        let scheduler = Arc::new(Scheduler::default());
        let (old, root) = setup(2_000_000, 1000, 1, scheduler.clone());
        let (mut input, _) = source(vec![entry("late", Kind::Directory)]);
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        input.block = Some((ready_tx, release_rx));
        start(&old, &root, Category::Directories, input);
        ready_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        old.close();
        let (new, new_root) = setup(2_000_000, 1000, 2, scheduler.clone());
        let (input, counts) = source(vec![entry("new", Kind::Directory)]);
        let handle = start(&new, &new_root, Category::Directories, input);
        assert_eq!(
            new.work(&handle.task_id, 0).unwrap().wait_reason,
            Some(WaitReason::Draining)
        );
        assert_eq!(counts.opened.load(Ordering::SeqCst), 0);
        release_tx.send(()).unwrap();
        until(|| new.work(&handle.task_id, 1).unwrap().phase.terminal());
        assert_eq!(old.index.len(), 1);
        assert_eq!(
            new.page(&handle.task_id, None, 2, 1).unwrap().entries[0].display_name,
            "new"
        );
    }
    #[test]
    fn metadata_errors_keep_unknown_entries_and_root_failure_is_not_empty_success() {
        let scheduler = Arc::new(Scheduler::default());
        let (service, root) = setup(2_000_000, 1000, 1, scheduler.clone());
        let mut unreadable = entry("gone", Kind::Unknown);
        let mut error = issue("NOT_FOUND");
        error.scope = IssueScope::Entry;
        error.native_code = Some(2);
        unreadable.issue = Some(error.clone());
        unreadable.entry.own_logical_bytes = Field::Error { issue: error };
        let (input, _) = source(vec![
            entry("directory", Kind::Directory),
            unreadable,
            entry("healthy", Kind::RegularFile),
        ]);
        let handle = start(&service, &root, Category::Files, input);
        until(|| service.work(&handle.task_id, 1).unwrap().phase.terminal());
        let page = service.page(&handle.task_id, None, 2, 1).unwrap();
        assert_eq!(page.coverage, Coverage::Partial);
        assert_eq!(page.entries.len(), 2);
        assert_eq!(page.entries[0].kind, Kind::Unknown);
        assert_eq!(page.issues.samples[0].native_code, Some(2));
        let (mut input, _) = source(vec![]);
        input.open_error = Some(issue("PERMISSION_DENIED"));
        let failed = start(&service, &root, Category::Directories, input);
        until(|| service.work(&failed.task_id, 1).unwrap().phase.terminal());
        let page = service.page(&failed.task_id, None, 2, 1).unwrap();
        assert_eq!(page.work.phase, Phase::Failed);
        assert_eq!(page.coverage, Coverage::Partial);
        assert!(page.entries.is_empty());
    }
    #[test]
    fn parked_limit_becomes_partial_resource_limit_and_keeps_collected_pages() {
        let scheduler = Arc::new(Scheduler::new(64, 0, 320));
        let (service, root) = setup(2_000_000, 1000, 1, scheduler.clone());
        let (input, counts) = source(
            (0..9)
                .map(|n| entry(&format!("f{n}"), Kind::RegularFile))
                .collect(),
        );
        let handle = start(&service, &root, Category::Files, input);
        until(|| service.work(&handle.task_id, 1).unwrap().phase.terminal());
        let first = service.page(&handle.task_id, None, 2, 1).unwrap();
        let last = service
            .page(&handle.task_id, first.next_cursor.as_deref(), 2, 1)
            .unwrap();
        assert_eq!(first.entries.len() + last.entries.len(), 4);
        assert_eq!(last.coverage, Coverage::Partial);
        assert!(last.next_cursor.is_none());
        assert!(last.issues.counts.contains_key("RESOURCE_LIMIT"));
        until(|| counts.closed.load(Ordering::SeqCst) == 1);
    }
    #[test]
    fn scan_shares_directory_ids_summaries_and_global_handles_with_parked_listing() {
        use crate::domain::model::{Measure, Reason};
        use crate::scan::{
            budgeted_source::BudgetedSource,
            usage::{
                FileIdentity, Metric, Observation, ObservationSource, ScanLimits, UsageScanner,
            },
        };
        struct ScanSource;
        impl ObservationSource for ScanSource {
            type Cursor = bool;
            fn open(&mut self, _: &Observation) -> Result<bool, FsIssue> {
                Ok(false)
            }
            fn next(&mut self, done: &mut bool) -> Result<Option<Observation>, FsIssue> {
                if *done {
                    return Ok(None);
                }
                *done = true;
                Ok(Some(Observation {
                    node: 2,
                    raw_name: b"child0".to_vec(),
                    kind: Kind::Directory,
                    identity: Some(FileIdentity {
                        device: 1,
                        inode: 2,
                    }),
                    link_count: 1,
                    mount: 1,
                    logical: Metric::Known(3),
                    allocated: Metric::Known(8),
                }))
            }
        }
        let scheduler = Arc::new(Scheduler::new(64, 64, 2));
        let (service, root) = setup(2_000_000, 1000, 1, scheduler.clone());
        let (input, _) = source(
            (0..8)
                .map(|n| entry(&format!("child{n}"), Kind::Directory))
                .collect(),
        );
        let handle = start(&service, &root, Category::Directories, input);
        until(|| scheduler.counts().2 == 1);
        let page = service.page(&handle.task_id, None, 2, 1).unwrap();
        let id = page.entries[0].entry_id.clone();
        let before = service.index.managed_bytes();
        let root_observation = Observation {
            node: 1,
            raw_name: b"root".to_vec(),
            kind: Kind::Directory,
            identity: Some(FileIdentity {
                device: 1,
                inode: 1,
            }),
            link_count: 1,
            mount: 1,
            logical: Metric::Known(2),
            allocated: Metric::Known(8),
        };
        let mut scanner = UsageScanner::new(
            BudgetedSource::new(ScanSource, scheduler.handles()),
            root_observation,
            service.index.clone(),
            ScanLimits::default(),
            Cancellation::default(),
        )
        .unwrap();
        assert_eq!(scheduler.handles().open(), 2);
        scanner.run("2026-09-13T00:00:00Z");
        assert_eq!(scheduler.handles().peak(), 2);
        assert_eq!(scheduler.handles().open(), 1);
        assert_eq!(service.index.managed_bytes(), before);
        let child_usage = service.index.usage(&id).unwrap().unwrap();
        assert!(
            matches!(child_usage.logical, Measure::Partial { ref observed_bytes, ref reasons } if *observed_bytes == 3_u64.into() && reasons.contains(&Reason::ResourceLimit))
        );
        assert_eq!(service.index.entry(&id).unwrap(), page.entries[0]);
        assert_eq!(
            service.index.usage(&root).unwrap().unwrap(),
            scanner.index.usage(scanner.root_id()).unwrap().unwrap()
        );
        service.close();
        assert_eq!(scheduler.handles().open(), 0);
    }
    #[test]
    fn a_new_revision_updates_metadata_without_changing_ids_or_accumulating_index_cost() {
        let scheduler = Arc::new(Scheduler::default());
        let (service, root) = setup(2_000_000, 10, 1, scheduler);
        let (input, _) = source(vec![entry("child", Kind::Directory)]);
        let old = start(&service, &root, Category::Directories, input);
        until(|| service.work(&old.task_id, 0).unwrap().phase.terminal());
        let old_page = service.page(&old.task_id, None, 2, 0).unwrap();
        let initial_cost = service.index.managed_bytes();
        service.expire(10);
        let mut changed = entry("child", Kind::Directory);
        changed.entry.observed_at = "2026-09-13T00:01:00Z".into();
        changed.entry.own_logical_bytes = Field::Known {
            value: 9_u64.into(),
        };
        let (input, _) = source(vec![changed]);
        let new = start(&service, &root, Category::Directories, input);
        until(|| service.work(&new.task_id, 0).unwrap().phase.terminal());
        let new_page = service.page(&new.task_id, None, 2, 0).unwrap();
        assert_ne!(old.revision, new.revision);
        assert_eq!(old_page.entries[0].entry_id, new_page.entries[0].entry_id);
        assert_eq!(
            old_page.entries[0].own_logical_bytes,
            Field::Known {
                value: 1_u64.into()
            }
        );
        assert_eq!(
            new_page.entries[0].own_logical_bytes,
            Field::Known {
                value: 9_u64.into()
            }
        );
        assert_eq!(new_page.coverage, Coverage::Complete);
        assert_eq!(service.index.managed_bytes(), initial_cost);
    }
    #[test]
    fn first_read_selects_page_limit_independently_of_initial_collection_demand() {
        let scheduler = Arc::new(Scheduler::default());
        let (service, root) = setup(2_000_000, 1000, 1, scheduler.clone());
        let (input, counts) = source(
            (0..10)
                .map(|n| entry(&format!("f{n}"), Kind::RegularFile))
                .collect(),
        );
        let handle = start(&service, &root, Category::Files, input);
        until(|| scheduler.counts().2 == 1);
        assert_eq!(counts.next.load(Ordering::SeqCst), 4);
        let first = service.page(&handle.task_id, None, 1000, 0).unwrap();
        assert_eq!(first.entries.len(), 4);
        let (unused, unused_counts) = source(vec![]);
        assert_eq!(start(&service, &root, Category::Files, unused), handle);
        until(|| service.work(&handle.task_id, 0).unwrap().phase.terminal());
        assert_eq!(
            service.page(&handle.task_id, first.next_cursor.as_deref(), 200, 0),
            Err(CacheError::InvalidArgument)
        );
        let last = service
            .page(&handle.task_id, first.next_cursor.as_deref(), 1000, 0)
            .unwrap();
        assert_eq!(last.entries.len(), 6);
        assert!(last.next_cursor.is_none());
        assert_eq!(unused_counts.opened.load(Ordering::SeqCst), 0);
    }
    #[test]
    fn parked_data_emits_availability_and_cancel_emits_terminal_without_throttle_delay() {
        use crate::runtime::events::EventChannel;
        let scheduler = Arc::new(Scheduler::default());
        let (mut service, root) = setup(2_000_000, 1000, 1, scheduler.clone());
        let (sink, receiver) = EventChannel::bounded(16);
        service.sink = Arc::new(sink);
        let (input, _) = source(
            (0..8)
                .map(|n| entry(&format!("file{n}"), Kind::RegularFile))
                .collect(),
        );
        let handle = start(&service, &root, Category::Files, input);
        until(|| scheduler.counts().2 == 1);
        let event = receiver.recv_timeout(Duration::from_secs(3)).unwrap();
        assert_eq!(event.kind, EventKind::ListingAvailable);
        assert_eq!(event.task_id, handle.task_id);
        assert!(!serde_json::to_string(&event).unwrap().contains("file0"));
        service.cancel(&handle.task_id).unwrap();
        let terminal = receiver.recv_timeout(Duration::from_secs(3)).unwrap();
        assert_eq!(terminal.kind, EventKind::Terminal);
        assert!(terminal.sequence > event.sequence);
        assert_eq!(
            service.work(&handle.task_id, 1).unwrap().phase,
            Phase::Cancelled
        );
        assert_eq!(
            service
                .page(&handle.task_id, None, 2, 1)
                .unwrap()
                .entries
                .len(),
            2
        );
        assert_eq!(scheduler.handles().open(), 0);
    }
    #[test]
    fn cancellation_wins_over_a_late_open_error_and_does_not_publish_it() {
        let scheduler = Arc::new(Scheduler::default());
        let (service, root) = setup(2_000_000, 1000, 1, scheduler.clone());
        let (mut input, counts) = source(vec![]);
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        input.block_open = Some((ready_tx, release_rx));
        input.open_error = Some(issue("PERMISSION_DENIED"));
        let handle = start(&service, &root, Category::Directories, input);
        ready_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        service.cancel(&handle.task_id).unwrap();
        assert_eq!(
            service.work(&handle.task_id, 1).unwrap().wait_reason,
            Some(WaitReason::Draining)
        );
        release_tx.send(()).unwrap();
        until(|| service.work(&handle.task_id, 1).unwrap().phase.terminal());
        let work = service.work(&handle.task_id, 1).unwrap();
        assert_eq!(work.phase, Phase::Cancelled);
        assert!(work.issues.counts.is_empty());
        assert_eq!(counts.next.load(Ordering::SeqCst), 0);
        until(|| scheduler.handles().open() == 0);
    }
}
