//! Root I/O on the existing fixed foreground worker; no replacement threads.
use super::*;
use crate::{
    domain::controller::Cancellation,
    scheduler::{HandleBudget, Job, JobStep, Lane, Status, Ticket},
};
use std::{path::PathBuf, sync::Weak};
use tauri::async_runtime::{Receiver, Sender, channel};
static NEXT_JOB: AtomicU64 = AtomicU64::new(1);
enum Action {
    Selection {
        ticket: SelectionTicket,
        path: PathBuf,
    },
    Refresh(RefreshTicket),
}
enum Outcome {
    Selection(Result<Option<RootData>, FsIssue>),
    Refresh(Result<RootData, RefreshFailure>),
}
struct RootJob {
    runtime: Weak<NativeRuntime>,
    action: Option<Action>,
    sender: Sender<Outcome>,
    publish: Arc<Mutex<bool>>,
    // Payload/result can outlive queue retirement until the receiver consumes it.
    _memory: Arc<Reservation>,
}
impl RootJob {
    fn finish(&mut self, prepared: Result<PreparedRoot, FsIssue>) {
        let Some(action) = self.action.take() else {
            return;
        };
        let Some(runtime) = self.runtime.upgrade() else {
            return;
        };
        let enabled = self.publish.lock().unwrap();
        let prepared = if *enabled {
            prepared
        } else {
            Err(issue("SESSION_CLOSED"))
        };
        let outcome = match action {
            Action::Selection { ticket, .. } => {
                Outcome::Selection(runtime.finish_selection(ticket, prepared.map(Some)))
            }
            Action::Refresh(ticket) => Outcome::Refresh(runtime.finish_refresh(ticket, prepared)),
        };
        let _ = self.sender.try_send(outcome);
    }
}
impl Job for RootJob {
    fn step(&mut self, cancel: &Cancellation, _: &HandleBudget) -> JobStep {
        let Some(runtime) = self.runtime.upgrade() else {
            return JobStep::Completed;
        };
        let prepared = if cancel.is_cancelled() {
            Err(issue("SESSION_CLOSED"))
        } else {
            match self.action.as_ref().expect("one-shot root job") {
                Action::Selection { ticket, path } => runtime.prepare_selection(ticket, path),
                Action::Refresh(ticket) => runtime.prepare_refresh(ticket),
            }
        };
        // The syscall may have completed after cancellation. Do not approve it.
        self.finish(if cancel.is_cancelled() {
            Err(issue("SESSION_CLOSED"))
        } else {
            prepared
        });
        JobStep::Completed
    }
    fn stopped(&mut self, status: Status) {
        if self.action.is_some() {
            self.finish(Err(issue(match status {
                Status::Cancelled => "SESSION_CLOSED",
                Status::ResourceLimit => "RESOURCE_LIMIT",
                _ => "INTERNAL",
            })));
        }
    }
}
impl Drop for RootJob {
    fn drop(&mut self) {
        // submit failure drops the job without a scheduler retirement callback.
        // A panic in preparation also leaves the action available for cleanup.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if self.action.is_some() {
                self.finish(Err(issue("RESOURCE_LIMIT")));
            }
        }));
    }
}
struct PendingRoot {
    receiver: Receiver<Outcome>,
    publish: Arc<Mutex<bool>>,
    ticket: Option<Ticket>,
    scheduler: Weak<Scheduler>,
    _memory: Option<Arc<Reservation>>,
}
impl Drop for PendingRoot {
    fn drop(&mut self) {
        // Linearize abandonment against approval, without waiting for native I/O.
        *self.publish.lock().unwrap() = false;
        if let (Some(scheduler), Some(ticket)) = (self.scheduler.upgrade(), &self.ticket) {
            scheduler.cancel_task(ticket);
        }
    }
}
impl NativeRuntime {
    fn enqueue_root(self: &Arc<Self>, action: Action) -> PendingRoot {
        let (sender, receiver) = channel(1);
        let publish = Arc::new(Mutex::new(true));
        let bytes = 16 * 1024
            + match &action {
                Action::Selection { path, .. } => path.capacity(),
                _ => 0,
            };
        let memory = self.resources.budgets.work.reserve(bytes).map(Arc::new);
        let mut pending = PendingRoot {
            receiver,
            publish: publish.clone(),
            ticket: None,
            scheduler: Arc::downgrade(&self.resources.scheduler),
            _memory: memory.as_ref().ok().cloned(),
        };
        let scope = self.state.lock().unwrap().root_work_scope.clone();
        let action_valid = match &action {
            Action::Selection { ticket, .. } => self
                .state
                .lock()
                .unwrap()
                .controller
                .validate_selection(ticket)
                .map_err(control),
            Action::Refresh(ticket) => self.validate_refresh(ticket),
        };
        if let Err(error) = action_valid {
            let outcome = match action {
                Action::Selection { ticket, .. } => {
                    Outcome::Selection(self.finish_selection(ticket, Err(error)))
                }
                Action::Refresh(ticket) => {
                    Outcome::Refresh(self.finish_refresh(ticket, Err(error)))
                }
            };
            let _ = sender.try_send(outcome);
            return pending;
        }
        let memory = match memory {
            Ok(memory) => memory,
            Err(_) => {
                let outcome = match action {
                    Action::Selection { ticket, .. } => Outcome::Selection(
                        self.finish_selection(ticket, Err(issue("RESOURCE_LIMIT"))),
                    ),
                    Action::Refresh(ticket) => {
                        Outcome::Refresh(self.finish_refresh(ticket, Err(issue("RESOURCE_LIMIT"))))
                    }
                };
                let _ = sender.try_send(outcome);
                return pending;
            }
        };
        let id = format!("root-prepare-{}", NEXT_JOB.fetch_add(1, Ordering::Relaxed));
        let job = RootJob {
            runtime: Arc::downgrade(self),
            action: Some(action),
            sender,
            publish,
            _memory: memory,
        };
        if let Ok(ticket) =
            self.resources
                .scheduler
                .submit(id, scope.clone(), Lane::Foreground, job)
        {
            // Close may have cancelled this scope before submit installed it.
            let stale = self.state.lock().unwrap().root_work_scope != scope;
            if stale {
                self.resources.scheduler.cancel_task(&ticket);
            }
            pending.ticket = Some(ticket);
        }
        pending
    }
    /// Called by a trusted native picker callback. The selected path never crosses
    /// IPC, and cancellation/validation failure keeps the previous root intact.
    pub async fn select_path(
        self: Arc<Self>,
        ticket: SelectionTicket,
        path: Option<PathBuf>,
    ) -> Result<Option<RootData>, FsIssue> {
        let Some(path) = path else {
            return self.finish_selection(ticket, Ok(None));
        };
        let mut pending = self.enqueue_root(Action::Selection { ticket, path });
        match pending.receiver.recv().await {
            Some(Outcome::Selection(result)) => result,
            _ => Err(issue("INTERNAL")),
        }
    }
    pub async fn refresh(
        self: Arc<Self>,
        context: NativeContext,
        epoch: u64,
        expected: Scope,
    ) -> Result<RootData, RefreshFailure> {
        let ticket = self
            .begin_refresh(&context, epoch, &expected)
            .map_err(RefreshFailure::uncommitted)?;
        let mut pending = self.enqueue_root(Action::Refresh(ticket));
        match pending.receiver.recv().await {
            Some(Outcome::Refresh(result)) => result,
            _ => Err(RefreshFailure::uncommitted(issue("INTERNAL"))),
        }
    }
}
