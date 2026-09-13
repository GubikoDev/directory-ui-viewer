//! Desktop-only IPC. Native lifecycle owns contexts; JavaScript supplies no paths.
use super::{
    protocol::{Command, MAX_MESSAGE_BYTES, Request},
    replay::{self, CONTROL_REPLY_BYTES, ReplayService, RequestAdmission},
};
use crate::{
    domain::{
        controller::{ControlError, NativeContext, Scope},
        model::{FsIssue, IssueScope, WorkEvent},
    },
    platform::macos_runtime::{NativeRuntime, RuntimeResources},
    runtime::{
        clock::{Clock, SystemClock},
        events::EventSink,
        memory::{ManagedBudgets, Reservation},
    },
    scan::service::CancelStatus,
    scheduler::Scheduler,
};
use serde::{Deserialize, Serialize};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tauri::{
    Manager, WebviewWindow,
    ipc::{InvokeBody, InvokeResponseBody},
    webview::{NewWindowResponse, PageLoadEvent},
};
use tauri_plugin_dialog::DialogExt;
use tokio::sync::{mpsc, oneshot};

const INIT: &str = r#"(() => {
  if (window !== window.top) return;
  const bytes = crypto.getRandomValues(new Uint8Array(32));
  Object.defineProperty(window, '__DIRECTORY_DOCUMENT_PROOF__', {
    value: Array.from(bytes, b => b.toString(16).padStart(2, '0')).join(''),
    writable: false, configurable: false, enumerable: false
  });
})();"#;

fn issue(code: &str) -> FsIssue {
    FsIssue {
        code: code.into(),
        operation: "request".into(),
        scope: IssueScope::Root,
        entry_id: None,
        native_code: None,
    }
}
fn control(error: ControlError) -> FsIssue {
    issue(match error {
        ControlError::ClientExpired => "CLIENT_EXPIRED",
        ControlError::InvalidArgument => "INVALID_ARGUMENT",
        ControlError::RequestExpired => "REQUEST_EXPIRED",
        ControlError::StaleGeneration => "STALE_GENERATION",
        ControlError::SessionClosed => "SESSION_CLOSED",
        ControlError::ResourceLimit => "RESOURCE_LIMIT",
    })
}
fn valid_proof(proof: &str) -> bool {
    proof.len() == 64 && proof.bytes().all(|b| b.is_ascii_hexdigit())
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Connect {
    document_proof: String,
    protocol_version: u8,
    client_nonce: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Envelope {
    document_proof: String,
    response_key: String,
    request: Request,
}
#[derive(Serialize)]
#[serde(
    tag = "outcome",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
enum Response<T> {
    Ok {
        request_id: String,
        data: T,
    },
    Cancelled {
        request_id: String,
    },
    Error {
        request_id: String,
        error: FsIssue,
        #[serde(skip_serializing_if = "Option::is_none")]
        current_scope: Option<Scope>,
    },
}
fn result<T>(id: String, value: Result<T, FsIssue>) -> Response<T> {
    match value {
        Ok(data) => Response::Ok {
            request_id: id,
            data,
        },
        Err(error) => Response::Error {
            request_id: id,
            error,
            current_scope: None,
        },
    }
}
struct Document {
    context: NativeContext,
    replay: Option<(u64, Arc<ReplayService>)>,
    proof: Option<String>,
}
/// Events carry IDs only. A full queue drops hints; readTask remains authoritative.
struct NativeEvents {
    sender: mpsc::Sender<WorkEvent>,
    _memory: Reservation,
}
impl EventSink for NativeEvents {
    fn try_emit(&self, event: WorkEvent) -> bool {
        self.sender.try_send(event).is_ok()
    }
}
pub struct Bridge {
    document: Mutex<Document>,
    dispatch: Mutex<()>,
    runtime: Arc<NativeRuntime>,
    budgets: ManagedBudgets,
    clock: Arc<dyn Clock>,
}
impl Bridge {
    fn new() -> Result<(Arc<Self>, mpsc::Receiver<WorkEvent>), FsIssue> {
        let budgets = ManagedBudgets::default();
        let clock: Arc<dyn Clock> = Arc::new(SystemClock::default());
        let (sender, receiver) = mpsc::channel(64);
        let events = Arc::new(NativeEvents {
            sender,
            _memory: budgets
                .work
                .reserve(64 * 1024)
                .map_err(|_| issue("RESOURCE_LIMIT"))?,
        });
        let context = NativeContext {
            window: "main".into(),
            document: 0,
        };
        let runtime = Arc::new(NativeRuntime::new(
            context.clone(),
            RuntimeResources {
                scheduler: Arc::new(Scheduler::new(64, 64, 320)),
                budgets: budgets.clone(),
                clock: clock.clone(),
                events,
            },
        )?);
        Ok((
            Arc::new(Self {
                dispatch: Mutex::new(()),
                document: Mutex::new(Document {
                    context,
                    replay: None,
                    proof: None,
                }),
                runtime,
                budgets,
                clock,
            }),
            receiver,
        ))
    }
    fn loaded(&self) {
        let mut document = self.document.lock().unwrap();
        document.proof = None;
        // Exhaustion fails closed; there is no wrapped document identity.
        document.context.document = document
            .context
            .document
            .checked_add(1)
            .expect("document counter exhausted");
        if let Some((_, replay)) = document.replay.take() {
            replay.close();
        }
        self.runtime.document_loaded(document.context.clone());
    }
    fn current(&self, context: &NativeContext) -> Result<(), FsIssue> {
        if self.document.lock().unwrap().context == *context {
            Ok(())
        } else {
            Err(issue("CLIENT_EXPIRED"))
        }
    }
    async fn authenticate(
        &self,
        window: &WebviewWindow,
        proof: &str,
    ) -> Result<NativeContext, FsIssue> {
        // Tauri async IPC runs off the main thread. Enforce that precondition:
        // after didCommitNavigation increments document, a proxy-queued eval
        // cannot run until Wry has drained pending_scripts and set it to None.
        // A main-thread immediate eval would not have that ordering guarantee.
        if unsafe { libc::pthread_main_np() } != 0 {
            return Err(issue("CLIENT_EXPIRED"));
        }
        if window.label() != "main" || !valid_proof(proof) {
            return Err(issue("CLIENT_EXPIRED"));
        }
        let context = self.document.lock().unwrap().context.clone();
        if context.document == 0 {
            return Err(issue("CLIENT_EXPIRED"));
        }
        let probe =
            super::delivery::Delivery::proof_check(proof, &self.budgets.work).map_err(|error| {
                issue(match error {
                    super::delivery::DeliveryError::ResourceLimit => "RESOURCE_LIMIT",
                    _ => "CLIENT_EXPIRED",
                })
            })?;
        let receipt = probe
            .submit(|script, callback| {
                window
                    .eval_with_callback(script, callback)
                    .map_err(|_| super::delivery::DeliveryError::Disconnected)
            })
            .map_err(|_| issue("CLIENT_EXPIRED"))?;
        tokio::time::timeout(Duration::from_secs(10), receipt.wait())
            .await
            .map_err(|_| issue("CLIENT_EXPIRED"))?
            .map_err(|_| issue("CLIENT_EXPIRED"))?;
        self.current(&context)?;
        Ok(context)
    }
    fn parse<T: for<'a> Deserialize<'a>>(
        &self,
        raw: &tauri::ipc::Request<'_>,
    ) -> Result<(T, Reservation), FsIssue> {
        let InvokeBody::Raw(bytes) = raw.body() else {
            return Err(issue("INVALID_ARGUMENT"));
        };
        if bytes.len() > MAX_MESSAGE_BYTES {
            return Err(issue("RESOURCE_LIMIT"));
        }
        // Input, serde-owned strings/container growth, and authentication future.
        let memory = self
            .budgets
            .work
            .reserve(bytes.len() * 4 + 4096)
            .map_err(|_| issue("RESOURCE_LIMIT"))?;
        let input = serde_json::from_slice(bytes).map_err(|_| issue("INVALID_ARGUMENT"))?;
        Ok((input, memory))
    }
    fn replay(&self, context: &NativeContext, epoch: u64) -> Result<Arc<ReplayService>, FsIssue> {
        let document = self.document.lock().unwrap();
        if document.context != *context {
            return Err(issue("CLIENT_EXPIRED"));
        }
        document
            .replay
            .as_ref()
            .filter(|(current, _)| *current == epoch)
            .map(|(_, replay)| replay.clone())
            .ok_or_else(|| issue("CLIENT_EXPIRED"))
    }
}

#[tauri::command]
pub async fn connect_client(
    window: WebviewWindow,
    state: tauri::State<'_, Arc<Bridge>>,
    request: tauri::ipc::Request<'_>,
) -> Result<InvokeResponseBody, FsIssue> {
    let (input, _memory): (Connect, _) = state.parse(&request)?;
    if input.protocol_version != 1 {
        return Err(issue("VERSION_UNSUPPORTED"));
    }
    if input.client_nonce.is_empty() || input.client_nonce.len() > 128 {
        return Err(issue("INVALID_ARGUMENT"));
    }
    let context = state.authenticate(&window, &input.document_proof).await?;
    let mut document = state.document.lock().unwrap();
    if document.context != context {
        return Err(issue("CLIENT_EXPIRED"));
    }
    // A failed cache allocation is retryable with the same nonce/epoch.
    let epoch = state.runtime.connect(&context, &input.client_nonce)?;
    if document.replay.is_none() {
        // Controller epochs increase once per successfully connected document.
        let replay =
            ReplayService::new(epoch, state.budgets.work.clone(), 256, 300_000).map_err(control)?;
        document.replay = Some((epoch, Arc::new(replay)));
    }
    document.proof = Some(input.document_proof);
    Ok(InvokeResponseBody::Json(format!(
        "{{\"epoch\":\"native-epoch-{epoch}\"}}"
    )))
}

#[tauri::command]
pub async fn filesystem_request(
    window: WebviewWindow,
    state: tauri::State<'_, Arc<Bridge>>,
    request: tauri::ipc::Request<'_>,
) -> Result<InvokeResponseBody, FsIssue> {
    let (input, memory): (Envelope, _) = state.parse(&request)?;
    if !super::delivery::valid_key(&input.response_key) {
        return Err(issue("INVALID_ARGUMENT"));
    }
    let number = input.request.validate()?;
    let context = state.authenticate(&window, &input.document_proof).await?;
    let replay = state.replay(&context, number.epoch)?;
    let hash = replay::fingerprint(&input.request).map_err(control)?;
    let epoch = number.epoch;
    let waiter = {
        // Admission and immediate control effects have one order. Only native
        // picker/worker waits outlive this lock; no filesystem call runs here.
        let _dispatch = state.dispatch.lock().unwrap();
        match replay
            .begin(
                number,
                &hash,
                state.clock.sample().monotonic_ms,
                CONTROL_REPLY_BYTES,
            )
            .map_err(control)?
        {
            RequestAdmission::Wait(waiter) => waiter,
            RequestAdmission::Execute(completion, waiter) => {
                execute(
                    state.inner().clone(),
                    window.clone(),
                    context.clone(),
                    epoch,
                    input.request,
                    completion,
                    memory,
                );
                waiter
            }
        }
    };
    let reply = waiter.wait().await.map_err(control)?;
    state.current(&context)?;
    let delivery = super::delivery::Delivery::prepare(
        &input.document_proof,
        &input.response_key,
        reply.bytes(),
        &state.budgets.work,
    )
    .map_err(|error| {
        issue(match error {
            super::delivery::DeliveryError::ResourceLimit => "RESOURCE_LIMIT",
            _ => "CLIENT_EXPIRED",
        })
    })?;
    drop(reply);
    let receipt = delivery
        .submit(|script, callback| {
            window
                .eval_with_callback(script, callback)
                .map_err(|_| super::delivery::DeliveryError::Disconnected)
        })
        .map_err(|_| issue("CLIENT_EXPIRED"))?;
    tokio::time::timeout(Duration::from_secs(10), receipt.wait())
        .await
        .map_err(|_| issue("CLIENT_EXPIRED"))?
        .map_err(|_| issue("CLIENT_EXPIRED"))?;
    state.current(&context)?;
    // Only a fixed-size acknowledgement uses the ordinary IPC response path.
    Ok(InvokeResponseBody::Json("null".into()))
}

fn execute(
    bridge: Arc<Bridge>,
    window: WebviewWindow,
    context: NativeContext,
    epoch: u64,
    request: Request,
    completion: replay::Completion,
    memory: Reservation,
) {
    let id = request.request_id;
    // Retain bounded control/root/WorkRecord temporaries through serialization.
    // Variable listing/usage clones carry an additional Charged owner themselves.
    let response_memory = match bridge.budgets.work.reserve(64 * 1024) {
        Ok(memory) => memory,
        Err(_) => {
            let _ = completion.complete(result::<()>(id, Err(issue("RESOURCE_LIMIT"))));
            return;
        }
    };
    let scope = request.body.scope();
    let runtime = bridge.runtime.clone();
    // Each branch serializes its typed response directly into the charged cache.
    match request.body {
        Command::ChooseRoot {} => {
            let ticket = runtime.begin_selection(&context, epoch);
            tauri::async_runtime::spawn(async move {
                let _memory = memory;
                let _response_memory = response_memory;
                let selected = async {
                    let ticket = ticket?;
                    let (tx, rx) = oneshot::channel();
                    window
                        .dialog()
                        .file()
                        .set_parent(&window)
                        .pick_folder(move |path| {
                            let _ = tx.send(path);
                        });
                    match rx.await {
                        Ok(path) => match path.map(|p| p.into_path()).transpose() {
                            Ok(path) => runtime.clone().select_path(ticket, path).await,
                            Err(_) => {
                                runtime.finish_selection(ticket, Err(issue("INVALID_ARGUMENT")))
                            }
                        },
                        Err(_) => runtime.finish_selection(ticket, Err(issue("INTERNAL"))),
                    }
                }
                .await;
                let response = match selected {
                    Ok(Some(data)) => Response::Ok {
                        request_id: id,
                        data,
                    },
                    Ok(None) => Response::Cancelled { request_id: id },
                    Err(error) => Response::Error {
                        request_id: id,
                        error,
                        current_scope: None,
                    },
                };
                let _ = completion.complete(response);
            });
        }
        Command::RefreshRoot { .. } => {
            let ticket = runtime.begin_refresh(&context, epoch, &scope.unwrap());
            tauri::async_runtime::spawn(async move {
                let _memory = memory;
                let _response_memory = response_memory;
                let response = match ticket {
                    Ok(ticket) => match runtime.refresh_prepared(ticket).await {
                        Ok(data) => Response::Ok {
                            request_id: id,
                            data,
                        },
                        Err(failure) => Response::Error {
                            request_id: id,
                            error: failure.error,
                            current_scope: failure.current_scope,
                        },
                    },
                    Err(error) => Response::Error {
                        request_id: id,
                        error,
                        current_scope: None,
                    },
                };
                let _ = completion.complete(response);
            });
        }
        Command::StartListing {
            directory_id,
            category,
            ..
        } => {
            #[derive(Serialize)]
            #[serde(rename_all = "camelCase")]
            struct Start {
                work: crate::domain::model::WorkRecord,
                listing_revision: String,
                directory_id: String,
                category: crate::domain::model::Category,
            }
            let value = runtime
                .start_listing(&context, epoch, &scope.unwrap(), &directory_id, category)
                .map(|(handle, work)| Start {
                    work,
                    listing_revision: handle.revision,
                    directory_id,
                    category,
                });
            let _ = completion.complete(result(id, value));
        }
        Command::ReadListingPage {
            task_id,
            cursor,
            limit,
            ..
        } => {
            let _ = completion.complete(result(
                id,
                runtime.read_listing(
                    &context,
                    epoch,
                    &scope.unwrap(),
                    &task_id,
                    cursor.as_deref(),
                    limit,
                ),
            ));
        }
        Command::StartUsageScan { .. } => {
            let _ = completion.complete(result(
                id,
                runtime.start_usage(&context, epoch, &scope.unwrap()),
            ));
        }
        Command::ReadUsage { directory_ids, .. } => {
            let _ = completion.complete(result(
                id,
                runtime.read_usage(&context, epoch, &scope.unwrap(), &directory_ids),
            ));
        }
        Command::ReadTask { task_id, .. } => {
            let _ = completion.complete(result(
                id,
                runtime.read_task(&context, epoch, &scope.unwrap(), &task_id),
            ));
        }
        Command::CancelTask { task_id, .. } => {
            #[derive(Serialize)]
            struct Cancel {
                status: &'static str,
                work: crate::domain::model::WorkRecord,
            }
            let value = runtime
                .cancel_task(&context, epoch, &scope.unwrap(), &task_id)
                .map(|(status, work)| Cancel {
                    status: match status {
                        CancelStatus::Accepted => "accepted",
                        CancelStatus::AlreadyTerminal => "alreadyTerminal",
                    },
                    work,
                });
            let _ = completion.complete(result(id, value));
        }
        Command::CloseSession { session_id } => {
            #[derive(Serialize)]
            struct Close {
                status: &'static str,
            }
            let value = runtime
                .close_session(&context, epoch, &session_id)
                .map(|closed| Close {
                    status: if closed { "closed" } else { "alreadyClosed" },
                });
            let _ = completion.complete(result(id, value));
        }
    }
}

pub fn setup(app: &mut tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    let (bridge, mut events) = Bridge::new().map_err(|e| std::io::Error::other(e.code))?;
    app.manage(bridge.clone());
    let lifecycle = bridge.clone();
    let config = app
        .config()
        .app
        .windows
        .iter()
        .find(|w| w.label == "main")
        .ok_or("main window config absent")?;
    let window = tauri::WebviewWindowBuilder::from_config(app, config)?
        .initialization_script(INIT)
        .on_navigation(|url| allowed_origin(url))
        .on_new_window(|_, _| NewWindowResponse::Deny)
        .on_page_load(move |_, payload| {
            // Wry's macOS Started callback is didCommitNavigation.
            if payload.event() == PageLoadEvent::Started {
                lifecycle.loaded();
            }
        })
        .build()?;
    let closing = bridge.clone();
    window.on_window_event(move |event| {
        if matches!(
            event,
            tauri::WindowEvent::CloseRequested { .. } | tauri::WindowEvent::Destroyed
        ) {
            closing.loaded();
        }
    });
    tauri::async_runtime::spawn(async move {
        let event_lane = Arc::new(tokio::sync::Semaphore::new(1));
        while let Some(event) = events.recv().await {
            let Ok(permit) = event_lane.clone().try_acquire_owned() else {
                // A timed-out WK callback still owns the lane. Drop hints until
                // that callback is destroyed; never consume more work budget.
                continue;
            };
            // read_task validates current ownership but returns a WorkRecord
            // clone; admit its bounded temporary before acquiring it.
            let Ok(memory) = bridge.budgets.work.reserve(64 * 1024) else {
                continue;
            };
            let proof = {
                let document = bridge.document.lock().unwrap();
                let valid = document.replay.as_ref().is_some_and(|(epoch, _)| {
                    bridge
                        .runtime
                        .read_task(
                            &document.context,
                            *epoch,
                            &Scope {
                                session_id: event.session_id.clone(),
                                generation: event.generation,
                            },
                            &event.task_id,
                        )
                        .is_ok()
                });
                if valid { document.proof.clone() } else { None }
            };
            let Some(proof) = proof else {
                continue;
            };
            let Ok(delivery) = super::delivery::Delivery::work_event(&proof, &event, memory) else {
                continue;
            };
            drop(proof);
            drop(event);
            let Ok(receipt) = delivery.submit_guarded(permit, |script, callback| {
                window
                    .eval_with_callback(script, callback)
                    .map_err(|_| super::delivery::DeliveryError::Disconnected)
            }) else {
                continue;
            };
            // One event in flight per loop; timeout does not release a callback
            // still owned by WebKit. The shared work budget bounds abandoned calls.
            let _ = tokio::time::timeout(Duration::from_secs(10), receipt.wait()).await;
        }
    });
    Ok(())
}
fn allowed_origin(url: &tauri::Url) -> bool {
    (url.scheme() == "tauri" && url.host_str() == Some("localhost"))
        || (cfg!(debug_assertions)
            && url.scheme() == "http"
            && url.host_str() == Some("127.0.0.1")
            && url.port() == Some(1420))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::protocol::RequestNumber;
    #[test]
    fn only_local_app_navigation_and_strict_envelopes_are_accepted() {
        assert!(allowed_origin(
            &"tauri://localhost/index.html".parse().unwrap()
        ));
        for url in [
            "https://example.com",
            "tauri://elsewhere",
            "http://127.0.0.1:9999",
            "file:///tmp/index.html",
            "data:text/html,test",
        ] {
            assert!(!allowed_origin(&url.parse().unwrap()));
        }
        assert!(valid_proof(&"a".repeat(64)));
        assert!(!valid_proof(&"a".repeat(63)));
        assert!(!valid_proof(&"z".repeat(64)));
        let valid = serde_json::json!({"documentProof":"a".repeat(64),"responseKey":"b".repeat(32),"request":{"protocolVersion":1,"requestId":"native-epoch-1:1","body":{"operation":"chooseRoot"}}});
        assert!(serde_json::from_value::<Envelope>(valid).is_ok());
        for extra in ["window", "document", "path"] {
            let value = serde_json::json!({"documentProof":"a".repeat(64),"responseKey":"b".repeat(32),"request":{"protocolVersion":1,"requestId":"native-epoch-1:1","body":{"operation":"chooseRoot"}},extra:1});
            assert!(serde_json::from_value::<Envelope>(value).is_err());
        }
    }
    #[test]
    fn native_document_commit_retires_epoch_pending_replies_and_rejects_old_context() {
        let (bridge, _events) = Bridge::new().unwrap();
        bridge.loaded();
        let context = bridge.document.lock().unwrap().context.clone();
        let epoch = bridge.runtime.connect(&context, "nonce").unwrap();
        let service =
            Arc::new(ReplayService::new(epoch, bridge.budgets.work.clone(), 4, 100).unwrap());
        bridge.document.lock().unwrap().replay = Some((epoch, service.clone()));
        bridge.document.lock().unwrap().proof = Some("a".repeat(64));
        let RequestAdmission::Execute(owner, waiter) = service
            .begin(
                RequestNumber { epoch, sequence: 1 },
                &"a".repeat(64),
                0,
                1024,
            )
            .unwrap()
        else {
            panic!()
        };
        bridge.loaded();
        assert!(bridge.document.lock().unwrap().proof.is_none());
        assert_eq!(bridge.current(&context).unwrap_err().code, "CLIENT_EXPIRED");
        assert!(bridge.replay(&context, epoch).is_err());
        assert!(tauri::async_runtime::block_on(waiter.wait()).is_err());
        assert!(
            owner
                .complete(serde_json::json!({"accepted":true}))
                .is_err()
        );
        let next = bridge.document.lock().unwrap().context.clone();
        assert_eq!(
            bridge.runtime.connect(&next, "new-nonce").unwrap(),
            epoch + 1
        );
    }
    #[test]
    fn refresh_failure_wire_preserves_new_scope_without_claiming_root_data() {
        let value = serde_json::to_value(Response::<()>::Error {
            request_id: "native-epoch-1:3".into(),
            error: issue("IO_ERROR"),
            current_scope: Some(Scope {
                session_id: "s".into(),
                generation: 2,
            }),
        })
        .unwrap();
        assert_eq!(value["outcome"], "error");
        assert_eq!(value["requestId"], "native-epoch-1:3");
        assert_eq!(value["currentScope"]["generation"], 2);
        assert!(value.get("data").is_none());
    }
}
