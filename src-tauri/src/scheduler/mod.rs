//! Two fixed application-wide OS workers. Never recreate this pool per root.
use crate::{
    domain::controller::{Cancellation, Scope},
    runtime::memory::{ByteBudget, Reservation},
};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    Foreground,
    Background,
}
impl Lane {
    fn index(self) -> usize {
        match self {
            Self::Foreground => 0,
            Self::Background => 1,
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobStep {
    Yield,
    Paused,
    Completed,
    Failed,
    ResourceLimit,
}
pub trait Job: Send + 'static {
    fn step(&mut self, cancel: &Cancellation, handles: &HandleBudget) -> JobStep;
    /// Runs once after retirement, outside all scheduler locks. No new I/O.
    fn stopped(&mut self, _status: Status) {}
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Queued,
    Running,
    Paused,
    Completed,
    Cancelled,
    Failed,
    ResourceLimit,
}
impl Status {
    pub fn terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Cancelled | Self::Failed | Self::ResourceLimit
        )
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wait {
    Slot,
    Draining,
    PageDemand,
}
#[derive(Clone)]
pub struct Ticket {
    inner: Arc<TicketData>,
}
struct TicketData {
    id: String,
    scope: Scope,
    cancel: Cancellation,
    status: Arc<Mutex<Status>>,
    wake_requested: Arc<AtomicBool>,
    _memory: Reservation,
}
impl Ticket {
    fn cancel(&self) {
        self.inner.cancel.cancel();
    }
    pub fn status(&self) -> Status {
        *self.inner.status.lock().unwrap()
    }
    pub fn id(&self) -> &str {
        &self.inner.id
    }
    fn set(&self, status: Status) {
        *self.inner.status.lock().unwrap() = status;
    }
}
struct Pending {
    // Drop the job before the ticket can release its shared reservation.
    job: Box<dyn Job>,
    ticket: Ticket,
    lane: Lane,
}
impl Drop for Pending {
    fn drop(&mut self) {
        let status = self.ticket.status();
        // A task finalizer must not take down a fixed application worker.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.job.stopped(status)));
    }
}
struct State {
    queues: [VecDeque<Pending>; 2],
    active: [Option<Ticket>; 2],
    parked: BTreeMap<String, Pending>,
    shutdown: bool,
}
struct Shared {
    state: Mutex<State>,
    ready: Condvar,
    queue_limit: usize,
    parked_limit: usize,
    handles: HandleBudget,
    budget: ByteBudget,
    _memory: Reservation,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitError {
    ResourceLimit,
    Duplicate,
    Closed,
    Missing,
}
#[derive(Clone)]
pub struct HandleBudget {
    inner: Arc<HandleCount>,
}
struct HandleCount {
    open: AtomicUsize,
    peak: AtomicUsize,
    limit: usize,
    _memory: Reservation,
}
pub struct HandlePermit {
    inner: Arc<HandleCount>,
}
impl HandleBudget {
    pub fn same_pool(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }
    pub fn new(limit: usize) -> Self {
        Self::with_budget(limit, &ByteBudget::new(usize::MAX)).expect("standalone handle counter")
    }
    fn with_budget(limit: usize, budget: &ByteBudget) -> Result<Self, SubmitError> {
        let memory = budget
            .reserve(512)
            .map_err(|_| SubmitError::ResourceLimit)?;
        Ok(Self {
            inner: Arc::new(HandleCount {
                open: AtomicUsize::new(0),
                peak: AtomicUsize::new(0),
                limit,
                _memory: memory,
            }),
        })
    }
    pub fn acquire(&self) -> Result<HandlePermit, SubmitError> {
        let count = self
            .inner
            .open
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |open| {
                (open < self.inner.limit).then_some(open + 1)
            })
            .map_err(|_| SubmitError::ResourceLimit)?
            + 1;
        self.inner.peak.fetch_max(count, Ordering::Relaxed);
        Ok(HandlePermit {
            inner: self.inner.clone(),
        })
    }
    pub fn open(&self) -> usize {
        self.inner.open.load(Ordering::Acquire)
    }
    pub fn peak(&self) -> usize {
        self.inner.peak.load(Ordering::Acquire)
    }
}
impl Drop for HandlePermit {
    fn drop(&mut self) {
        self.inner.open.fetch_sub(1, Ordering::AcqRel);
    }
}
pub struct Scheduler {
    shared: Arc<Shared>,
}
impl Default for Scheduler {
    fn default() -> Self {
        Self::new(64, 64, 320)
    }
}
impl Scheduler {
    /// Standalone fixture scheduler. NativeRuntime requires with_budget instead.
    pub fn new(queue_limit: usize, parked_limit: usize, handle_limit: usize) -> Self {
        Self::with_budget(
            queue_limit,
            parked_limit,
            handle_limit,
            ByteBudget::new(usize::MAX),
        )
        .expect("valid standalone scheduler")
    }
    pub fn with_budget(
        queue_limit: usize,
        parked_limit: usize,
        handle_limit: usize,
        budget: ByteBudget,
    ) -> Result<Self, SubmitError> {
        let slots = queue_limit
            .checked_add(1)
            .ok_or(SubmitError::ResourceLimit)?;
        // Ring capacity rounding, parked nodes, fixed worker/condvar/handle
        // metadata and bounded stack work. OS thread stacks/RSS are separate.
        let bytes = slots
            .checked_mul(4 * std::mem::size_of::<Pending>())
            .and_then(|n| {
                parked_limit
                    .checked_mul(std::mem::size_of::<Pending>() + 512)
                    .and_then(|p| n.checked_add(p))
            })
            .and_then(|n| n.checked_add(32 * 1024))
            .ok_or(SubmitError::ResourceLimit)?;
        let memory = budget
            .reserve(bytes)
            .map_err(|_| SubmitError::ResourceLimit)?;
        let mut queues = [VecDeque::new(), VecDeque::new()];
        for queue in &mut queues {
            queue
                .try_reserve_exact(slots)
                .map_err(|_| SubmitError::ResourceLimit)?;
            if queue.capacity() > slots.saturating_mul(2) {
                return Err(SubmitError::ResourceLimit);
            }
        }
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                queues,
                active: [None, None],
                parked: BTreeMap::new(),
                shutdown: false,
            }),
            ready: Condvar::new(),
            queue_limit,
            parked_limit,
            handles: HandleBudget::with_budget(handle_limit, &budget)?,
            budget,
            _memory: memory,
        });
        for lane in [Lane::Foreground, Lane::Background] {
            let worker_shared = shared.clone();
            if thread::Builder::new()
                .name(
                    match lane {
                        Lane::Foreground => "directory-foreground",
                        Lane::Background => "directory-background",
                    }
                    .into(),
                )
                .spawn(move || worker(worker_shared, lane))
                .is_err()
            {
                shared.state.lock().unwrap().shutdown = true;
                shared.ready.notify_all();
                return Err(SubmitError::ResourceLimit);
            }
        }
        Ok(Self { shared })
    }
    pub fn fixed_bytes(&self) -> usize {
        self.shared._memory.bytes() + self.shared.handles.inner._memory.bytes()
    }
    pub fn uses_budget(&self, budget: &ByteBudget) -> bool {
        self.shared.budget.same_pool(budget)
    }
    pub fn handles(&self) -> HandleBudget {
        self.shared.handles.clone()
    }
    pub fn submit(
        &self,
        id: String,
        scope: Scope,
        lane: Lane,
        job: impl Job,
    ) -> Result<Ticket, SubmitError> {
        let mut state = self.shared.state.lock().unwrap();
        if state.shutdown {
            return Err(SubmitError::Closed);
        }
        if state.queues.iter().map(VecDeque::len).sum::<usize>() >= self.shared.queue_limit {
            return Err(SubmitError::ResourceLimit);
        }
        if state.active.iter().flatten().any(|t| t.inner.id == id)
            || state.parked.contains_key(&id)
            || state
                .queues
                .iter()
                .flatten()
                .any(|p| p.ticket.inner.id == id)
        {
            return Err(SubmitError::Duplicate);
        }
        // Arguments are moved, not copied. Ticket clones share this owner;
        // include the parked map's one copied key and the boxed job allocation.
        let bytes = id
            .capacity()
            .checked_add(scope.session_id.capacity())
            .and_then(|n| n.checked_add(id.len().saturating_mul(2)))
            .and_then(|n| n.checked_add(std::mem::size_of_val(&job)))
            .and_then(|n| n.checked_add(2048))
            .ok_or(SubmitError::ResourceLimit)?;
        let memory = self
            .shared
            .budget
            .reserve(bytes)
            .map_err(|_| SubmitError::ResourceLimit)?;
        let ticket = Ticket {
            inner: Arc::new(TicketData {
                id,
                scope,
                cancel: Cancellation::default(),
                status: Arc::new(Mutex::new(Status::Queued)),
                wake_requested: Arc::new(AtomicBool::new(false)),
                _memory: memory,
            }),
        };
        state.queues[lane.index()].push_back(Pending {
            ticket: ticket.clone(),
            lane,
            job: Box::new(job),
        });
        self.shared.ready.notify_all();
        Ok(ticket)
    }
    /// Demand may arrive while step() is about to return Paused. Remember that
    /// wake under the same lock used to park, so the request cannot be lost.
    pub fn resume(&self, id: &str) -> Result<(), SubmitError> {
        let mut state = self.shared.state.lock().unwrap();
        if state.shutdown {
            return Err(SubmitError::Closed);
        }
        if let Some(ticket) = state.active.iter().flatten().find(|t| t.inner.id == id) {
            ticket.inner.wake_requested.store(true, Ordering::Release);
            return Ok(());
        }
        if state
            .queues
            .iter()
            .flatten()
            .any(|p| p.ticket.inner.id == id)
        {
            return Ok(());
        }
        if state.queues.iter().map(VecDeque::len).sum::<usize>() >= self.shared.queue_limit {
            return Err(SubmitError::ResourceLimit);
        }
        let pending = state.parked.remove(id).ok_or(SubmitError::Missing)?;
        pending.ticket.set(Status::Queued);
        let lane = pending.lane.index();
        state.queues[lane].push_back(pending);
        self.shared.ready.notify_all();
        Ok(())
    }
    pub fn cancel_task(&self, ticket: &Ticket) -> bool {
        let retired = {
            let mut state = self.shared.state.lock().unwrap();
            if ticket.status().terminal() {
                return false;
            }
            ticket.cancel();
            if let Some(index) = state.queues.iter().position(|queue| {
                queue.iter().any(|p| {
                    p.ticket.inner.id == ticket.inner.id
                        && p.ticket.inner.scope == ticket.inner.scope
                })
            }) {
                let position = state.queues[index]
                    .iter()
                    .position(|p| {
                        p.ticket.inner.id == ticket.inner.id
                            && p.ticket.inner.scope == ticket.inner.scope
                    })
                    .unwrap();
                let pending = state.queues[index].remove(position).unwrap();
                pending.ticket.set(Status::Cancelled);
                Some(pending)
            } else if state
                .parked
                .get(&ticket.inner.id)
                .is_some_and(|p| p.ticket.inner.scope == ticket.inner.scope)
            {
                let pending = state.parked.remove(&ticket.inner.id).unwrap();
                pending.ticket.set(Status::Cancelled);
                Some(pending)
            } else {
                None
            }
        };
        self.shared.ready.notify_all();
        drop(retired);
        true
    }
    /// No disk I/O or worker join while holding the control lock. Parked cursor
    /// destructors run after unlock; active blocking calls retain their slot.
    pub fn cancel_scope(&self, scope: &Scope) {
        {
            let state = self.shared.state.lock().unwrap();
            for ticket in state.active.iter().flatten() {
                if &ticket.inner.scope == scope {
                    ticket.cancel();
                }
            }
            for pending in state.queues.iter().flatten().chain(state.parked.values()) {
                if &pending.ticket.inner.scope == scope {
                    pending.ticket.cancel();
                    pending.ticket.set(Status::Cancelled);
                }
            }
        }
        self.shared.ready.notify_all();
        // Remove marked owners individually. No temporary Vec/String/ring
        // allocation is required for close under pressure. Destructors run
        // outside the lock; new submissions are not part of this snapshot.
        loop {
            let retired = {
                let mut state = self.shared.state.lock().unwrap();
                let matches = |p: &Pending| {
                    &p.ticket.inner.scope == scope && p.ticket.status() == Status::Cancelled
                };
                let mut found = None;
                for queue in &mut state.queues {
                    if let Some(index) = queue.iter().position(&matches) {
                        found = queue.remove(index);
                        break;
                    }
                }
                if found.is_none() {
                    let ticket = state
                        .parked
                        .values()
                        .find(|p| matches(p))
                        .map(|p| p.ticket.clone());
                    if let Some(ticket) = ticket {
                        found = state.parked.remove(ticket.id());
                    }
                }
                found
            };
            if retired.is_none() {
                break;
            }
            drop(retired);
        }
    }
    pub fn wait_reason(&self, ticket: &Ticket) -> Option<Wait> {
        let state = self.shared.state.lock().unwrap();
        let status = ticket.status();
        if status == Status::Paused {
            return Some(Wait::PageDemand);
        }
        if status == Status::Running && ticket.inner.cancel.is_cancelled() {
            return Some(Wait::Draining);
        }
        if status != Status::Queued {
            return None;
        }
        let lane = state
            .queues
            .iter()
            .position(|q| q.iter().any(|p| p.ticket.inner.id == ticket.inner.id))?;
        Some(
            if state.active[lane]
                .as_ref()
                .is_some_and(|t| t.inner.cancel.is_cancelled())
            {
                Wait::Draining
            } else {
                Wait::Slot
            },
        )
    }
    pub fn counts(&self) -> (usize, usize, usize) {
        let state = self.shared.state.lock().unwrap();
        (
            state.active.iter().flatten().count(),
            state.queues.iter().map(VecDeque::len).sum(),
            state.parked.len(),
        )
    }
}
impl Drop for Scheduler {
    fn drop(&mut self) {
        let retired = {
            let mut state = self.shared.state.lock().unwrap();
            state.shutdown = true;
            for ticket in state.active.iter().flatten() {
                ticket.cancel();
            }
            for pending in state.queues.iter().flatten().chain(state.parked.values()) {
                pending.ticket.cancel();
                pending.ticket.set(Status::Cancelled);
            }
            (
                std::mem::take(&mut state.queues),
                std::mem::take(&mut state.parked),
            )
        };
        self.shared.ready.notify_all();
        drop(retired);
        // Original workers retain Shared and its reservations through blocked
        // OS calls. No join on the control thread and no replacement workers.
    }
}
fn worker(shared: Arc<Shared>, lane: Lane) {
    let mut current: Option<Pending> = None;
    loop {
        let mut pending = match current.take() {
            Some(pending) => pending,
            None => {
                let mut state = shared.state.lock().unwrap();
                loop {
                    if state.shutdown {
                        return;
                    }
                    if let Some(pending) = state.queues[lane.index()].pop_front() {
                        pending.ticket.set(Status::Running);
                        state.active[lane.index()] = Some(pending.ticket.clone());
                        break pending;
                    }
                    state = shared.ready.wait(state).unwrap();
                }
            }
        };
        pending.ticket.set(Status::Running);
        let step = if pending.ticket.inner.cancel.is_cancelled() {
            JobStep::Completed
        } else {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                pending
                    .job
                    .step(&pending.ticket.inner.cancel, &shared.handles)
            }))
            .unwrap_or(JobStep::Failed)
        };
        let mut state = shared.state.lock().unwrap();
        if state.shutdown || pending.ticket.inner.cancel.is_cancelled() {
            pending.ticket.set(Status::Cancelled);
            state.active[lane.index()] = None;
            drop(state);
            drop(pending);
            continue;
        }
        let step = if step == JobStep::Paused
            && pending
                .ticket
                .inner
                .wake_requested
                .swap(false, Ordering::AcqRel)
        {
            JobStep::Yield
        } else {
            step
        };
        match step {
            JobStep::Yield => {
                pending.ticket.set(Status::Queued);
                // Rotate atomically: after releasing the lock the queue never
                // contains more than its configured number of waiting jobs.
                state.queues[lane.index()].push_back(pending);
                current = state.queues[lane.index()].pop_front();
                if let Some(next) = &current {
                    next.ticket.set(Status::Running);
                }
                state.active[lane.index()] = current.as_ref().map(|p| p.ticket.clone());
            }
            JobStep::Paused => {
                state.active[lane.index()] = None;
                if state.parked.len() >= shared.parked_limit {
                    pending.ticket.set(Status::ResourceLimit);
                    drop(state);
                    drop(pending);
                    continue;
                }
                pending.ticket.set(Status::Paused);
                state
                    .parked
                    .insert(pending.ticket.inner.id.clone(), pending);
            }
            terminal => {
                pending.ticket.set(match terminal {
                    JobStep::Completed => Status::Completed,
                    JobStep::ResourceLimit => Status::ResourceLimit,
                    _ => Status::Failed,
                });
                state.active[lane.index()] = None;
                drop(state);
                drop(pending);
                continue;
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::mpsc,
        time::{Duration, Instant},
    };
    fn scope(generation: u64) -> Scope {
        Scope {
            session_id: "fixture".into(),
            generation,
        }
    }
    fn wait(ticket: &Ticket, status: Status) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while ticket.status() != status {
            assert!(
                Instant::now() < deadline,
                "worker did not reach expected state"
            );
            thread::yield_now();
        }
    }
    struct Block {
        ready: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
        permit: Option<HandlePermit>,
    }
    impl Job for Block {
        fn step(&mut self, _: &Cancellation, handles: &HandleBudget) -> JobStep {
            self.permit = Some(handles.acquire().unwrap());
            self.ready.send(()).unwrap();
            self.release.recv().unwrap();
            JobStep::Completed
        }
    }
    struct Once;
    impl Job for Once {
        fn step(&mut self, _: &Cancellation, _: &HandleBudget) -> JobStep {
            JobStep::Completed
        }
    }
    struct Pause {
        permit: Option<HandlePermit>,
        ran: bool,
    }
    impl Job for Pause {
        fn step(&mut self, _: &Cancellation, handles: &HandleBudget) -> JobStep {
            if self.ran {
                return JobStep::Completed;
            }
            self.ran = true;
            self.permit = Some(handles.acquire().unwrap());
            JobStep::Paused
        }
    }
    fn until(mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !condition() {
            assert!(Instant::now() < deadline, "owner did not drain");
            thread::yield_now();
        }
    }
    #[test]
    fn handle_counter_charge_follows_external_permits_after_scheduler_shutdown() {
        let budget = ByteBudget::new(256 * 1024);
        let scheduler = Scheduler::with_budget(64, 64, 320, budget.clone()).unwrap();
        let handles = scheduler.handles();
        let permit = handles.acquire().unwrap();
        drop(scheduler);
        until(|| budget.used() == 512);
        drop(handles);
        assert_eq!(budget.used(), 512);
        drop(permit);
        assert_eq!(budget.used(), 0);
    }
    #[test]
    fn ticket_clones_share_charged_metadata_until_the_last_actual_owner_drops() {
        let budget = ByteBudget::new(256 * 1024);
        let scheduler = Scheduler::with_budget(64, 64, 320, budget.clone()).unwrap();
        let fixed = scheduler.fixed_bytes();
        assert_eq!(budget.used(), fixed);
        let ticket = scheduler
            .submit("retained".into(), scope(1), Lane::Foreground, Once)
            .unwrap();
        wait(&ticket, Status::Completed);
        until(|| Arc::strong_count(&ticket.inner) == 1);
        let held = budget.used() - fixed;
        assert!(held >= 2048);
        let clone = ticket.clone();
        assert!(Arc::ptr_eq(&ticket.inner, &clone.inner));
        assert_eq!(budget.used(), fixed + held);
        drop(ticket);
        drop(scheduler);
        until(|| budget.used() == held);
        drop(clone);
        assert_eq!(budget.used(), 0);
    }
    #[test]
    fn constructor_and_submission_admit_before_creating_owned_work() {
        let small = ByteBudget::new(1024);
        assert!(Scheduler::with_budget(64, 64, 320, small.clone()).is_err());
        assert_eq!(small.used(), 0);
        let budget = ByteBudget::new(256 * 1024);
        let scheduler = Scheduler::with_budget(64, 64, 320, budget.clone()).unwrap();
        let pressure = budget.reserve(budget.available()).unwrap();
        assert!(matches!(
            scheduler.submit("rejected".into(), scope(1), Lane::Foreground, Once),
            Err(SubmitError::ResourceLimit)
        ));
        assert_eq!(scheduler.counts(), (0, 0, 0));
        assert_eq!(budget.used(), budget.limit());
        drop(pressure);
        drop(scheduler);
        until(|| budget.used() == 0);
    }
    #[test]
    fn saturated_scope_cancel_and_drop_allocate_no_retirement_lists_and_keep_blocked_costs() {
        let budget = ByteBudget::new(256 * 1024);
        let scheduler = Scheduler::with_budget(64, 64, 320, budget.clone()).unwrap();
        let fixed = scheduler.fixed_bytes();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let blocked = scheduler
            .submit(
                "blocked".into(),
                scope(1),
                Lane::Foreground,
                Block {
                    ready: ready_tx,
                    release: release_rx,
                    permit: None,
                },
            )
            .unwrap();
        ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let parked = scheduler
            .submit(
                "parked".into(),
                scope(1),
                Lane::Background,
                Pause {
                    permit: None,
                    ran: false,
                },
            )
            .unwrap();
        wait(&parked, Status::Paused);
        let pressure = budget.reserve(budget.available()).unwrap();
        scheduler.cancel_scope(&scope(1));
        assert_eq!(parked.status(), Status::Cancelled);
        assert_eq!(scheduler.counts(), (1, 0, 0));
        drop(scheduler);
        drop(parked);
        drop(pressure);
        assert!(
            budget.used() > fixed,
            "blocked OS owner retains pool and ticket"
        );
        release_tx.send(()).unwrap();
        wait(&blocked, Status::Cancelled);
        drop(blocked);
        until(|| budget.used() == 0);
        assert!(budget.peak() <= budget.limit());
    }
    #[test]
    fn demand_arriving_before_park_is_not_lost() {
        struct AboutToPark {
            ready: mpsc::Sender<()>,
            release: mpsc::Receiver<()>,
            first: bool,
        }
        impl Job for AboutToPark {
            fn step(&mut self, _: &Cancellation, _: &HandleBudget) -> JobStep {
                if !self.first {
                    return JobStep::Completed;
                }
                self.first = false;
                self.ready.send(()).unwrap();
                self.release.recv().unwrap();
                JobStep::Paused
            }
        }
        let scheduler = Scheduler::default();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let ticket = scheduler
            .submit(
                "demand-race".into(),
                scope(1),
                Lane::Foreground,
                AboutToPark {
                    ready: ready_tx,
                    release: release_rx,
                    first: true,
                },
            )
            .unwrap();
        ready_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        scheduler.resume(ticket.id()).unwrap();
        release_tx.send(()).unwrap();
        wait(&ticket, Status::Completed);
        assert_eq!(scheduler.counts(), (0, 0, 0));
    }
    #[test]
    fn blocking_old_generation_retains_slot_but_control_and_other_lane_progress() {
        let scheduler = Scheduler::new(2, 2, 2);
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let old = scheduler
            .submit(
                "old".into(),
                scope(1),
                Lane::Background,
                Block {
                    ready: ready_tx,
                    release: release_rx,
                    permit: None,
                },
            )
            .unwrap();
        ready_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        let start = Instant::now();
        scheduler.cancel_scope(&scope(1));
        assert!(start.elapsed() < Duration::from_millis(100));
        assert_eq!(scheduler.wait_reason(&old), Some(Wait::Draining));
        let new = scheduler
            .submit("new".into(), scope(2), Lane::Background, Once)
            .unwrap();
        assert_eq!(scheduler.wait_reason(&new), Some(Wait::Draining));
        let front = scheduler
            .submit("front".into(), scope(2), Lane::Foreground, Once)
            .unwrap();
        wait(&front, Status::Completed);
        assert_eq!(scheduler.handles().open(), 1);
        release_tx.send(()).unwrap();
        wait(&old, Status::Cancelled);
        wait(&new, Status::Completed);
        assert!(scheduler.handles().peak() <= 2);
    }
    #[test]
    fn paused_cursor_releases_execution_slot_and_closes_on_generation_change() {
        let scheduler = Scheduler::new(4, 2, 4);
        let paused = scheduler
            .submit(
                "paused".into(),
                scope(1),
                Lane::Foreground,
                Pause {
                    permit: None,
                    ran: false,
                },
            )
            .unwrap();
        wait(&paused, Status::Paused);
        assert_eq!(scheduler.wait_reason(&paused), Some(Wait::PageDemand));
        let later = scheduler
            .submit("later".into(), scope(1), Lane::Foreground, Once)
            .unwrap();
        wait(&later, Status::Completed);
        assert_eq!(scheduler.handles().open(), 1);
        scheduler.cancel_scope(&scope(1));
        assert_eq!(paused.status(), Status::Cancelled);
        assert_eq!(scheduler.handles().open(), 0);
    }
    #[test]
    fn repeated_generation_changes_keep_two_blocked_workers_and_bounded_control_latency() {
        let scheduler = Scheduler::new(64, 64, 320);
        let mut releases = vec![];
        let mut old_tickets = vec![];
        for (n, lane) in [Lane::Foreground, Lane::Background].into_iter().enumerate() {
            let (ready_tx, ready_rx) = mpsc::channel();
            let (release_tx, release_rx) = mpsc::channel();
            let ticket = scheduler
                .submit(
                    format!("old-{n}"),
                    scope(1),
                    lane,
                    Block {
                        ready: ready_tx,
                        release: release_rx,
                        permit: None,
                    },
                )
                .unwrap();
            ready_rx.recv_timeout(Duration::from_secs(3)).unwrap();
            releases.push(release_tx);
            old_tickets.push(ticket);
        }
        scheduler.cancel_scope(&scope(1));
        let mut samples = vec![];
        for generation in 2..=11 {
            for n in 0..64 {
                scheduler
                    .submit(
                        format!("{generation}-{n}"),
                        scope(generation),
                        if n % 2 == 0 {
                            Lane::Foreground
                        } else {
                            Lane::Background
                        },
                        Once,
                    )
                    .unwrap();
            }
            let counts = scheduler.counts();
            assert_eq!(counts, (2, 64, 0));
            let start = Instant::now();
            scheduler.cancel_scope(&scope(generation));
            samples.push(start.elapsed());
            assert_eq!(scheduler.counts(), (2, 0, 0));
            assert_eq!(scheduler.handles().open(), 2);
        }
        assert!(
            samples
                .iter()
                .all(|latency| *latency < Duration::from_millis(100))
        );
        println!(
            "scheduler fixture: cancel_samples=10 max_us={} active_workers=2 open_permits=2",
            samples.iter().max().unwrap().as_micros()
        );
        for release in releases {
            release.send(()).unwrap();
        }
        for ticket in old_tickets {
            wait(&ticket, Status::Cancelled);
        }
    }
    #[test]
    fn individual_cancel_releases_a_parked_cursor_without_cancelling_siblings() {
        let scheduler = Scheduler::new(4, 4, 4);
        let paused = scheduler
            .submit(
                "paused".into(),
                scope(1),
                Lane::Foreground,
                Pause {
                    permit: None,
                    ran: false,
                },
            )
            .unwrap();
        wait(&paused, Status::Paused);
        assert!(scheduler.cancel_task(&paused));
        assert_eq!(paused.status(), Status::Cancelled);
        assert_eq!(scheduler.handles().open(), 0);
        assert!(!scheduler.cancel_task(&paused));
        let sibling = scheduler
            .submit("sibling".into(), scope(1), Lane::Foreground, Once)
            .unwrap();
        wait(&sibling, Status::Completed);
    }
    #[test]
    fn queue_and_handle_limits_are_enforced_and_resume_reuses_worker() {
        let handles = HandleBudget::new(1);
        let permit = handles.acquire().unwrap();
        assert!(matches!(handles.acquire(), Err(SubmitError::ResourceLimit)));
        drop(permit);
        assert_eq!(handles.open(), 0);
        let scheduler = Scheduler::new(1, 1, 2);
        let (tx, rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let blocker = scheduler
            .submit(
                "block".into(),
                scope(1),
                Lane::Foreground,
                Block {
                    ready: tx,
                    release: release_rx,
                    permit: None,
                },
            )
            .unwrap();
        rx.recv_timeout(Duration::from_secs(3)).unwrap();
        let queued = scheduler
            .submit("queued".into(), scope(1), Lane::Foreground, Once)
            .unwrap();
        assert!(matches!(
            scheduler.submit("overflow".into(), scope(1), Lane::Foreground, Once),
            Err(SubmitError::ResourceLimit)
        ));
        release_tx.send(()).unwrap();
        wait(&blocker, Status::Completed);
        wait(&queued, Status::Completed);
        let paused = scheduler
            .submit(
                "pause".into(),
                scope(1),
                Lane::Foreground,
                Pause {
                    permit: None,
                    ran: false,
                },
            )
            .unwrap();
        wait(&paused, Status::Paused);
        scheduler.resume("pause").unwrap();
        wait(&paused, Status::Completed);
    }
}
