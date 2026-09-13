//! Variable-size replies retain their charge in the native evaluation callback.
use super::protocol::MAX_MESSAGE_BYTES;
use crate::runtime::memory::{ByteBudget, Reservation};
use std::{
    io::Write,
    sync::{Arc, Mutex},
};
use tokio::sync::oneshot;

#[derive(Debug, PartialEq, Eq)]
pub enum DeliveryError {
    InvalidArgument,
    ResourceLimit,
    Disconnected,
}
pub type Callback = Box<dyn Fn(String) + Send + 'static>;
pub struct Delivery {
    script: String,
    memory: Arc<Reservation>,
}
pub struct Receipt {
    receiver: oneshot::Receiver<bool>,
    _memory: Arc<Reservation>,
}
pub fn valid_key(key: &str) -> bool {
    key.len() == 32 && key.bytes().all(|b| b.is_ascii_hexdigit())
}
impl Delivery {
    /// Event delivery has one native receiver loop and no framework event queue.
    /// A missing subscriber, stale document or resource pressure drops the hint.
    pub fn work_event(
        proof: &str,
        event: &crate::domain::model::WorkEvent,
        budget: &ByteBudget,
    ) -> Result<Self, DeliveryError> {
        if proof.len() != 64 || !proof.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(DeliveryError::InvalidArgument);
        }
        if crate::runtime::serialization::json_bytes(event)
            .map_err(|_| DeliveryError::InvalidArgument)?
            > 4096
        {
            return Err(DeliveryError::ResourceLimit);
        }
        // Raw JSON <=4KiB, quoted JSON <=8KiB, script/native UTF-16 copies,
        // boolean result and callback metadata. Reserve before serialization.
        let memory = budget
            .reserve(64 * 1024)
            .map_err(|_| DeliveryError::ResourceLimit)?;
        let json = serde_json::to_string(event).map_err(|_| DeliveryError::InvalidArgument)?;
        let quoted = serde_json::to_string(&json).map_err(|_| DeliveryError::InvalidArgument)?;
        let script = format!(
            "(()=>{{if(window.__DIRECTORY_DOCUMENT_PROOF__!==\"{proof}\")return false;const receive=window.__DIRECTORY_WORK_EVENT__;return typeof receive===\"function\"&&receive({quoted})===true;}})()"
        );
        Ok(Self {
            script,
            memory: Arc::new(memory),
        })
    }
    /// Main-frame authentication returns only a boolean. The page's property
    /// value is never serialized into an unbounded native callback response.
    pub fn proof_check(proof: &str, budget: &ByteBudget) -> Result<Self, DeliveryError> {
        if proof.len() != 64 || !proof.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(DeliveryError::InvalidArgument);
        }
        // Fixed script, boolean result, oneshot/callback and native copies.
        // Keep this charge in both callback and waiter, as with payload delivery.
        let memory = budget
            .reserve(4096)
            .map_err(|_| DeliveryError::ResourceLimit)?;
        Ok(Self {
            script: format!("window.__DIRECTORY_DOCUMENT_PROOF__===\"{proof}\""),
            memory: Arc::new(memory),
        })
    }
    pub fn prepare(
        proof: &str,
        key: &str,
        json: &[u8],
        budget: &ByteBudget,
    ) -> Result<Self, DeliveryError> {
        if proof.len() != 64 || !proof.bytes().all(|b| b.is_ascii_hexdigit()) || !valid_key(key) {
            return Err(DeliveryError::InvalidArgument);
        }
        if json.len() > MAX_MESSAGE_BYTES {
            return Err(DeliveryError::ResourceLimit);
        }
        let json = std::str::from_utf8(json).map_err(|_| DeliveryError::InvalidArgument)?;
        // JSON string escaping can double already-serialized JSON. Count first,
        // without creating an uncharged second payload. Includes non-ASCII UTF-8.
        let capacity = crate::runtime::serialization::json_bytes(json)
            .map_err(|_| DeliveryError::InvalidArgument)?
            + 512;
        // Rust script plus UTF-16 NSString copy and conservative native handoff
        // headroom. Framework/WebView process RSS is measured separately.
        let memory = budget
            .reserve(capacity * 4 + 2048)
            .map_err(|_| DeliveryError::ResourceLimit)?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(capacity)
            .map_err(|_| DeliveryError::ResourceLimit)?;
        if bytes.capacity() > capacity {
            return Err(DeliveryError::ResourceLimit);
        }
        write!(&mut bytes, "(()=>{{if(window.__DIRECTORY_DOCUMENT_PROOF__!==\"{proof}\")return false;const receive=window.__DIRECTORY_RECEIVE__;if(typeof receive!==\"function\")return false;return receive(\"{key}\",").unwrap();
        serde_json::to_writer(&mut bytes, json).map_err(|_| DeliveryError::ResourceLimit)?;
        bytes.extend_from_slice(b")===true;})()");
        assert!(bytes.len() <= capacity);
        Ok(Self {
            script: String::from_utf8(bytes).expect("UTF-8 script"),
            memory: Arc::new(memory),
        })
    }
    /// The scheduler must keep the callback alongside the script until execution
    /// completes or the queued/native operation is destroyed. No timeout releases
    /// the callback's reservation. Wry's macOS callback path satisfies this after
    /// initialization (the command has already completed main-frame proof eval).
    pub fn submit(
        self,
        schedule: impl FnOnce(String, Callback) -> Result<(), DeliveryError>,
    ) -> Result<Receipt, DeliveryError> {
        let (sender, receiver) = oneshot::channel();
        let sender = Mutex::new(Some(sender));
        let memory = self.memory.clone();
        let callback = Box::new(move |value: String| {
            // Keep the reservation in the callback object, even after invocation,
            // until the framework releases that object.
            let _retained = &memory;
            if let Some(sender) = sender.lock().unwrap().take() {
                let _ = sender.send(value == "true");
            }
        });
        schedule(self.script, callback)?;
        Ok(Receipt {
            receiver,
            _memory: self.memory,
        })
    }
}
impl Receipt {
    /// Requires the evaluated expression to return the boolean true.
    /// For payload delivery this confirms the client slot settled; it can contain a client-local
    /// resource error; this acknowledgement never denotes operation success.
    pub async fn wait(self) -> Result<(), DeliveryError> {
        if self
            .receiver
            .await
            .map_err(|_| DeliveryError::Disconnected)?
        {
            Ok(())
        } else {
            Err(DeliveryError::Disconnected)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn event_scripts_quote_untrusted_fields_and_keep_abandoned_costs_bounded() {
        let budget = ByteBudget::new(64 * 1024);
        let wire: serde_json::Value = serde_json::from_str(include_str!(
            "../../../src/features/explorer/model/wire-fixture.json"
        ))
        .unwrap();
        let mut event: crate::domain::model::WorkEvent =
            serde_json::from_value(wire["event"].clone()).unwrap();
        event.task_id = "\");throw Error('injected')//한글".into();
        let mut queued = None;
        let receipt = Delivery::work_event(&"a".repeat(64), &event, &budget)
            .unwrap()
            .submit(|script, callback| {
                queued = Some((script, callback));
                Ok(())
            })
            .unwrap();
        drop(receipt);
        assert_eq!(budget.used(), 64 * 1024);
        assert!(matches!(
            Delivery::work_event(&"a".repeat(64), &event, &budget),
            Err(DeliveryError::ResourceLimit)
        ));
        let (script, callback) = queued.take().unwrap();
        let quoted = script
            .split("&&receive(")
            .nth(1)
            .unwrap()
            .strip_suffix(")===true;})()")
            .unwrap();
        let json: String = serde_json::from_str(quoted).unwrap();
        let decoded: crate::domain::model::WorkEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.task_id, event.task_id);
        assert!(script.contains("window.__DIRECTORY_DOCUMENT_PROOF__!=="));
        drop(script);
        callback("false".into());
        assert_eq!(budget.used(), 64 * 1024);
        drop(callback);
        assert_eq!(budget.used(), 0);
        event.task_id = "x".repeat(4097);
        assert!(matches!(
            Delivery::work_event(&"a".repeat(64), &event, &budget),
            Err(DeliveryError::ResourceLimit)
        ));
        assert_eq!(budget.used(), 0);
    }
    #[test]
    fn proof_timeout_keeps_native_callback_charged_and_returns_only_boolean() {
        let budget = ByteBudget::new(4096);
        let proof = "a".repeat(64);
        let mut queued = None;
        let receipt = Delivery::proof_check(&proof, &budget)
            .unwrap()
            .submit(|script, callback| {
                assert_eq!(
                    script,
                    format!("window.__DIRECTORY_DOCUMENT_PROOF__===\"{proof}\"")
                );
                queued = Some((script, callback));
                Ok(())
            })
            .unwrap();
        tauri::async_runtime::block_on(async {
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(1), receipt.wait())
                    .await
                    .is_err()
            );
        });
        assert_eq!(budget.used(), 4096);
        assert!(matches!(
            Delivery::proof_check(&proof, &budget),
            Err(DeliveryError::ResourceLimit)
        ));
        let (script, callback) = queued.take().unwrap();
        callback("true".into());
        assert_eq!(budget.used(), 4096);
        drop(script);
        drop(callback);
        assert_eq!(budget.used(), 0);
        for value in ["false", "null", "\"true\"", ""] {
            let receipt = Delivery::proof_check(&proof, &budget)
                .unwrap()
                .submit(|_, callback| {
                    callback(value.into());
                    Ok(())
                })
                .unwrap();
            assert_eq!(
                tauri::async_runtime::block_on(receipt.wait()),
                Err(DeliveryError::Disconnected)
            );
            assert_eq!(budget.used(), 0);
        }
        assert!(Delivery::proof_check("\";injected", &budget).is_err());
        assert_eq!(budget.used(), 0);
    }
    fn make(budget: &ByteBudget) -> Delivery {
        Delivery::prepare(
            &"a".repeat(64),
            &"b".repeat(32),
            br#"{"outcome":"ok"}"#,
            budget,
        )
        .unwrap()
    }
    #[test]
    fn abandoned_waiter_keeps_queued_payload_charged_until_callback_is_destroyed() {
        let budget = ByteBudget::new(100_000);
        let mut queued = None;
        let receipt = make(&budget)
            .submit(|script, callback| {
                queued = Some((script, callback));
                Ok(())
            })
            .unwrap();
        let used = budget.used();
        drop(receipt);
        assert_eq!(budget.used(), used);
        let (script, callback) = queued.take().unwrap();
        callback("true".into());
        assert_eq!(budget.used(), used);
        drop(script);
        drop(callback);
        assert_eq!(budget.used(), 0);
    }
    #[test]
    fn dropped_operation_wakes_waiter_and_success_is_once_only() {
        let budget = ByteBudget::new(100_000);
        let receipt = make(&budget)
            .submit(|_, callback| {
                drop(callback);
                Ok(())
            })
            .unwrap();
        assert_eq!(
            tauri::async_runtime::block_on(receipt.wait()),
            Err(DeliveryError::Disconnected)
        );
        assert_eq!(budget.used(), 0);
        let receipt = make(&budget)
            .submit(|_, callback| {
                callback("true".into());
                callback("false".into());
                Ok(())
            })
            .unwrap();
        assert_eq!(tauri::async_runtime::block_on(receipt.wait()), Ok(()));
        assert_eq!(budget.used(), 0);
    }
    #[test]
    fn failed_submit_and_budget_pressure_release_only_finished_operations() {
        let budget = ByteBudget::new(20_000);
        assert!(
            make(&budget)
                .submit(|_, _| Err(DeliveryError::Disconnected))
                .is_err()
        );
        assert_eq!(budget.used(), 0);
        let mut callbacks = vec![];
        let mut receipts = vec![];
        while let Ok(delivery) =
            Delivery::prepare(&"a".repeat(64), &"b".repeat(32), b"null", &budget)
        {
            receipts.push(
                delivery
                    .submit(|_, callback| {
                        callbacks.push(callback);
                        Ok(())
                    })
                    .unwrap(),
            );
        }
        assert!(!callbacks.is_empty());
        drop(receipts);
        assert!(budget.used() > 0);
        drop(callbacks);
        assert_eq!(budget.used(), 0);
    }
    #[test]
    fn script_quotes_payload_instead_of_treating_json_as_javascript_source() {
        let budget = ByteBudget::new(100_000);
        let json = serde_json::to_vec(
            &serde_json::json!({"name":"\");throw Error('injection')//\n한글","__proto__":{"x":1}}),
        )
        .unwrap();
        let delivery = Delivery::prepare(&"a".repeat(64), &"b".repeat(32), &json, &budget).unwrap();
        let quoted = delivery
            .script
            .split(&format!("receive(\"{}\",", "b".repeat(32)))
            .nth(1)
            .unwrap()
            .strip_suffix(")===true;})()")
            .unwrap();
        let decoded: String = serde_json::from_str(quoted).unwrap();
        assert_eq!(decoded.as_bytes(), json);
        assert!(Delivery::prepare(&"a".repeat(64), "\";bad", &json, &budget).is_err());
    }
}
