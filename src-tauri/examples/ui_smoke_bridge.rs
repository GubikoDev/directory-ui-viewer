//! Test-only stdio transport for UI -> NativeAdapter -> real macOS runtime.
//! Owns and removes its temporary fixture. Never accepts paths from the UI.
#[cfg(target_os = "macos")]
mod smoke {
    use directory_ui_viewer_lib::{
        commands::protocol::{Command, Request},
        domain::{
            controller::{NativeContext, Scope},
            model::FsIssue,
        },
        platform::macos_runtime::{NativeRuntime, RuntimeResources},
        runtime::{clock::SystemClock, events::EventChannel, memory::ManagedBudgets},
        scan::service::CancelStatus,
        scheduler::Scheduler,
    };
    use serde_json::{Value, json};
    use std::{
        fs,
        io::{self, BufRead, Write},
        path::PathBuf,
        sync::Arc,
    };
    struct Fixture(PathBuf);
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    pub fn run() {
        let path = std::env::temp_dir().join(format!("duv-ui-smoke-{}", std::process::id()));
        fs::create_dir(&path).unwrap();
        let fixture = Fixture(path);
        fs::create_dir(fixture.0.join("child")).unwrap();
        fs::write(fixture.0.join("child/value.txt"), b"engine connected").unwrap();
        let (events, _receiver) = EventChannel::bounded(64);
        let budgets = ManagedBudgets::default();
        let resources = RuntimeResources {
            scheduler: Arc::new(Scheduler::with_budget(64, 64, 320, budgets.work.clone()).unwrap()),
            budgets,
            clock: Arc::new(SystemClock::default()),
            events: Arc::new(events),
        };
        let context = NativeContext {
            window: "main".into(),
            document: 1,
        };
        let runtime = NativeRuntime::new(context.clone(), resources).unwrap();
        let mut epoch = 0;
        for line in io::stdin().lock().lines() {
            let input: Value = serde_json::from_str(&line.unwrap()).unwrap();
            let output = if input["command"] == "connect_client" {
                epoch = runtime
                    .connect(&context, input["payload"]["clientNonce"].as_str().unwrap())
                    .unwrap();
                json!({"epoch": format!("native-epoch-{epoch}")})
            } else {
                let request: Request =
                    serde_json::from_value(input["payload"]["request"].clone()).unwrap();
                request.validate().unwrap();
                let result = dispatch(&runtime, &context, epoch, &fixture.0, request.body);
                match result {
                    Ok(data) => {
                        json!({"requestId": request.request_id, "outcome":"ok", "data":data})
                    }
                    Err(error) => {
                        json!({"requestId": request.request_id, "outcome":"error", "error":error})
                    }
                }
            };
            println!("{output}");
            io::stdout().flush().unwrap();
        }
    }
    fn dispatch(
        runtime: &NativeRuntime,
        context: &NativeContext,
        epoch: u64,
        fixture: &std::path::Path,
        command: Command,
    ) -> Result<Value, FsIssue> {
        let scope = |session_id, generation| Scope {
            session_id,
            generation,
        };
        Ok(match command {
            Command::ChooseRoot {} => {
                let ticket = runtime.begin_selection(context, epoch)?;
                let prepared = runtime.prepare_selection(&ticket, fixture)?;
                json!(
                    runtime
                        .finish_selection(ticket, Ok(Some(prepared)))?
                        .unwrap()
                )
            }
            Command::StartListing {
                session_id,
                generation,
                directory_id,
                category,
            } => {
                let (handle, work) = runtime.start_listing(
                    context,
                    epoch,
                    &scope(session_id, generation),
                    &directory_id,
                    category,
                )?;
                json!({"work":work,"listingRevision":handle.revision,"directoryId":directory_id,"category":category})
            }
            Command::ReadListingPage {
                session_id,
                generation,
                task_id,
                cursor,
                limit,
            } => {
                let page = runtime.read_listing(
                    context,
                    epoch,
                    &scope(session_id, generation),
                    &task_id,
                    cursor.as_deref(),
                    limit,
                )?;
                json!(&*page)
            }
            Command::StartUsageScan {
                session_id,
                generation,
            } => json!(runtime.start_usage(context, epoch, &scope(session_id, generation))?),
            Command::ReadUsage {
                session_id,
                generation,
                directory_ids,
            } => json!(runtime.read_usage(
                context,
                epoch,
                &scope(session_id, generation),
                &directory_ids
            )?),
            Command::ReadTask {
                session_id,
                generation,
                task_id,
            } => json!(runtime.read_task(
                context,
                epoch,
                &scope(session_id, generation),
                &task_id
            )?),
            Command::CancelTask {
                session_id,
                generation,
                task_id,
            } => {
                let (status, work) = runtime.cancel_task(
                    context,
                    epoch,
                    &scope(session_id, generation),
                    &task_id,
                )?;
                json!({"status":match status {CancelStatus::Accepted => "accepted",CancelStatus::AlreadyTerminal => "alreadyTerminal"},"work":work})
            }
            Command::RefreshRoot {
                session_id,
                expected_generation,
            } => {
                let ticket = runtime.begin_refresh(
                    context,
                    epoch,
                    &scope(session_id, expected_generation),
                )?;
                let prepared = runtime.prepare_refresh(&ticket)?;
                json!(
                    runtime
                        .finish_refresh(ticket, Ok(prepared))
                        .map_err(|failure| failure.error)?
                )
            }
            Command::CloseSession { session_id } => {
                json!({"status": if runtime.close_session(context, epoch, &session_id)? {"closed"} else {"alreadyClosed"}})
            }
        })
    }
}
#[cfg(target_os = "macos")]
fn main() {
    smoke::run();
}
#[cfg(not(target_os = "macos"))]
fn main() {
    panic!("UI smoke bridge requires macOS");
}
