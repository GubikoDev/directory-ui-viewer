//! Pure control plane. Native window/document contexts must be provided by the
//! trusted command boundary, never deserialized from frontend arguments.
use super::model::Phase;
use std::collections::VecDeque;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

#[derive(Debug, Clone, Default)]
pub struct Cancellation(Arc<AtomicBool>);
impl Cancellation {
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
}

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlError {
    ClientExpired,
    InvalidArgument,
    RequestExpired,
    StaleGeneration,
    SessionClosed,
    ResourceLimit,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeContext {
    pub window: String,
    pub document: u64,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    pub session_id: String,
    pub generation: u64,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admission<T> {
    New,
    Pending,
    Replay(T),
}
#[derive(Debug)]
struct RequestRecord<T> {
    number: u64,
    fingerprint: String,
    result: Option<T>,
    accepted_ms: u64,
}
/// Bounded completed results + a high-water mark prevent expired requests from
/// executing again. Pending requests are never evicted to make room for new work.
pub struct RequestLedger<T> {
    records: VecDeque<RequestRecord<T>>,
    high_water: u64,
    max_records: usize,
    ttl_ms: u64,
}
impl<T: Clone> RequestLedger<T> {
    pub fn new(max_records: usize, ttl_ms: u64) -> Self {
        Self {
            records: VecDeque::new(),
            high_water: 0,
            max_records,
            ttl_ms,
        }
    }
    pub fn admit(
        &mut self,
        number: u64,
        fingerprint: &str,
        now: u64,
    ) -> Result<Admission<T>, ControlError> {
        if number == 0 || number > MAX_SAFE_INTEGER || fingerprint.len() > 1024 {
            return Err(ControlError::InvalidArgument);
        }
        self.records
            .retain(|r| r.result.is_none() || now.saturating_sub(r.accepted_ms) < self.ttl_ms);
        if let Some(record) = self.records.iter().find(|r| r.number == number) {
            if record.fingerprint != fingerprint {
                return Err(ControlError::InvalidArgument);
            }
            return Ok(match &record.result {
                Some(result) => Admission::Replay(result.clone()),
                None => Admission::Pending,
            });
        }
        if number <= self.high_water {
            return Err(ControlError::RequestExpired);
        }
        while self.records.len() >= self.max_records {
            let Some(index) = self.records.iter().position(|r| r.result.is_some()) else {
                return Err(ControlError::ResourceLimit);
            };
            self.records.remove(index);
        }
        self.high_water = number;
        self.records.push_back(RequestRecord {
            number,
            fingerprint: fingerprint.into(),
            result: None,
            accepted_ms: now,
        });
        Ok(Admission::New)
    }
    pub fn complete(&mut self, number: u64, result: T) -> Result<(), ControlError> {
        let record = self
            .records
            .iter_mut()
            .find(|r| r.number == number)
            .ok_or(ControlError::RequestExpired)?;
        if record.result.is_none() {
            record.result = Some(result);
        }
        Ok(())
    }
}
pub struct SessionController {
    context: NativeContext,
    nonce: Option<String>,
    epoch_serial: u64,
    pub scope: Option<Scope>,
    selection_serial: u64,
    active_selection: Option<u64>,
    cancellation: Cancellation,
}
#[derive(Debug, Clone)]
pub struct SelectionTicket {
    context: NativeContext,
    epoch_serial: u64,
    serial: u64,
}
impl SessionController {
    pub fn new(context: NativeContext) -> Self {
        Self {
            context,
            nonce: None,
            epoch_serial: 0,
            scope: None,
            selection_serial: 0,
            active_selection: None,
            cancellation: Cancellation::default(),
        }
    }
    pub fn connect(&mut self, context: &NativeContext, nonce: &str) -> Result<u64, ControlError> {
        if context != &self.context || context.window != "main" {
            return Err(ControlError::ClientExpired);
        }
        if nonce.is_empty() || nonce.len() > 128 {
            return Err(ControlError::InvalidArgument);
        }
        if let Some(current) = &self.nonce {
            return if current == nonce {
                Ok(self.epoch_serial)
            } else {
                Err(ControlError::ClientExpired)
            };
        }
        self.epoch_serial = self
            .epoch_serial
            .checked_add(1)
            .filter(|n| *n <= MAX_SAFE_INTEGER)
            .ok_or(ControlError::ResourceLimit)?;
        self.nonce = Some(nonce.into());
        Ok(self.epoch_serial)
    }
    pub fn document_loaded(&mut self, context: NativeContext) {
        if context == self.context {
            return;
        }
        self.close();
        self.nonce = None;
        self.context = context;
    }
    pub fn validate(
        &self,
        context: &NativeContext,
        epoch: u64,
        scope: Option<&Scope>,
    ) -> Result<(), ControlError> {
        if context != &self.context || self.nonce.is_none() || epoch != self.epoch_serial {
            return Err(ControlError::ClientExpired);
        }
        if let Some(expected) = scope {
            let actual = self.scope.as_ref().ok_or(ControlError::SessionClosed)?;
            if actual.session_id != expected.session_id {
                return Err(ControlError::SessionClosed);
            }
            if actual.generation != expected.generation {
                return Err(ControlError::StaleGeneration);
            }
        }
        Ok(())
    }
    pub fn begin_selection(
        &mut self,
        context: &NativeContext,
        epoch: u64,
    ) -> Result<SelectionTicket, ControlError> {
        self.validate(context, epoch, None)?;
        if self.active_selection.is_some() {
            return Err(ControlError::ResourceLimit);
        }
        self.selection_serial = self
            .selection_serial
            .checked_add(1)
            .ok_or(ControlError::ResourceLimit)?;
        self.active_selection = Some(self.selection_serial);
        Ok(SelectionTicket {
            context: context.clone(),
            epoch_serial: epoch,
            serial: self.selection_serial,
        })
    }
    pub fn accept_selection(
        &mut self,
        ticket: &SelectionTicket,
        session_id: String,
    ) -> Result<Scope, ControlError> {
        self.dismiss_selection(ticket)?;
        self.validate(&ticket.context, ticket.epoch_serial, None)?;
        if ticket.serial != self.selection_serial {
            return Err(ControlError::ClientExpired);
        }
        self.selection_serial += 1;
        let scope = Scope {
            session_id,
            generation: 1,
        };
        self.cancellation.cancel();
        self.cancellation = Cancellation::default();
        self.scope = Some(scope.clone());
        Ok(scope)
    }
    /// Native callback cleanup, including cancellation/validation failure. Releasing
    /// a stale picker must not release a newer one or change the current session.
    pub fn dismiss_selection(&mut self, ticket: &SelectionTicket) -> Result<(), ControlError> {
        if self.active_selection != Some(ticket.serial) {
            return Err(ControlError::ClientExpired);
        }
        self.active_selection = None;
        Ok(())
    }
    /// Caller revalidates the root identity after advancing the generation.
    /// Failure leaves the new generation in an error state, never restores old data.
    pub fn refresh(&mut self, expected: &Scope) -> Result<Scope, ControlError> {
        let actual = self.scope.as_mut().ok_or(ControlError::SessionClosed)?;
        if actual.session_id != expected.session_id {
            return Err(ControlError::SessionClosed);
        }
        if actual.generation != expected.generation {
            return Err(ControlError::StaleGeneration);
        }
        actual.generation = actual
            .generation
            .checked_add(1)
            .filter(|n| *n <= MAX_SAFE_INTEGER)
            .ok_or(ControlError::ResourceLimit)?;
        self.cancellation.cancel();
        self.cancellation = Cancellation::default();
        self.selection_serial += 1;
        Ok(actual.clone())
    }
    pub fn cancellation_token(&self) -> Cancellation {
        self.cancellation.clone()
    }
    pub fn close(&mut self) {
        self.cancellation.cancel();
        self.scope = None;
        self.selection_serial += 1;
    }
}
/// State publication barrier shared by workers; cancellation requests don't imply
/// that a blocking kernel call has returned.
#[derive(Debug)]
pub struct TaskState {
    pub scope: Scope,
    pub phase: Phase,
    pub sequence: u64,
    pub cancel_requested: bool,
    pub cancellation: Cancellation,
}
impl TaskState {
    pub fn cancel(&mut self) -> bool {
        if self.phase.terminal() {
            return false;
        }
        self.cancel_requested = true;
        true
    }
    pub fn publish(&mut self, current: &Scope, phase: Phase) -> bool {
        if current != &self.scope
            || self.phase.terminal()
            || ((self.cancel_requested || self.cancellation.is_cancelled())
                && phase != Phase::Cancelled)
        {
            return false;
        }
        let Some(sequence) = self
            .sequence
            .checked_add(1)
            .filter(|n| *n <= MAX_SAFE_INTEGER)
        else {
            return false;
        };
        self.sequence = sequence;
        self.phase = phase;
        true
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn context(document: u64) -> NativeContext {
        NativeContext {
            window: "main".into(),
            document,
        }
    }
    #[test]
    fn requests_replay_expire_and_reject_payload_reuse() {
        let mut ledger = RequestLedger::new(2, 100);
        assert_eq!(ledger.admit(1, "refresh:1", 0), Ok(Admission::New));
        assert_eq!(ledger.admit(1, "refresh:1", 1), Ok(Admission::Pending));
        ledger.complete(1, 2).unwrap();
        assert_eq!(ledger.admit(1, "refresh:1", 2), Ok(Admission::Replay(2)));
        assert_eq!(
            ledger.admit(1, "refresh:2", 2),
            Err(ControlError::InvalidArgument)
        );
        assert_eq!(
            ledger.admit(1, "refresh:1", 101),
            Err(ControlError::RequestExpired)
        );
    }
    #[test]
    fn pending_requests_are_bounded_and_never_reexecuted() {
        let mut ledger = RequestLedger::<()>::new(1, 100);
        ledger.admit(1, "picker", 0).unwrap();
        assert_eq!(
            ledger.admit(2, "picker", 999),
            Err(ControlError::ResourceLimit)
        );
        assert_eq!(ledger.admit(1, "picker", 999), Ok(Admission::Pending));
    }
    #[test]
    fn reconnect_is_idempotent_only_for_the_same_native_document() {
        let mut controller = SessionController::new(context(1));
        let epoch = controller.connect(&context(1), "nonce1").unwrap();
        let ticket = controller.begin_selection(&context(1), epoch).unwrap();
        controller.accept_selection(&ticket, "s".into()).unwrap();
        assert_eq!(controller.connect(&context(1), "nonce1"), Ok(epoch));
        assert!(controller.scope.is_some());
        assert_eq!(
            controller.connect(&context(1), "nonce2"),
            Err(ControlError::ClientExpired)
        );
        controller.document_loaded(context(2));
        assert!(controller.scope.is_none());
        assert_eq!(
            controller.connect(&context(1), "nonce1"),
            Err(ControlError::ClientExpired)
        );
        assert_ne!(controller.connect(&context(2), "nonce2").unwrap(), epoch);
        assert_eq!(
            controller.accept_selection(&ticket, "old".into()),
            Err(ControlError::ClientExpired)
        );
    }
    #[test]
    fn one_picker_at_a_time_even_while_old_callback_is_draining() {
        let mut controller = SessionController::new(context(1));
        let epoch = controller.connect(&context(1), "nonce").unwrap();
        let ticket = controller.begin_selection(&context(1), epoch).unwrap();
        assert!(matches!(
            controller.begin_selection(&context(1), epoch),
            Err(ControlError::ResourceLimit)
        ));
        controller.close();
        assert!(matches!(
            controller.begin_selection(&context(1), epoch),
            Err(ControlError::ResourceLimit)
        ));
        controller.dismiss_selection(&ticket).unwrap();
        let newer = controller.begin_selection(&context(1), epoch).unwrap();
        assert_eq!(
            controller.dismiss_selection(&ticket),
            Err(ControlError::ClientExpired)
        );
        controller.dismiss_selection(&newer).unwrap();
    }
    #[test]
    fn close_invalidates_picker_and_refresh_rejects_old_generation() {
        let mut controller = SessionController::new(context(1));
        let epoch = controller.connect(&context(1), "nonce").unwrap();
        let ticket = controller.begin_selection(&context(1), epoch).unwrap();
        controller.close();
        assert_eq!(
            controller.accept_selection(&ticket, "old".into()),
            Err(ControlError::ClientExpired)
        );
        let ticket = controller.begin_selection(&context(1), epoch).unwrap();
        let scope = controller.accept_selection(&ticket, "s".into()).unwrap();
        assert_eq!(controller.refresh(&scope).unwrap().generation, 2);
        assert_eq!(
            controller.refresh(&scope),
            Err(ControlError::StaleGeneration)
        );
    }
    #[test]
    fn scope_replacement_signals_old_workers_without_cancelling_new_scope() {
        let mut controller = SessionController::new(context(1));
        let epoch = controller.connect(&context(1), "nonce").unwrap();
        let first = controller.begin_selection(&context(1), epoch).unwrap();
        controller.accept_selection(&first, "s1".into()).unwrap();
        let old = controller.cancellation_token();
        let second = controller.begin_selection(&context(1), epoch).unwrap();
        assert!(!old.is_cancelled()); // picker cancellation would preserve old root
        controller.accept_selection(&second, "s2".into()).unwrap();
        assert!(old.is_cancelled());
        let current = controller.cancellation_token();
        assert!(!current.is_cancelled());
        controller.document_loaded(context(1));
        assert!(!current.is_cancelled()); // duplicate native load notification
        controller.close();
        assert!(current.is_cancelled());
    }
    #[test]
    fn worker_publication_respects_generation_cancellation_and_terminal_state() {
        let scope = Scope {
            session_id: "s".into(),
            generation: 1,
        };
        let mut task = TaskState {
            scope: scope.clone(),
            phase: Phase::Running,
            sequence: 0,
            cancel_requested: false,
            cancellation: Cancellation::default(),
        };
        assert!(!task.publish(
            &Scope {
                generation: 2,
                ..scope.clone()
            },
            Phase::Completed
        ));
        assert!(task.cancel());
        assert!(!task.cancellation.is_cancelled()); // individual cancel is not a session-wide signal
        assert!(!task.publish(&scope, Phase::Completed));
        assert!(task.publish(&scope, Phase::Cancelled));
        assert!(!task.publish(&scope, Phase::Running));
        assert!(!task.cancel());
    }
}
