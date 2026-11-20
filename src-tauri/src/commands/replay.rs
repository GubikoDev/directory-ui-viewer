//! Once-only request execution with bounded pending waiters and charged replies.
use super::delivery::ReplyDelivery;
use super::protocol::{MAX_MESSAGE_BYTES, RequestNumber};
use crate::{
    domain::controller::{Admission, ControlError, RequestLedger},
    runtime::memory::{ByteBudget, Reservation},
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    io::{self, Write},
    sync::{Arc, Mutex, Weak},
};
use tokio::sync::watch;
const CELL_BYTES: usize = 2048;
const WAITER_BYTES: usize = 512;
pub const CONTROL_REPLY_BYTES: usize = 32 * 1024;
/// Hash canonical typed input directly, without allocating a second JSON buffer.
pub fn fingerprint(value: &impl Serialize) -> Result<String, ControlError> {
    struct HashWriter(Sha256);
    impl Write for HashWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.update(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut writer = HashWriter(Sha256::new());
    serde_json::to_writer(&mut writer, value).map_err(|_| ControlError::InvalidArgument)?;
    Ok(format!("{:x}", writer.0.finalize()))
}
pub struct Reply {
    bytes: Vec<u8>,
    _allocation: Option<Reservation>,
    _base: Arc<Reservation>,
}
impl Reply {
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}
struct Cell {
    request_id: String,
    result: Mutex<Option<Arc<Reply>>>,
    signal: watch::Sender<bool>,
    memory: Arc<Reservation>,
}
impl Cell {
    fn publish(&self, reply: Arc<Reply>) -> Arc<Reply> {
        let mut state = self.result.lock().unwrap();
        if let Some(existing) = state.as_ref() {
            return existing.clone();
        }
        *state = Some(reply.clone());
        self.signal.send_replace(true);
        reply
    }
    fn failure(&self, code: &str) -> Arc<Reply> {
        // Bounded fallback is covered by CELL_BYTES even under total exhaustion.
        let bytes = serde_json::to_vec(&serde_json::json!({"requestId":self.request_id,"outcome":"error","error":{"code":code,"operation":"request","scope":"root"}})).expect("static error JSON");
        assert!(bytes.capacity() <= 1024);
        self.publish(Arc::new(Reply {
            bytes,
            _allocation: None,
            _base: self.memory.clone(),
        }))
    }
}
struct State {
    ledger: RequestLedger<Arc<Reply>>,
    pending: BTreeMap<u64, Arc<Cell>>,
    closed: bool,
}
struct Inner {
    state: Mutex<State>,
    epoch: u64,
    budget: ByteBudget,
}
pub struct ReplayService(Arc<Inner>);
pub enum RequestAdmission {
    Execute(Completion, ReplyWaiter),
    Wait(ReplyWaiter),
}
pub struct Completion {
    inner: Weak<Inner>,
    number: u64,
    cell: Arc<Cell>,
    bytes: Vec<u8>,
    allocation: Option<Reservation>,
    max_bytes: Option<usize>,
    done: bool,
}
pub struct ReplyWaiter {
    inner: Weak<Inner>,
    result: WaitState,
    _memory: Option<Reservation>,
}
enum WaitState {
    Ready(Arc<Reply>),
    Pending {
        cell: Arc<Cell>,
        signal: watch::Receiver<bool>,
    },
}
impl ReplyWaiter {
    fn validate(inner: &Weak<Inner>) -> Result<(), ControlError> {
        let inner = inner.upgrade().ok_or(ControlError::ClientExpired)?;
        if inner.state.lock().unwrap().closed {
            Err(ControlError::ClientExpired)
        } else {
            Ok(())
        }
    }
    pub async fn wait(mut self) -> Result<Arc<Reply>, ControlError> {
        match &mut self.result {
            WaitState::Ready(reply) => {
                Self::validate(&self.inner)?;
                Ok(reply.clone())
            }
            WaitState::Pending { cell, signal } => loop {
                let ready = cell.result.lock().unwrap().clone();
                if let Some(reply) = ready {
                    Self::validate(&self.inner)?;
                    return Ok(reply);
                }
                signal
                    .changed()
                    .await
                    .map_err(|_| ControlError::ClientExpired)?;
            },
        }
    }
}
impl ReplayService {
    pub fn new(
        epoch: u64,
        budget: ByteBudget,
        records: usize,
        ttl_ms: u64,
    ) -> Result<Self, ControlError> {
        if epoch == 0 || epoch > 9_007_199_254_740_991 || records > 256 {
            return Err(ControlError::InvalidArgument);
        }
        let ledger = RequestLedger::with_budget(records, ttl_ms, budget.clone())?;
        Ok(Self(Arc::new(Inner {
            state: Mutex::new(State {
                ledger,
                pending: BTreeMap::new(),
                closed: false,
            }),
            epoch,
            budget,
        })))
    }
    /// Reserve mutation replies before executing effects. Read-only responses may
    /// start with a smaller allowance and grow while serializing, within 1 MiB.
    pub fn begin(
        &self,
        request: RequestNumber,
        hash: &str,
        now: u64,
        reply_allowance: usize,
    ) -> Result<RequestAdmission, ControlError> {
        self.begin_with_priority(request, hash, now, reply_allowance, false)
    }
    pub(crate) fn begin_with_priority(
        &self,
        request: RequestNumber,
        hash: &str,
        now: u64,
        reply_allowance: usize,
        priority: bool,
    ) -> Result<RequestAdmission, ControlError> {
        self.begin_for_delivery(request, hash, now, reply_allowance, priority, None)
            .map(|(admission, _)| admission)
    }
    pub(crate) fn begin_for_delivery(
        &self,
        request: RequestNumber,
        hash: &str,
        now: u64,
        reply_allowance: usize,
        priority: bool,
        delivery_allowance: Option<usize>,
    ) -> Result<(RequestAdmission, Option<ReplyDelivery>), ControlError> {
        let budget = if priority {
            self.0.budget.control()
        } else {
            self.0.budget.clone()
        };
        if request.epoch != self.0.epoch {
            return Err(ControlError::ClientExpired);
        }
        if reply_allowance > MAX_MESSAGE_BYTES
            || hash.len() != 64
            || !hash.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(ControlError::InvalidArgument);
        }
        let mut state = self.0.state.lock().unwrap();
        if state.closed {
            return Err(ControlError::ClientExpired);
        }
        // Hold an existing reply before pressure eviction. A replay waiter can
        // keep using it even if its ledger record is the one evicted below.
        let existing = state.ledger.lookup(request.sequence, hash, now)?;
        let delivery = if let Some(bytes) = delivery_allowance {
            if bytes > MAX_MESSAGE_BYTES {
                return Err(ControlError::InvalidArgument);
            }
            Some(loop {
                match ReplyDelivery::reserve(bytes, &budget) {
                    Ok(delivery) => break delivery,
                    Err(_) if state.ledger.evict_completed() => (),
                    Err(_) => return Err(ControlError::ResourceLimit),
                }
            })
        } else {
            None
        };
        match existing {
            Some(existing) => {
                let memory = budget
                    .reserve(WAITER_BYTES)
                    .map_err(|_| ControlError::ResourceLimit)?;
                let result = match existing {
                    Admission::Replay(reply) => WaitState::Ready(reply),
                    Admission::Pending => {
                        let cell = state
                            .pending
                            .get(&request.sequence)
                            .expect("pending cell accompanies ledger")
                            .clone();
                        let signal = cell.signal.subscribe();
                        WaitState::Pending { cell, signal }
                    }
                    Admission::New => unreachable!(),
                };
                return Ok((
                    RequestAdmission::Wait(ReplyWaiter {
                        inner: Arc::downgrade(&self.0),
                        result,
                        _memory: Some(memory),
                    }),
                    delivery,
                ));
            }
            None => (),
        }
        let (base, mut allocation) = loop {
            let reserved = budget.reserve(CELL_BYTES).and_then(|base| {
                budget
                    .reserve(reply_allowance)
                    .map(|allocation| (base, allocation))
            });
            match reserved {
                Ok(pair) => break pair,
                Err(_) if state.ledger.evict_completed() => (),
                Err(_) => return Err(ControlError::ResourceLimit),
            }
        };
        // Allocate the admitted reply buffer before ledger admission/effects.
        // Bounded mutation replies then serialize without budget growth or a
        // later allocation. Read-only replies may still grow via ChargedWriter.
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(reply_allowance)
            .map_err(|_| ControlError::ResourceLimit)?;
        allocation
            .resize(bytes.capacity())
            .map_err(|_| ControlError::ResourceLimit)?;
        assert!(matches!(
            state
                .ledger
                .admit_with_priority(request.sequence, hash, now, priority)?,
            Admission::New
        ));
        let (signal, _) = watch::channel(false);
        let cell = Arc::new(Cell {
            request_id: format!("native-epoch-{}:{}", request.epoch, request.sequence),
            result: Mutex::new(None),
            signal,
            memory: Arc::new(base),
        });
        state.pending.insert(request.sequence, cell.clone());
        let waiting = ReplyWaiter {
            inner: Arc::downgrade(&self.0),
            result: WaitState::Pending {
                cell: cell.clone(),
                signal: cell.signal.subscribe(),
            },
            _memory: None,
        };
        Ok((
            RequestAdmission::Execute(
                Completion {
                    inner: Arc::downgrade(&self.0),
                    number: request.sequence,
                    cell,
                    allocation: Some(allocation),
                    bytes,
                    max_bytes: delivery_allowance.map(|_| reply_allowance),
                    done: false,
                },
                waiting,
            ),
            delivery,
        ))
    }
    pub fn close(&self) {
        let pending = {
            let mut state = self.0.state.lock().unwrap();
            state.closed = true;
            state.ledger.clear();
            std::mem::take(&mut state.pending)
        };
        for cell in pending.into_values() {
            cell.failure("CLIENT_EXPIRED");
        }
    }
}
impl Drop for ReplayService {
    fn drop(&mut self) {
        self.close();
    }
}
struct ChargedWriter {
    bytes: Vec<u8>,
    memory: Reservation,
    max_bytes: usize,
}
impl Write for ChargedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let length = self
            .bytes
            .len()
            .checked_add(bytes.len())
            .filter(|n| *n <= self.max_bytes)
            .ok_or_else(|| io::Error::other("RESOURCE_LIMIT"))?;
        if length > self.bytes.capacity() {
            let preferred = length.next_power_of_two().max(512).min(MAX_MESSAGE_BYTES);
            let capacity = if self
                .memory
                .resize(self.memory.bytes().max(preferred))
                .is_ok()
            {
                preferred
            } else {
                length
            };
            self.memory
                .resize(self.memory.bytes().max(capacity))
                .map_err(|_| io::Error::other("RESOURCE_LIMIT"))?;
            self.bytes
                .try_reserve_exact(capacity - self.bytes.len())
                .map_err(|_| io::Error::other("RESOURCE_LIMIT"))?;
            // Do not assume an allocator reports exactly the requested capacity.
            self.memory
                .resize(self.memory.bytes().max(self.bytes.capacity()))
                .map_err(|_| io::Error::other("RESOURCE_LIMIT"))?;
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
impl Completion {
    pub fn complete(mut self, value: impl Serialize) -> Result<Arc<Reply>, ControlError> {
        let mut writer = ChargedWriter {
            bytes: std::mem::take(&mut self.bytes),
            memory: self.allocation.take().expect("one completion"),
            max_bytes: self.max_bytes.unwrap_or(MAX_MESSAGE_BYTES),
        };
        let serialized = serde_json::to_writer(&mut writer, &value);
        drop(value);
        let reply = if serialized.is_ok() {
            writer.bytes.shrink_to_fit();
            writer
                .memory
                .resize(writer.bytes.capacity())
                .expect("shrinking reply reservation");
            self.cell.publish(Arc::new(Reply {
                bytes: writer.bytes,
                _allocation: Some(writer.memory),
                _base: self.cell.memory.clone(),
            }))
        } else {
            drop(writer);
            // Mutation overflow is a producer-contract defect, not permission
            // to compete for post-effect memory. Read-only growth is bounded
            // by the wire limit and may legitimately exhaust the shared pool.
            self.cell.failure(if self.max_bytes.is_some() {
                "INTERNAL"
            } else {
                "RESOURCE_LIMIT"
            })
        };
        self.retire(reply.clone());
        self.done = true;
        ReplyWaiter::validate(&self.inner)?;
        Ok(reply)
    }
    fn retire(&self, reply: Arc<Reply>) {
        if let Some(inner) = self.inner.upgrade() {
            let mut state = inner.state.lock().unwrap();
            if !state.closed {
                state
                    .ledger
                    .complete(self.number, reply)
                    .expect("pending records are retained");
                state.pending.remove(&self.number);
            }
        }
    }
}
impl Drop for Completion {
    fn drop(&mut self) {
        if !self.done {
            let reply = self.cell.failure("INTERNAL");
            self.retire(reply);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn completed_small_replies_release_the_unused_start_listing_allowance() {
        let budget = ByteBudget::with_headroom(4 * 1024 * 1024, 1024 * 1024);
        let service = ReplayService::new(1, budget.clone(), 256, 300_000).unwrap();
        let allowance = 80 * 1024;
        for sequence in 1..=300 {
            let (RequestAdmission::Execute(owner, _waiter), Some(_delivery)) = service
                .begin_for_delivery(n(sequence), &hash(), 0, allowance, false, Some(allowance))
                .unwrap()
            else {
                panic!("expected a new listing request");
            };
            let _response_memory = budget.reserve(64 * 1024).unwrap();
            owner.complete(json!({"sequence": sequence})).unwrap();
        }
        let replay = tauri::async_runtime::block_on(wait(&service, 300, 0).wait()).unwrap();
        assert_eq!(replay.bytes(), br#"{"sequence":300}"#);
    }
    #[test]
    fn delivery_admission_reclaims_completed_cache_and_preserves_current_replay() {
        let budget = ByteBudget::new(192 * 1024);
        let service = ReplayService::new(1, budget.clone(), 256, 1000).unwrap();
        drop(
            execute(&service, 1, 48 * 1024)
                .complete(json!({"id":1}))
                .unwrap(),
        );
        drop(
            execute(&service, 2, 48 * 1024)
                .complete("x".repeat(48 * 1024 - 2))
                .unwrap(),
        );
        // Force delivery to reclaim the second reply while replaying the first.
        let pressure = budget.reserve(budget.available()).unwrap();
        let (RequestAdmission::Wait(waiter), Some(delivery)) = service
            .begin_for_delivery(n(1), &hash(), 0, 4096, false, Some(4096))
            .unwrap()
        else {
            panic!("must replay, never repeat effects");
        };
        let reply = tauri::async_runtime::block_on(waiter.wait()).unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(reply.bytes()).unwrap(),
            json!({"id":1})
        );
        // New requests also remain admissible after the completed cache was
        // reclaimed, rather than leaving the request ledger permanently full.
        drop((reply, delivery, pressure));
        let (RequestAdmission::Execute(owner, _), Some(delivery)) = service
            .begin_for_delivery(n(3), &hash(), 0, 4096, false, Some(4096))
            .unwrap()
        else {
            panic!("new request");
        };
        drop(owner.complete(json!({"id":3})).unwrap());
        drop((delivery, service));
        assert_eq!(budget.used(), 0);
    }
    #[test]
    fn failed_delivery_admission_does_not_execute_or_consume_request_identity() {
        let budget = ByteBudget::new(128 * 1024);
        let service = ReplayService::new(1, budget.clone(), 256, 1000).unwrap();
        let pressure = budget.reserve(budget.available()).unwrap();
        assert!(matches!(
            service.begin_for_delivery(n(1), &hash(), 0, 4096, false, Some(4096)),
            Err(ControlError::ResourceLimit)
        ));
        drop(pressure);
        assert!(matches!(
            service.begin_for_delivery(n(1), &hash(), 0, 4096, false, Some(4096)),
            Ok((RequestAdmission::Execute(..), Some(_)))
        ));
    }
    #[test]
    fn mutation_serialization_never_grows_past_its_admitted_contract() {
        let budget = ByteBudget::new(128 * 1024);
        let service = ReplayService::new(1, budget.clone(), 256, 1000).unwrap();
        let (RequestAdmission::Execute(owner, _), Some(delivery)) = service
            .begin_for_delivery(n(1), &hash(), 0, 4096, false, Some(4096))
            .unwrap()
        else {
            panic!("new mutation");
        };
        let peak = budget.peak();
        let reply = owner.complete(&"x".repeat(4096)).unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(reply.bytes()).unwrap()["error"]["code"],
            "INTERNAL"
        );
        assert_eq!(budget.peak(), peak, "no post-effect reservation growth");
        assert!(
            delivery
                .prepare(&"a".repeat(64), &"b".repeat(32), reply.bytes())
                .is_ok()
        );
        drop((reply, service));
        assert_eq!(budget.used(), 0);
    }
    #[test]
    fn general_pending_admission_exhausts_bytes_before_slots_and_leaves_control_room() {
        let budget = ByteBudget::with_headroom(4 * 1024 * 1024, 1024 * 1024);
        let service = ReplayService::new(1, budget.clone(), 256, 1000).unwrap();
        let mut owners = Vec::new();
        let mut sequence = 1;
        loop {
            match service.begin(n(sequence), &hash(), 0, CONTROL_REPLY_BYTES) {
                Ok(RequestAdmission::Execute(owner, _)) => owners.push(owner),
                Err(ControlError::ResourceLimit) => break,
                _ => panic!("unexpected admission"),
            }
            sequence += 1;
        }
        assert!(owners.len() > 1 && owners.len() < 254);
        // No completed records exist to evict and every ordinary reply keeps
        // its 32KiB minimum until completion. A close still obtains a slot.
        let RequestAdmission::Execute(close, _) = service
            .begin_with_priority(n(sequence), &hash(), 0, CONTROL_REPLY_BYTES, true)
            .unwrap()
        else {
            panic!()
        };
        close.complete(json!({"status":"closed"})).unwrap();
        assert!(budget.peak() <= budget.limit());
        drop(owners);
        drop(service);
        assert_eq!(budget.used(), 0);
    }
    #[test]
    fn priority_admission_and_duplicates_work_under_general_pressure_and_remain_charged() {
        let budget = ByteBudget::with_headroom(256 * 1024, 128 * 1024);
        let service = ReplayService::new(1, budget.clone(), 4, 1000).unwrap();
        let pressure = budget.reserve(budget.available()).unwrap();
        assert_eq!(
            error(service.begin(n(1), &hash(), 0, CONTROL_REPLY_BYTES)),
            ControlError::ResourceLimit
        );
        let RequestAdmission::Execute(owner, waiter) = service
            .begin_with_priority(n(1), &hash(), 0, CONTROL_REPLY_BYTES, true)
            .unwrap()
        else {
            panic!()
        };
        let RequestAdmission::Wait(duplicate) = service
            .begin_with_priority(n(1), &hash(), 0, CONTROL_REPLY_BYTES, true)
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(
            error(service.begin_with_priority(n(1), &"b".repeat(64), 0, CONTROL_REPLY_BYTES, true)),
            ControlError::InvalidArgument
        );
        let reply = owner.complete(json!({"status":"closed"})).unwrap();
        let one = tauri::async_runtime::block_on(waiter.wait()).unwrap();
        let two = tauri::async_runtime::block_on(duplicate.wait()).unwrap();
        assert!(Arc::ptr_eq(&reply, &one));
        assert!(Arc::ptr_eq(&reply, &two));
        assert_eq!(budget.available(), 0);
        assert!(budget.used() <= budget.limit());
        drop(service);
        drop(pressure);
        assert!(budget.used() > 0);
        drop((reply, one, two));
        assert_eq!(budget.used(), 0);
    }
    #[test]
    fn writer_uses_exact_growth_when_rounded_capacity_does_not_fit() {
        let budget = ByteBudget::new(700_000);
        let mut writer = ChargedWriter {
            bytes: Vec::new(),
            memory: budget.reserve(0).unwrap(),
            max_bytes: MAX_MESSAGE_BYTES,
        };
        writer.write_all(&vec![b'x'; 600_001]).unwrap();
        assert_eq!(writer.bytes.len(), 600_001);
        assert_eq!(budget.used(), writer.bytes.capacity());
        drop(writer);
        assert_eq!(budget.used(), 0);
    }
    fn n(sequence: u64) -> RequestNumber {
        RequestNumber { epoch: 1, sequence }
    }
    fn hash() -> String {
        fingerprint(&json!({"operation":"refreshRoot","expectedGeneration":1})).unwrap()
    }
    fn execute(service: &ReplayService, sequence: u64, allowance: usize) -> Completion {
        match service.begin(n(sequence), &hash(), 0, allowance).unwrap() {
            RequestAdmission::Execute(c, _) => c,
            _ => panic!("expected new execution"),
        }
    }
    fn wait(service: &ReplayService, sequence: u64, now: u64) -> ReplyWaiter {
        match service.begin(n(sequence), &hash(), now, 0).unwrap() {
            RequestAdmission::Wait(w) => w,
            _ => panic!("unexpected repeat execution"),
        }
    }
    fn error<T>(result: Result<T, ControlError>) -> ControlError {
        match result {
            Err(e) => e,
            Ok(_) => panic!("expected rejection"),
        }
    }
    #[test]
    fn pending_and_completed_retries_share_one_reply_and_changed_payload_is_rejected() {
        let budget = ByteBudget::new(100_000);
        let service = ReplayService::new(1, budget.clone(), 4, 100).unwrap();
        let owner = execute(&service, 1, CONTROL_REPLY_BYTES);
        let waiting = wait(&service, 1, 99);
        assert_eq!(
            error(service.begin(n(1), &fingerprint(&json!("different")).unwrap(), 99, 0)),
            ControlError::InvalidArgument
        );
        let reply = owner.complete(json!({"generation":2})).unwrap();
        let duplicate = tauri::async_runtime::block_on(waiting.wait()).unwrap();
        assert!(Arc::ptr_eq(&reply, &duplicate));
        let repeated = tauri::async_runtime::block_on(wait(&service, 1, 99).wait()).unwrap();
        assert!(Arc::ptr_eq(&reply, &repeated));
        assert_eq!(
            error(service.begin(n(1), &hash(), 100, 0)),
            ControlError::RequestExpired
        );
        assert_eq!(
            error(service.begin(
                RequestNumber {
                    epoch: 2,
                    sequence: 2
                },
                &hash(),
                100,
                0
            )),
            ControlError::ClientExpired
        );
        drop(service);
        assert!(
            budget.used() > 0,
            "returned reply stays charged after cache eviction"
        );
        drop(reply);
        drop(duplicate);
        drop(repeated);
        assert_eq!(budget.used(), 0);
    }
    #[test]
    fn pending_is_not_evicted_by_ttl_or_count_and_completed_eviction_keeps_high_water() {
        let budget = ByteBudget::new(100_000);
        let service = ReplayService::new(1, budget.clone(), 1, 1).unwrap();
        let owner = execute(&service, 1, 0);
        assert_eq!(
            error(service.begin(n(2), &hash(), 1000, 0)),
            ControlError::ResourceLimit
        );
        let waiting = wait(&service, 1, 1000);
        owner.complete(json!("original")).unwrap();
        assert_eq!(
            tauri::async_runtime::block_on(waiting.wait())
                .unwrap()
                .bytes(),
            b"\"original\""
        );
        execute(&service, 2, 0).complete(json!("next")).unwrap();
        assert_eq!(
            error(service.begin(n(1), &hash(), 0, 0)),
            ControlError::RequestExpired
        );
        drop(service);
        assert_eq!(budget.used(), 0);
    }
    #[test]
    fn abandoned_owner_and_oversized_output_are_final_errors_not_reexecutions() {
        let service = ReplayService::new(1, ByteBudget::new(3_000_000), 4, 1000).unwrap();
        let owner = execute(&service, 1, 0);
        let waiting = wait(&service, 1, 0);
        drop(owner);
        let reply = tauri::async_runtime::block_on(waiting.wait()).unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(reply.bytes()).unwrap()["error"]["code"],
            "INTERNAL"
        );
        let oversized = execute(&service, 2, 0)
            .complete("x".repeat(MAX_MESSAGE_BYTES))
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(oversized.bytes()).unwrap()["error"]["code"],
            "RESOURCE_LIMIT"
        );
        assert!(Arc::ptr_eq(
            &oversized,
            &tauri::async_runtime::block_on(wait(&service, 2, 0).wait()).unwrap()
        ));
    }
    #[test]
    fn reply_holders_keep_reservations_under_pressure_and_release_allows_new_admission() {
        let budget = ByteBudget::new(64 * 1024);
        let service = ReplayService::new(1, budget.clone(), 4, 1000).unwrap();
        let mut held = vec![];
        for sequence in 1..=3 {
            held.push(
                execute(&service, sequence, 16 * 1024)
                    .complete("x".repeat(16 * 1024 - 2))
                    .unwrap(),
            );
        }
        assert_eq!(
            error(service.begin(n(4), &hash(), 0, 16 * 1024)),
            ControlError::ResourceLimit
        );
        assert!(
            budget.used() > 50 * 1024,
            "eviction does not uncharge externally held replies"
        );
        held.remove(0);
        let next = execute(&service, 4, 16 * 1024)
            .complete(json!("after release"))
            .unwrap();
        assert!(budget.used() <= budget.limit());
        drop(held);
        drop(next);
        drop(service);
        assert_eq!(budget.used(), 0);
    }
    #[test]
    fn closing_epoch_wakes_pending_waiters_and_rejects_late_or_ready_results() {
        let budget = ByteBudget::new(100_000);
        let service = ReplayService::new(1, budget.clone(), 4, 1000).unwrap();
        let first = execute(&service, 1, 0);
        let pending = wait(&service, 1, 0);
        execute(&service, 2, 0).complete(json!("done")).unwrap();
        let ready = wait(&service, 2, 0);
        service.close();
        assert_eq!(
            error(tauri::async_runtime::block_on(pending.wait())),
            ControlError::ClientExpired
        );
        assert_eq!(
            error(tauri::async_runtime::block_on(ready.wait())),
            ControlError::ClientExpired
        );
        assert_eq!(
            error(first.complete(json!("too late"))),
            ControlError::ClientExpired
        );
        assert_eq!(
            error(service.begin(n(3), &hash(), 0, 0)),
            ControlError::ClientExpired
        );
        drop(service);
        assert_eq!(budget.used(), 0);
    }
    #[test]
    fn simultaneous_duplicates_execute_once_and_all_waiters_receive_the_same_value() {
        use std::sync::{
            Barrier,
            atomic::{AtomicUsize, Ordering},
        };
        let service = Arc::new(ReplayService::new(1, ByteBudget::new(100_000), 4, 1000).unwrap());
        let admitted = Arc::new(Barrier::new(8));
        let executions = Arc::new(AtomicUsize::new(0));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let service = service.clone();
                let admitted = admitted.clone();
                let executions = executions.clone();
                std::thread::spawn(move || {
                    let admission = service.begin(n(1), &hash(), 0, 0).unwrap();
                    admitted.wait();
                    match admission {
                        RequestAdmission::Execute(owner, _) => {
                            executions.fetch_add(1, Ordering::SeqCst);
                            owner.complete(json!("one result")).unwrap()
                        }
                        RequestAdmission::Wait(waiting) => {
                            tauri::async_runtime::block_on(waiting.wait()).unwrap()
                        }
                    }
                })
            })
            .collect();
        let replies: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        assert_eq!(executions.load(Ordering::SeqCst), 1);
        assert!(replies.iter().all(|reply| Arc::ptr_eq(reply, &replies[0])));
    }
    #[test]
    fn canonical_typed_payload_hash_ignores_json_whitespace_but_not_changed_arguments() {
        use super::super::protocol::Request;
        let a = Request::parse(br#"{"protocolVersion":1,"requestId":"native-epoch-1:1","body":{"operation":"chooseRoot"}}"#).unwrap();
        let b = Request::parse(br#"{ "body": { "operation": "chooseRoot" }, "requestId":"native-epoch-1:1", "protocolVersion":1 }"#).unwrap();
        assert_eq!(fingerprint(&a).unwrap(), fingerprint(&b).unwrap());
        assert_eq!(fingerprint(&a).unwrap().len(), 64);
    }
}
