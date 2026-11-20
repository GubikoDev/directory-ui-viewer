//! Desktop-only IPC. Native lifecycle owns contexts; JavaScript supplies no paths.
use super::{
    protocol::{Command, MAX_MESSAGE_BYTES, Request},
    replay::{self, CONTROL_REPLY_BYTES, ReplayService, RequestAdmission},
};
#[cfg(feature = "internal-metrics")]
use crate::runtime::metrics::{Recorder, Stage};
use crate::{
    domain::{
        controller::{ControlError, NativeContext, Scope},
        model::{FsIssue, IssueScope, WorkEvent},
    },
    platform::macos_runtime::{NativeRuntime, RuntimeResources},
    runtime::{
        clock::{Clock, SystemClock},
        events::EventSink,
        memory::{ByteBudget, ManagedBudgets, Reservation},
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
/// Admission hint only: fields borrow input, but serde can use scratch for
/// escaped keys/errors. Production calls hold Bridge's precharged hint workspace.
/// The full strict parser and normal validation still precede every effect. Large or escaped
/// hints cannot use the control reserve. No caller-supplied priority flag exists.
fn control_request(bytes: &[u8]) -> bool {
    #[derive(Deserialize)]
    struct Hint<'a> {
        #[serde(borrow)]
        request: HintRequest<'a>,
    }
    #[derive(Deserialize)]
    struct HintRequest<'a> {
        #[serde(borrow)]
        body: HintBody<'a>,
    }
    #[derive(Deserialize)]
    struct HintBody<'a> {
        operation: &'a str,
    }
    bytes.len() <= 4096
        && serde_json::from_slice::<Hint<'_>>(bytes)
            .is_ok_and(|hint| matches!(hint.request.body.operation, "cancelTask" | "closeSession"))
}
fn parse_bytes<T: for<'a> Deserialize<'a>>(
    bytes: &[u8],
    budget: &ByteBudget,
) -> Result<(T, Reservation), FsIssue> {
    if bytes.len() > MAX_MESSAGE_BYTES {
        return Err(issue("RESOURCE_LIMIT"));
    }
    // Input, serde-owned strings/container growth, and authentication future.
    let memory = budget
        .reserve(bytes.len() * 4 + 4096)
        .map_err(|_| issue("RESOURCE_LIMIT"))?;
    let input = serde_json::from_slice(bytes).map_err(|_| issue("INVALID_ARGUMENT"))?;
    Ok((input, memory))
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
    #[cfg(feature = "internal-metrics")]
    metrics: Recorder,
    document: Mutex<Document>,
    dispatch: Mutex<()>,
    hint_memory: Mutex<Reservation>,
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
        let scheduler = Arc::new(
            Scheduler::with_budget(64, 64, 320, budgets.work.clone())
                .map_err(|_| issue("RESOURCE_LIMIT"))?,
        );
        #[cfg(feature = "internal-metrics")]
        let metrics = Recorder::new(budgets.clone(), scheduler.handles())
            .map_err(|_| issue("RESOURCE_LIMIT"))?;
        let runtime = Arc::new(NativeRuntime::new(
            context.clone(),
            RuntimeResources {
                scheduler,
                budgets: budgets.clone(),
                clock: clock.clone(),
                events,
            },
        )?);
        Ok((
            Arc::new(Self {
                #[cfg(feature = "internal-metrics")]
                metrics,
                dispatch: Mutex::new(()),
                // One synchronous hint parser at a time, including escaped-key
                // scratch, recursion and errors, even when data admission is full.
                hint_memory: Mutex::new(
                    budgets
                        .work
                        .reserve(32 * 1024)
                        .map_err(|_| issue("RESOURCE_LIMIT"))?,
                ),
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
        #[cfg(feature = "internal-metrics")]
        let _metrics = self.metrics.span(Stage::DocumentBegin, Stage::DocumentEnd);
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
        budget: &ByteBudget,
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
        let probe = super::delivery::Delivery::proof_check(proof, budget).map_err(|error| {
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
        budget: &ByteBudget,
    ) -> Result<(T, Reservation), FsIssue> {
        let InvokeBody::Raw(bytes) = raw.body() else {
            return Err(issue("INVALID_ARGUMENT"));
        };
        parse_bytes(bytes, budget)
    }
    fn control_request(&self, request: &tauri::ipc::Request<'_>) -> bool {
        let _memory = self.hint_memory.lock().unwrap();
        matches!(request.body(), InvokeBody::Raw(bytes) if control_request(bytes))
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
    #[cfg(feature = "internal-metrics")]
    let _metrics = state.metrics.span(Stage::ConnectBegin, Stage::ConnectEnd);
    let (input, _memory): (Connect, _) = state.parse(&request, &state.budgets.work)?;
    if input.protocol_version != 1 {
        return Err(issue("VERSION_UNSUPPORTED"));
    }
    if input.client_nonce.is_empty() || input.client_nonce.len() > 128 {
        return Err(issue("INVALID_ARGUMENT"));
    }
    let context = state
        .authenticate(&window, &input.document_proof, &state.budgets.work)
        .await?;
    let mut document = state.document.lock().unwrap();
    if document.context != context {
        return Err(issue("CLIENT_EXPIRED"));
    }
    // A failed cache allocation is retryable with the same nonce/epoch.
    let epoch = state.runtime.connect(&context, &input.client_nonce)?;
    if document.replay.is_none() {
        // Controller epochs increase once per successfully connected document.
        let replay = ReplayService::new(epoch, state.budgets.replay.clone(), 256, 300_000)
            .map_err(control)?;
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
    #[cfg(feature = "internal-metrics")]
    let _metrics = state.metrics.span(Stage::RequestBegin, Stage::RequestEnd);
    let priority = state.control_request(&request);
    let budget = if priority {
        state.budgets.work.control()
    } else {
        state.budgets.work.clone()
    };
    let (input, memory): (Envelope, _) = state.parse(&request, &budget)?;
    // The command future still borrows the framework's incoming request while
    // awaiting reply delivery. Native execution can finish earlier, or outlive
    // an abandoned caller, so both lifetimes must retain the parsed/input cost.
    let request_memory = Arc::new(memory);
    if !super::delivery::valid_key(&input.response_key) {
        return Err(issue("INVALID_ARGUMENT"));
    }
    let number = input.request.validate()?;
    let context = state
        .authenticate(&window, &input.document_proof, &budget)
        .await?;
    let replay = state.replay(&context, number.epoch)?;
    let hash = replay::fingerprint(&input.request).map_err(control)?;
    let epoch = number.epoch;
    let mutation_bytes = mutation_reply_bytes(&input.request.body);
    let admission_ack =
        super::delivery::Delivery::admission(&input.document_proof, &input.response_key, &budget)
            .map_err(|_| issue("RESOURCE_LIMIT"))?;
    let (waiter, delivery) = {
        // Admission and immediate control effects have one order. Only native
        // picker/worker waits outlive this lock; no filesystem call runs here.
        let _dispatch = state.dispatch.lock().unwrap();
        // Replay admission evicts completed cache records when necessary and
        // reserves this caller's delivery before any effects become visible.
        let (admission, delivery) = replay
            .begin_for_delivery(
                number,
                &hash,
                state.clock.sample().monotonic_ms,
                mutation_bytes.unwrap_or(CONTROL_REPLY_BYTES),
                priority,
                mutation_bytes,
            )
            .map_err(control)?;
        let waiter = match admission {
            RequestAdmission::Wait(waiter) => waiter,
            RequestAdmission::Execute(completion, waiter) => {
                execute(
                    state.inner().clone(),
                    window.clone(),
                    context.clone(),
                    epoch,
                    input.request,
                    completion,
                    request_memory.clone(),
                );
                waiter
            }
        };
        (waiter, delivery)
    };
    // Immediate control effects have happened, while picker/disk waits remain
    // independent. JS may now send its next monotonically numbered request.
    let admitted = admission_ack
        .submit(|script, callback| {
            window
                .eval_with_callback(script, callback)
                .map_err(|_| super::delivery::DeliveryError::Disconnected)
        })
        .map_err(|_| issue("CLIENT_EXPIRED"))?;
    tokio::time::timeout(Duration::from_secs(10), admitted.wait())
        .await
        .map_err(|_| issue("CLIENT_EXPIRED"))?
        .map_err(|_| issue("CLIENT_EXPIRED"))?;
    let reply = waiter.wait().await.map_err(control)?;
    state.current(&context)?;
    let delivery = match delivery {
        Some(delivery) => {
            delivery.prepare(&input.document_proof, &input.response_key, reply.bytes())
        }
        None => super::delivery::Delivery::prepare(
            &input.document_proof,
            &input.response_key,
            reply.bytes(),
            &budget,
        ),
    }
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

fn mutation_reply_bytes(command: &Command) -> Option<usize> {
    // Root names <=1023 raw bytes; errors and metadata have fixed fields.
    // WorkRecord has <=32 samples with operation<=64 and entry ID<=256
    // UTF8 bytes, <=18 canonical counts, and generated opaque identifiers.
    // Include worst ASCII-control JSON escaping and the response envelope.
    match command {
        Command::ChooseRoot {} | Command::RefreshRoot { .. } => Some(16 * 1024),
        Command::StartListing { .. }
        | Command::StartUsageScan { .. }
        | Command::CancelTask { .. } => Some(80 * 1024),
        Command::CloseSession { .. } => Some(4 * 1024),
        _ => None,
    }
}

fn execute(
    bridge: Arc<Bridge>,
    window: WebviewWindow,
    context: NativeContext,
    epoch: u64,
    request: Request,
    completion: replay::Completion,
    memory: Arc<Reservation>,
) {
    let id = request.request_id;
    // Retain bounded control/root/WorkRecord temporaries through serialization.
    // Variable listing/usage clones carry an additional Charged owner themselves.
    let budget = if matches!(
        request.body,
        Command::CancelTask { .. } | Command::CloseSession { .. }
    ) {
        bridge.budgets.work.control()
    } else {
        bridge.budgets.work.clone()
    };
    let response_memory = match budget.reserve(64 * 1024) {
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
    #[cfg(feature = "internal-metrics")]
    bridge.metrics.emit(Stage::Startup);
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
    fn mutation_reply_and_delivery_are_admitted_before_late_pressure() {
        use crate::commands::delivery::Callback;
        use crate::domain::model::{Issues, RootData, WorkRecord};
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../src/features/explorer/model/wire-fixture.json"
        ))
        .unwrap();
        let mut root: RootData = serde_json::from_value(fixture["root"].clone()).unwrap();
        root.root.display_name = "\u{1}".repeat(1023);
        root.session.session_id = "s".repeat(128);
        root.session.root_entry_id = "e".repeat(256);
        root.root.entry_id = root.session.root_entry_id.clone();
        let mut work: WorkRecord = serde_json::from_value(fixture["work"].clone()).unwrap();
        work.issues = Issues::default();
        for _ in 0..32 {
            work.issues.record(FsIssue {
                code: "LINK_TRAVERSAL_BLOCKED".into(),
                operation: "\u{1}".repeat(64),
                scope: IssueScope::Entry,
                entry_id: Some("\u{1}".repeat(256)),
                native_code: Some(i32::MIN),
            });
        }
        work.session_id = "s".repeat(128);
        work.task_id = "t".repeat(256);
        work.target_id = "e".repeat(256);
        let cases = [
            (
                Command::ChooseRoot {},
                serde_json::to_value(&root).unwrap(),
                false,
            ),
            (
                Command::RefreshRoot {
                    session_id: "s".into(),
                    expected_generation: 1,
                },
                serde_json::to_value(root).unwrap(),
                false,
            ),
            (
                Command::StartListing {
                    session_id: "s".into(),
                    generation: 1,
                    directory_id: "e".into(),
                    category: crate::domain::model::Category::Files,
                },
                serde_json::to_value(&work).unwrap(),
                false,
            ),
            (
                Command::StartUsageScan {
                    session_id: "s".into(),
                    generation: 1,
                },
                serde_json::to_value(&work).unwrap(),
                false,
            ),
            (
                Command::CancelTask {
                    session_id: "s".into(),
                    generation: 1,
                    task_id: "t".into(),
                },
                serde_json::json!({"status":"accepted","work":work}),
                true,
            ),
            (
                Command::CloseSession {
                    session_id: "s".into(),
                },
                serde_json::json!({"status":"closed"}),
                true,
            ),
        ];
        for (command, data, priority) in cases {
            let shared = ByteBudget::with_headroom(4 * 1024 * 1024, 1024 * 1024);
            let replay = ReplayService::new(1, shared.clone(), 256, 1000).unwrap();
            let general_pressure = priority.then(|| shared.reserve(shared.available()).unwrap());
            let budget = if priority {
                shared.control()
            } else {
                shared.clone()
            };
            let input_and_typed = budget.reserve(96 * 1024).unwrap();
            let allowance = mutation_reply_bytes(&command).unwrap();
            let response =
                serde_json::json!({"requestId":"native-epoch-1:1","outcome":"ok","data":data});
            assert!(crate::runtime::serialization::json_bytes(&response).unwrap() <= allowance);
            let (RequestAdmission::Execute(completion, waiter), Some(delivery)) = replay
                .begin_for_delivery(
                    RequestNumber {
                        epoch: 1,
                        sequence: 1,
                    },
                    &replay::fingerprint(&command).unwrap(),
                    0,
                    allowance,
                    priority,
                    Some(allowance),
                )
                .unwrap()
            else {
                panic!("first request");
            };
            let pressure = budget.reserve(budget.available()).unwrap();
            let reply = completion.complete(&response).unwrap();
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(reply.bytes()).unwrap(),
                response
            );
            let delivery = delivery
                .prepare(&"a".repeat(64), &"b".repeat(32), reply.bytes())
                .unwrap();
            let mut callback: Option<Callback> = None;
            let receipt = delivery
                .submit(|_, cb| {
                    callback = Some(cb);
                    Ok(())
                })
                .unwrap();
            drop(receipt);
            drop((
                replay,
                waiter,
                reply,
                pressure,
                general_pressure,
                input_and_typed,
            ));
            assert!(
                shared.used() > 0,
                "abandoned callback still owns script charge"
            );
            drop(callback);
            assert_eq!(shared.used(), 0);
            assert!(shared.peak() <= shared.limit());
        }
    }
    #[test]
    fn bounded_control_hint_never_bypasses_strict_validation() {
        let budget = ByteBudget::with_headroom(128 * 1024, 64 * 1024);
        let pressure = budget.reserve(budget.available()).unwrap();
        for operation in ["cancelTask", "closeSession"] {
            let bytes = serde_json::to_vec(&serde_json::json!({
                "documentProof":"a".repeat(64), "responseKey":"b".repeat(32),
                "request":{"protocolVersion":1,"requestId":"native-epoch-1:1","body":{"operation":operation,"sessionId":"s"}}
            })).unwrap();
            assert!(control_request(&bytes));
            assert!(parse_bytes::<Envelope>(&bytes, &budget).is_err());
            let parsed = parse_bytes::<Envelope>(&bytes, &budget.control());
            // cancelTask is missing generation and taskId. Classification grants
            // bounded parsing headroom, never schema or filesystem authority.
            if operation == "cancelTask" {
                assert!(parsed.is_err());
            } else {
                parsed.unwrap().0.request.validate().unwrap();
            }
        }
        for bytes in [
            br#"{"request":{"body":{"operation":"chooseRoot"}}}"#.as_slice(),
            br#"{"priority":true,"request":{"body":{"operation":"readUsage"}}}"#.as_slice(),
            br#"{"request":{"body":{"operation":"closeSession","operation":"chooseRoot"}}}"#.as_slice(),
            br#"{"request":{"body":{"operation":"closeSession"}},"request":{"body":{"operation":"chooseRoot"}}}"#.as_slice(),
        ] { assert!(!control_request(bytes)); }
        let large = format!(
            "{{\"request\":{{\"body\":{{\"operation\":\"closeSession\"}}}},\"padding\":\"{}\"}}",
            "x".repeat(4096)
        );
        assert!(!control_request(large.as_bytes()));
        assert_eq!(budget.used(), pressure.bytes());
    }
    #[test]
    fn control_parse_auth_reply_and_delivery_share_headroom_under_pressure() {
        use crate::commands::delivery::{Callback, Delivery};
        let budget = ByteBudget::with_headroom(4 * 1024 * 1024, 1024 * 1024);
        let replay = ReplayService::new(1, budget.clone(), 256, 1000).unwrap();
        let pressure = budget.reserve(budget.available()).unwrap();
        let bytes = br#"{"documentProof":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","responseKey":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","request":{"protocolVersion":1,"requestId":"native-epoch-1:1","body":{"operation":"closeSession","sessionId":"s"}}}"#;
        assert!(control_request(bytes));
        let control = budget.control();
        let (input, input_memory) = parse_bytes::<Envelope>(bytes, &control).unwrap();
        let number = input.request.validate().unwrap();
        let proof = Delivery::proof_check(&input.document_proof, &control).unwrap();
        let proof_receipt = proof
            .submit(|_, callback| {
                callback("true".into());
                Ok(())
            })
            .unwrap();
        tauri::async_runtime::block_on(proof_receipt.wait()).unwrap();
        let hash = replay::fingerprint(&input.request).unwrap();
        let RequestAdmission::Execute(owner, waiter) = replay
            .begin_with_priority(number, &hash, 0, CONTROL_REPLY_BYTES, true)
            .unwrap()
        else {
            panic!()
        };
        let temporary = control.reserve(64 * 1024).unwrap();
        owner.complete(serde_json::json!({"requestId":input.request.request_id,"outcome":"ok","data":{"status":"closed"}})).unwrap();
        drop(temporary);
        drop(input_memory);
        let reply = tauri::async_runtime::block_on(waiter.wait()).unwrap();
        let delivery = Delivery::prepare(
            &input.document_proof,
            &input.response_key,
            reply.bytes(),
            &control,
        )
        .unwrap();
        let mut callback: Option<Callback> = None;
        let receipt = delivery
            .submit(|_, pending| {
                callback = Some(pending);
                Ok(())
            })
            .unwrap();
        drop(receipt); // abandoned await must retain its native callback cost
        drop(reply);
        drop(replay);
        drop(pressure);
        assert!(budget.used() > 0);
        callback.take().unwrap()("true".into());
        assert_eq!(budget.used(), 0);
        assert!(budget.peak() <= budget.limit());
    }
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
