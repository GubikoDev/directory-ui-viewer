#![cfg(target_os = "macos")]
use directory_ui_viewer_lib::{
    domain::{
        controller::{NativeContext, Scope},
        model::{Category, Coverage, Entry, Measure, Phase, RootData},
    },
    platform::macos_runtime::{NativeRuntime, RuntimeResources},
    runtime::{clock::SystemClock, events::EventChannel, memory::ManagedBudgets},
    scheduler::Scheduler,
};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "duv-native-runtime-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        fs::create_dir(path.join("root")).unwrap();
        Self(path.canonicalize().unwrap())
    }
    fn p(&self, relative: &str) -> PathBuf {
        self.0.join(relative)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}
fn context(document: u64) -> NativeContext {
    NativeContext {
        window: "main".into(),
        document,
    }
}
fn setup() -> (NativeRuntime, RuntimeResources) {
    let (events, _receiver) = EventChannel::bounded(8);
    let budgets = ManagedBudgets::default();
    let resources = RuntimeResources {
        scheduler: Arc::new(Scheduler::with_budget(64, 64, 320, budgets.work.clone()).unwrap()),
        budgets,
        clock: Arc::new(SystemClock::default()),
        events: Arc::new(events),
    };
    (
        NativeRuntime::new(context(1), resources.clone()).unwrap(),
        resources,
    )
}
fn choose(runtime: &NativeRuntime, epoch: u64, path: &Path) -> RootData {
    let ticket = runtime.begin_selection(&context(1), epoch).unwrap();
    let prepared = runtime.prepare_selection(&ticket, path).unwrap();
    runtime
        .finish_selection(ticket, Ok(Some(prepared)))
        .unwrap()
        .unwrap()
}
fn scope(root: &RootData) -> Scope {
    Scope {
        session_id: root.session.session_id.clone(),
        generation: root.session.generation,
    }
}
fn until(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
}
fn collect(runtime: &NativeRuntime, epoch: u64, scope: &Scope, task: &str) -> Vec<Entry> {
    let mut cursor = None;
    let mut rows = vec![];
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let page = runtime
            .read_listing(&context(1), epoch, scope, task, cursor.as_deref(), 200)
            .unwrap();
        rows.extend(page.entries.iter().cloned());
        cursor = page.next_cursor.clone();
        if cursor.is_none() {
            assert_eq!(page.coverage, Coverage::Complete);
            return rows;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
}
#[test]
fn real_session_flow_refreshes_same_object_expires_ids_and_closes_all_resources() {
    let f = Fixture::new();
    fs::create_dir(f.p("root/child")).unwrap();
    fs::write(f.p("root/child/value"), b"value").unwrap();
    let (runtime, resources) = setup();
    let epoch = runtime.connect(&context(1), "one").unwrap();
    let root = choose(&runtime, epoch, &f.p("root"));
    let old = scope(&root);
    let (listing, _) = runtime
        .start_listing(
            &context(1),
            epoch,
            &old,
            &root.root.entry_id,
            Category::Directories,
        )
        .unwrap();
    let rows = collect(&runtime, epoch, &old, &listing.task_id);
    assert_eq!(rows.len(), 1);
    let task = runtime.start_usage(&context(1), epoch, &old).unwrap();
    until(|| {
        runtime
            .read_task(&context(1), epoch, &old, &task.task_id)
            .unwrap()
            .phase
            .terminal()
    });
    assert_eq!(
        runtime
            .start_usage(&context(1), epoch, &old)
            .unwrap()
            .task_id,
        task.task_id
    );
    let summary = runtime
        .read_usage(&context(1), epoch, &old, &[rows[0].entry_id.clone()])
        .unwrap();
    assert!(matches!(summary[0].logical, Measure::Complete { .. }));
    drop(summary);
    fs::write(f.p("root/current"), b"yes").unwrap();
    let ticket = runtime.begin_refresh(&context(1), epoch, &old).unwrap();
    assert_eq!(
        runtime
            .read_task(&context(1), epoch, &old, &task.task_id)
            .unwrap_err()
            .code,
        "STALE_GENERATION"
    );
    let prepared = runtime.prepare_refresh(&ticket).unwrap();
    let refreshed = runtime.finish_refresh(ticket, Ok(prepared)).unwrap();
    let new = scope(&refreshed);
    assert_eq!(new.generation, 2);
    assert_ne!(refreshed.root.entry_id, root.root.entry_id);
    assert_eq!(
        runtime
            .read_usage(&context(1), epoch, &new, &[rows[0].entry_id.clone()])
            .unwrap_err()
            .code,
        "ENTRY_EXPIRED"
    );
    let (handle, _) = runtime
        .start_listing(
            &context(1),
            epoch,
            &new,
            &refreshed.root.entry_id,
            Category::Files,
        )
        .unwrap();
    let rows = collect(&runtime, epoch, &new, &handle.task_id);
    assert_eq!(
        rows.iter()
            .map(|e| e.display_name.as_str())
            .collect::<Vec<_>>(),
        vec!["current"]
    );
    assert!(
        runtime
            .close_session(&context(1), epoch, &new.session_id)
            .unwrap()
    );
    assert!(
        !runtime
            .close_session(&context(1), epoch, &new.session_id)
            .unwrap()
    );
    assert_eq!(
        runtime
            .start_usage(&context(1), epoch, &new)
            .unwrap_err()
            .code,
        "SESSION_CLOSED"
    );
    until(|| resources.scheduler.handles().open() == 0);
    drop(runtime);
    drop(resources.scheduler);
    until(|| resources.budgets.used() == 0);
}
#[test]
fn cancelled_failed_and_resource_limited_choices_preserve_existing_session() {
    let f = Fixture::new();
    let (runtime, resources) = setup();
    let epoch = runtime.connect(&context(1), "one").unwrap();
    let root = choose(&runtime, epoch, &f.p("root"));
    let current = scope(&root);
    let cancelled = runtime.begin_selection(&context(1), epoch).unwrap();
    assert!(
        runtime
            .finish_selection(cancelled, Ok(None))
            .unwrap()
            .is_none()
    );
    let failed = runtime.begin_selection(&context(1), epoch).unwrap();
    let result = runtime
        .prepare_selection(&failed, &f.p("missing"))
        .map(Some);
    assert_eq!(
        runtime.finish_selection(failed, result).unwrap_err().code,
        "NOT_FOUND"
    );
    let limited = runtime.begin_selection(&context(1), epoch).unwrap();
    let prepared = runtime.prepare_selection(&limited, &f.p("root")).unwrap();
    let full = resources
        .budgets
        .work
        .reserve(resources.budgets.work.available())
        .unwrap();
    assert_eq!(
        runtime
            .finish_selection(limited, Ok(Some(prepared)))
            .unwrap_err()
            .code,
        "RESOURCE_LIMIT"
    );
    drop(full);
    assert_eq!(
        runtime.root_data(&context(1), epoch, &current).unwrap(),
        root
    );
    assert_eq!(resources.scheduler.handles().open(), 1);
    let pending = runtime.begin_selection(&context(1), epoch).unwrap();
    assert!(
        runtime
            .finish_selection(pending, Ok(None))
            .unwrap()
            .is_none()
    );
}
#[test]
fn stale_picker_callbacks_and_documents_cannot_reapprove_or_disrupt_a_new_selection() {
    let f = Fixture::new();
    let (runtime, resources) = setup();
    let epoch = runtime.connect(&context(1), "one").unwrap();
    let root = choose(&runtime, epoch, &f.p("root"));
    let ticket = runtime.begin_selection(&context(1), epoch).unwrap();
    let prepared = runtime.prepare_selection(&ticket, &f.p("root")).unwrap();
    runtime
        .close_session(&context(1), epoch, &root.session.session_id)
        .unwrap();
    assert_eq!(
        runtime
            .finish_selection(ticket, Ok(Some(prepared)))
            .unwrap_err()
            .code,
        "CLIENT_EXPIRED"
    );
    assert_eq!(resources.scheduler.handles().open(), 0);
    let pending = runtime.begin_selection(&context(1), epoch).unwrap();
    runtime.document_loaded(context(2));
    let epoch2 = runtime.connect(&context(2), "two").unwrap();
    assert_ne!(epoch2, epoch);
    // A non-existent path would report NOT_FOUND if candidate I/O ran.
    assert_eq!(
        runtime
            .prepare_selection(&pending, &f.p("missing"))
            .err()
            .unwrap()
            .code,
        "CLIENT_EXPIRED"
    );
    assert_eq!(
        runtime
            .finish_selection(pending, Ok(None))
            .unwrap_err()
            .code,
        "CLIENT_EXPIRED"
    );
    assert_eq!(
        runtime.connect(&context(1), "one").unwrap_err().code,
        "CLIENT_EXPIRED"
    );
    let newer = runtime.begin_selection(&context(2), epoch2).unwrap();
    runtime.finish_selection(newer, Ok(None)).unwrap();
    assert_eq!(resources.scheduler.handles().peak(), 2);
}
#[test]
fn duplicate_document_notice_keeps_session_and_closed_id_idempotence() {
    let f = Fixture::new();
    let (runtime, _) = setup();
    let epoch = runtime.connect(&context(1), "one").unwrap();
    let root = choose(&runtime, epoch, &f.p("root"));
    runtime.document_loaded(context(1));
    assert_eq!(runtime.connect(&context(1), "one").unwrap(), epoch);
    assert_eq!(
        runtime
            .root_data(&context(1), epoch, &scope(&root))
            .unwrap(),
        root
    );
    runtime
        .close_session(&context(1), epoch, &root.session.session_id)
        .unwrap();
    runtime.document_loaded(context(1));
    assert!(
        !runtime
            .close_session(&context(1), epoch, &root.session.session_id)
            .unwrap()
    );
}
#[test]
fn refresh_failure_keeps_new_generation_failed_and_retry_keeps_original_grant() {
    let f = Fixture::new();
    let (runtime, resources) = setup();
    let epoch = runtime.connect(&context(1), "one").unwrap();
    let root = choose(&runtime, epoch, &f.p("root"));
    let old = scope(&root);
    let ticket = runtime.begin_refresh(&context(1), epoch, &old).unwrap();
    let prepared = runtime.prepare_refresh(&ticket).unwrap();
    let full = resources
        .budgets
        .work
        .reserve(resources.budgets.work.available())
        .unwrap();
    let failure = runtime.finish_refresh(ticket, Ok(prepared)).unwrap_err();
    assert_eq!(failure.error.code, "RESOURCE_LIMIT");
    let wire = serde_json::to_value(&failure).unwrap();
    assert_eq!(wire["currentScope"]["generation"], 2);
    assert_eq!(wire["currentScope"]["sessionId"], old.session_id);
    drop(full);
    // Retry uses the returned scope, never a guessed increment or old value.
    let failed_scope = failure.current_scope.unwrap();
    assert_eq!(
        runtime
            .root_data(&context(1), epoch, &failed_scope)
            .unwrap_err()
            .code,
        "RESOURCE_LIMIT"
    );
    assert_eq!(
        runtime
            .root_data(&context(1), epoch, &old)
            .unwrap_err()
            .code,
        "STALE_GENERATION"
    );
    let retry = runtime
        .begin_refresh(&context(1), epoch, &failed_scope)
        .unwrap();
    let prepared = runtime.prepare_refresh(&retry).unwrap();
    assert_eq!(
        runtime
            .finish_refresh(retry, Ok(prepared))
            .unwrap()
            .session
            .generation,
        3
    );
    assert_eq!(
        resources.scheduler.handles().peak(),
        1,
        "refresh preserves the original persistent grant"
    );
}
#[test]
fn prepared_selection_cannot_be_paired_with_a_different_ticket() {
    let f = Fixture::new();
    let (runtime, resources) = setup();
    let epoch = runtime.connect(&context(1), "one").unwrap();
    let first = runtime.begin_selection(&context(1), epoch).unwrap();
    let prepared = runtime.prepare_selection(&first, &f.p("root")).unwrap();
    runtime.finish_selection(first, Ok(None)).unwrap();
    let second = runtime.begin_selection(&context(1), epoch).unwrap();
    assert_eq!(
        runtime
            .finish_selection(second, Ok(Some(prepared)))
            .unwrap_err()
            .code,
        "CLIENT_EXPIRED"
    );
    assert_eq!(resources.scheduler.handles().open(), 0);
}
#[test]
fn parked_listing_cancellation_and_document_close_release_resources() {
    let f = Fixture::new();
    for i in 0..600 {
        fs::write(f.p(&format!("root/f{i:03}")), b"x").unwrap();
    }
    let (runtime, resources) = setup();
    let epoch = runtime.connect(&context(1), "one").unwrap();
    let root = choose(&runtime, epoch, &f.p("root"));
    let current = scope(&root);
    let (handle, _) = runtime
        .start_listing(
            &context(1),
            epoch,
            &current,
            &root.root.entry_id,
            Category::Files,
        )
        .unwrap();
    until(|| resources.scheduler.counts().2 == 1);
    let (_, work) = runtime
        .cancel_task(&context(1), epoch, &current, &handle.task_id)
        .unwrap();
    assert_eq!(work.phase, Phase::Cancelled);
    until(|| resources.scheduler.handles().open() == 1);
    let page = runtime
        .read_listing(&context(1), epoch, &current, &handle.task_id, None, 200)
        .unwrap();
    assert_eq!(page.entries.len(), 200);
    assert_eq!(page.coverage, Coverage::Partial);
    runtime.document_loaded(context(2));
    assert_eq!(
        runtime
            .read_task(&context(1), epoch, &current, &handle.task_id)
            .unwrap_err()
            .code,
        "CLIENT_EXPIRED"
    );
    until(|| resources.scheduler.handles().open() == 0);
    assert!(resources.budgets.work.used() > resources.scheduler.fixed_bytes() + 32 * 1024);
    drop(page);
    assert_eq!(
        resources.budgets.work.used(),
        resources.scheduler.fixed_bytes() + 32 * 1024
    );
    drop(runtime);
    drop(resources.scheduler);
    until(|| resources.budgets.used() == 0);
}

#[test]
fn superseded_and_closed_refresh_completions_do_not_restore_old_generations() {
    let f = Fixture::new();
    let (runtime, resources) = setup();
    let epoch = runtime.connect(&context(1), "one").unwrap();
    let root = choose(&runtime, epoch, &f.p("root"));
    let first = runtime
        .begin_refresh(&context(1), epoch, &scope(&root))
        .unwrap();
    let prepared_first = runtime.prepare_refresh(&first).unwrap();
    let second = runtime
        .begin_refresh(
            &context(1),
            epoch,
            &Scope {
                generation: 2,
                ..scope(&root)
            },
        )
        .unwrap();
    let prepared_second = runtime.prepare_refresh(&second).unwrap();
    let new = runtime.finish_refresh(second, Ok(prepared_second)).unwrap();
    assert_eq!(
        runtime
            .finish_refresh(first, Ok(prepared_first))
            .unwrap_err()
            .error
            .code,
        "STALE_GENERATION"
    );
    assert_eq!(
        runtime.root_data(&context(1), epoch, &scope(&new)).unwrap(),
        new
    );
    let closing = runtime
        .begin_refresh(&context(1), epoch, &scope(&new))
        .unwrap();
    let prepared = runtime.prepare_refresh(&closing).unwrap();
    runtime
        .close_session(&context(1), epoch, &new.session.session_id)
        .unwrap();
    assert_eq!(
        runtime
            .finish_refresh(closing, Ok(prepared))
            .unwrap_err()
            .error
            .code,
        "SESSION_CLOSED"
    );
    assert_eq!(resources.scheduler.handles().open(), 0);
    assert_eq!(
        resources.budgets.work.used(),
        resources.scheduler.fixed_bytes() + 32 * 1024
    );
}

#[test]
fn refresh_ticket_rejects_prepared_candidate_from_another_selection() {
    let f = Fixture::new();
    let (runtime, resources) = setup();
    let epoch = runtime.connect(&context(1), "one").unwrap();
    let root = choose(&runtime, epoch, &f.p("root"));
    let selection = runtime.begin_selection(&context(1), epoch).unwrap();
    let candidate = runtime.prepare_selection(&selection, &f.p("root")).unwrap();
    let refresh = runtime
        .begin_refresh(&context(1), epoch, &scope(&root))
        .unwrap();
    assert_eq!(
        runtime
            .finish_refresh(refresh, Ok(candidate))
            .unwrap_err()
            .error
            .code,
        "ENTRY_CHANGED"
    );
    assert_eq!(
        runtime
            .finish_selection(selection, Ok(None))
            .unwrap_err()
            .code,
        "CLIENT_EXPIRED"
    );
    assert_eq!(resources.scheduler.handles().open(), 1);
}

#[test]
fn repeated_close_of_an_older_session_never_closes_a_new_active_root() {
    let f = Fixture::new();
    let (runtime, resources) = setup();
    let epoch = runtime.connect(&context(1), "one").unwrap();
    let first = choose(&runtime, epoch, &f.p("root"));
    runtime
        .close_session(&context(1), epoch, &first.session.session_id)
        .unwrap();
    let second = choose(&runtime, epoch, &f.p("root"));
    assert_eq!(
        runtime
            .close_session(&context(1), epoch, &first.session.session_id)
            .unwrap_err()
            .code,
        "SESSION_CLOSED"
    );
    assert_eq!(
        runtime
            .root_data(&context(1), epoch, &scope(&second))
            .unwrap(),
        second
    );
    runtime
        .close_session(&context(1), epoch, &second.session.session_id)
        .unwrap();
    assert!(
        !runtime
            .close_session(&context(1), epoch, &first.session.session_id)
            .unwrap()
    );
    assert_eq!(resources.scheduler.handles().open(), 0);
}

#[test]
fn async_selection_and_refresh_execute_metadata_on_the_same_fixed_foreground_worker() {
    use directory_ui_viewer_lib::runtime::clock::{Clock, TimeSample};
    use std::sync::Mutex;
    struct ThreadClock {
        inner: SystemClock,
        calls: Mutex<Vec<String>>,
    }
    impl Clock for ThreadClock {
        fn sample(&self) -> TimeSample {
            self.calls
                .lock()
                .unwrap()
                .push(std::thread::current().name().unwrap_or("unnamed").into());
            self.inner.sample()
        }
    }
    let f = Fixture::new();
    let (_, mut resources) = setup();
    let clock = Arc::new(ThreadClock {
        inner: SystemClock::default(),
        calls: Mutex::new(vec![]),
    });
    resources.clock = clock.clone();
    let runtime = Arc::new(NativeRuntime::new(context(1), resources.clone()).unwrap());
    let epoch = runtime.connect(&context(1), "one").unwrap();
    let ticket = runtime.begin_selection(&context(1), epoch).unwrap();
    let root =
        tauri::async_runtime::block_on(runtime.clone().select_path(ticket, Some(f.p("root"))))
            .unwrap()
            .unwrap();
    fs::write(f.p("root/current"), b"fresh").unwrap();
    let refreshed =
        tauri::async_runtime::block_on(runtime.clone().refresh(context(1), epoch, scope(&root)))
            .unwrap();
    assert_eq!(refreshed.session.generation, 2);
    let calls = clock.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 2);
    assert!(calls.iter().all(|name| name == "directory-foreground"));
    assert_eq!(resources.scheduler.handles().peak(), 1);
    runtime
        .close_session(&context(1), epoch, &root.session.session_id)
        .unwrap();
    drop(runtime);
    drop(resources.scheduler);
    until(|| resources.budgets.used() == 0);
}

struct BlockForeground {
    entered: std::sync::mpsc::SyncSender<()>,
    release: std::sync::mpsc::Receiver<()>,
}
impl directory_ui_viewer_lib::scheduler::Job for BlockForeground {
    fn step(
        &mut self,
        _: &directory_ui_viewer_lib::domain::controller::Cancellation,
        _: &directory_ui_viewer_lib::scheduler::HandleBudget,
    ) -> directory_ui_viewer_lib::scheduler::JobStep {
        self.entered.send(()).unwrap();
        self.release.recv_timeout(Duration::from_secs(5)).unwrap();
        directory_ui_viewer_lib::scheduler::JobStep::Completed
    }
}
fn block_foreground(resources: &RuntimeResources) -> std::sync::mpsc::SyncSender<()> {
    use directory_ui_viewer_lib::scheduler::Lane;
    let (entered, ready) = std::sync::mpsc::sync_channel(1);
    let (release, wait) = std::sync::mpsc::sync_channel(1);
    resources
        .scheduler
        .submit(
            "fixture-blocker".into(),
            Scope {
                session_id: "fixture-blocker".into(),
                generation: 1,
            },
            Lane::Foreground,
            BlockForeground {
                entered,
                release: wait,
            },
        )
        .unwrap();
    ready.recv_timeout(Duration::from_secs(5)).unwrap();
    release
}
#[test]
fn closing_queued_preparation_returns_without_waiting_and_does_not_cancel_new_work() {
    let f = Fixture::new();
    let (runtime, resources) = setup();
    let runtime = Arc::new(runtime);
    let epoch = runtime.connect(&context(1), "one").unwrap();
    let root = choose(&runtime, epoch, &f.p("root"));
    let release = block_foreground(&resources);
    let ticket = runtime.begin_selection(&context(1), epoch).unwrap();
    // The missing path must never be touched by this queued, invalidated request.
    let queued =
        tauri::async_runtime::spawn(runtime.clone().select_path(ticket, Some(f.p("missing"))));
    until(|| resources.scheduler.counts().1 == 1);
    let start = Instant::now();
    runtime
        .close_session(&context(1), epoch, &root.session.session_id)
        .unwrap();
    assert!(start.elapsed() < Duration::from_millis(100));
    assert_eq!(
        tauri::async_runtime::block_on(queued)
            .unwrap()
            .unwrap_err()
            .code,
        "CLIENT_EXPIRED"
    );
    assert_eq!(resources.scheduler.handles().open(), 0);
    let next = runtime.begin_selection(&context(1), epoch).unwrap();
    let new_work =
        tauri::async_runtime::spawn(runtime.clone().select_path(next, Some(f.p("root"))));
    until(|| resources.scheduler.counts().1 == 1);
    assert_eq!(
        resources.scheduler.handles().open(),
        0,
        "no replacement worker bypasses the blocked foreground lane"
    );
    release.send(()).unwrap();
    assert!(
        tauri::async_runtime::block_on(new_work)
            .unwrap()
            .unwrap()
            .is_some()
    );
    drop(runtime);
    drop(resources.scheduler);
    until(|| resources.budgets.used() == 0);
}
#[test]
fn dropping_async_receiver_cancels_queued_preparation_and_releases_picker_slot() {
    let f = Fixture::new();
    let (runtime, resources) = setup();
    let runtime = Arc::new(runtime);
    let epoch = runtime.connect(&context(1), "one").unwrap();
    let release = block_foreground(&resources);
    let baseline = resources.budgets.work.used();
    let ticket = runtime.begin_selection(&context(1), epoch).unwrap();
    let queued =
        tauri::async_runtime::spawn(runtime.clone().select_path(ticket, Some(f.p("root"))));
    until(|| resources.scheduler.counts().1 == 1);
    queued.abort();
    assert!(tauri::async_runtime::block_on(queued).is_err());
    until(|| resources.scheduler.counts().1 == 0);
    let next = runtime.begin_selection(&context(1), epoch).unwrap();
    runtime.finish_selection(next, Ok(None)).unwrap();
    assert_eq!(resources.scheduler.handles().open(), 0);
    until(|| resources.budgets.work.used() == baseline);
    release.send(()).unwrap();
}
#[test]
fn queue_admission_failure_preserves_choice_and_returns_advanced_refresh_scope() {
    let f = Fixture::new();
    let (_, mut resources) = setup();
    resources.scheduler =
        Arc::new(Scheduler::with_budget(0, 64, 320, resources.budgets.work.clone()).unwrap());
    let runtime = Arc::new(NativeRuntime::new(context(1), resources.clone()).unwrap());
    let epoch = runtime.connect(&context(1), "one").unwrap();
    let root = choose(&runtime, epoch, &f.p("root"));
    let ticket = runtime.begin_selection(&context(1), epoch).unwrap();
    assert_eq!(
        tauri::async_runtime::block_on(runtime.clone().select_path(ticket, Some(f.p("missing"))))
            .unwrap_err()
            .code,
        "RESOURCE_LIMIT"
    );
    assert_eq!(
        runtime
            .root_data(&context(1), epoch, &scope(&root))
            .unwrap(),
        root
    );
    let failure =
        tauri::async_runtime::block_on(runtime.clone().refresh(context(1), epoch, scope(&root)))
            .unwrap_err();
    assert_eq!(failure.error.code, "RESOURCE_LIMIT");
    assert_eq!(failure.current_scope.unwrap().generation, 2);
    assert_eq!(resources.scheduler.handles().peak(), 1);
    let next = runtime.begin_selection(&context(1), epoch).unwrap();
    runtime.finish_selection(next, Ok(None)).unwrap();
}

#[test]
fn abandoning_active_preparation_keeps_old_root_and_discards_late_candidate() {
    use directory_ui_viewer_lib::runtime::clock::{Clock, TimeSample};
    use std::sync::{Mutex, atomic::AtomicBool};
    struct PausedClock {
        inner: SystemClock,
        paused: AtomicBool,
        entered: std::sync::mpsc::SyncSender<()>,
        release: Mutex<std::sync::mpsc::Receiver<()>>,
    }
    impl Clock for PausedClock {
        fn sample(&self) -> TimeSample {
            if self.paused.load(Ordering::Acquire) {
                self.entered.send(()).unwrap();
                self.release
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap();
            }
            self.inner.sample()
        }
    }
    let f = Fixture::new();
    fs::create_dir(f.p("candidate")).unwrap();
    let (_, mut resources) = setup();
    let (entered, ready) = std::sync::mpsc::sync_channel(1);
    let (release, waiting) = std::sync::mpsc::sync_channel(1);
    let clock = Arc::new(PausedClock {
        inner: SystemClock::default(),
        paused: AtomicBool::new(false),
        entered,
        release: Mutex::new(waiting),
    });
    resources.clock = clock.clone();
    let runtime = Arc::new(NativeRuntime::new(context(1), resources.clone()).unwrap());
    let epoch = runtime.connect(&context(1), "one").unwrap();
    let root = choose(&runtime, epoch, &f.p("root"));
    clock.paused.store(true, Ordering::Release);
    let ticket = runtime.begin_selection(&context(1), epoch).unwrap();
    let pending =
        tauri::async_runtime::spawn(runtime.clone().select_path(ticket, Some(f.p("candidate"))));
    ready.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(
        resources.scheduler.handles().open(),
        2,
        "candidate owns a native handle while metadata observation drains"
    );
    pending.abort();
    assert!(tauri::async_runtime::block_on(pending).is_err());
    assert_eq!(
        runtime
            .root_data(&context(1), epoch, &scope(&root))
            .unwrap(),
        root
    );
    clock.paused.store(false, Ordering::Release);
    release.send(()).unwrap();
    until(|| resources.scheduler.handles().open() == 1);
    assert_eq!(
        runtime
            .root_data(&context(1), epoch, &scope(&root))
            .unwrap(),
        root
    );
    let next = runtime.begin_selection(&context(1), epoch).unwrap();
    runtime.finish_selection(next, Ok(None)).unwrap();
    drop(runtime);
    drop(resources.scheduler);
    until(|| resources.budgets.used() == 0);
}

#[test]
fn native_scanner_uses_the_runtime_hardlink_and_work_pools() {
    use directory_ui_viewer_lib::{
        domain::model::Reason,
        runtime::{events::DiscardEvents, memory::ByteBudget},
    };
    use std::os::unix::fs::MetadataExt;
    let fixture = Fixture::new();
    fs::write(fixture.p("root/first"), b"shared payload").unwrap();
    fs::hard_link(fixture.p("root/first"), fixture.p("root/second")).unwrap();
    let own = fs::symlink_metadata(fixture.p("root")).unwrap().size() as u128;
    let shared = fs::symlink_metadata(fixture.p("root/first"))
        .unwrap()
        .size() as u128;
    for limit in [0, 160] {
        let budgets = ManagedBudgets {
            hardlinks: ByteBudget::new(limit),
            ..ManagedBudgets::default()
        };
        let resources = RuntimeResources {
            scheduler: Arc::new(Scheduler::with_budget(64, 64, 320, budgets.work.clone()).unwrap()),
            budgets,
            clock: Arc::new(SystemClock::default()),
            events: Arc::new(DiscardEvents),
        };
        let runtime = NativeRuntime::new(context(1), resources.clone()).unwrap();
        let epoch = runtime.connect(&context(1), "budget").unwrap();
        let root = choose(&runtime, epoch, &fixture.p("root"));
        let current = scope(&root);
        let baseline = resources.budgets.work.used();
        let task = runtime.start_usage(&context(1), epoch, &current).unwrap();
        until(|| {
            runtime
                .read_task(&context(1), epoch, &current, &task.task_id)
                .unwrap()
                .phase
                .terminal()
        });
        let usage = runtime
            .read_usage(&context(1), epoch, &current, &[root.root.entry_id])
            .unwrap()[0]
            .clone();
        if limit == 0 {
            assert!(
                matches!(usage.logical, Measure::Partial { observed_bytes, reasons } if observed_bytes == own.into() && reasons.contains(&Reason::ResourceLimit))
            );
        } else {
            assert!(
                matches!(usage.logical, Measure::Complete { bytes } if bytes == (own + shared).into())
            );
            assert_eq!(resources.budgets.hardlinks.peak(), 160);
        }
        assert!(resources.budgets.work.peak() >= baseline + 64 * 1024);
        assert!(resources.budgets.hardlinks.peak() <= limit);
        runtime
            .close_session(&context(1), epoch, &current.session_id)
            .unwrap();
        drop(runtime);
        drop(resources.scheduler);
        until(|| resources.budgets.used() == 0);
    }
}

#[test]
fn parked_native_task_cancels_and_session_closes_with_general_work_budget_full() {
    let f = Fixture::new();
    for i in 0..600 {
        fs::write(f.p(&format!("root/pressure-{i:03}")), b"x").unwrap();
    }
    let (runtime, resources) = setup();
    let epoch = runtime.connect(&context(1), "control-pressure").unwrap();
    let root = choose(&runtime, epoch, &f.p("root"));
    let current = scope(&root);
    let (task, _) = runtime
        .start_listing(
            &context(1),
            epoch,
            &current,
            &root.root.entry_id,
            Category::Files,
        )
        .unwrap();
    until(|| resources.scheduler.counts().2 == 1);
    let pressure = resources
        .budgets
        .work
        .reserve(resources.budgets.work.available())
        .unwrap();
    assert!(resources.budgets.work.reserve(1).is_err());
    let (status, work) = runtime
        .cancel_task(&context(1), epoch, &current, &task.task_id)
        .unwrap();
    assert_eq!(
        status,
        directory_ui_viewer_lib::scan::service::CancelStatus::Accepted
    );
    assert_eq!(work.phase, Phase::Cancelled);
    until(|| resources.scheduler.handles().open() == 1);
    let additional = resources
        .budgets
        .work
        .reserve(resources.budgets.work.available())
        .unwrap();
    assert!(resources.budgets.work.reserve(1).is_err());
    assert!(
        runtime
            .close_session(&context(1), epoch, &current.session_id)
            .unwrap()
    );
    assert_eq!(
        runtime
            .read_task(&context(1), epoch, &current, &task.task_id)
            .unwrap_err()
            .code,
        "SESSION_CLOSED"
    );
    until(|| resources.scheduler.handles().open() == 0);
    drop((pressure, additional, runtime));
    drop(resources.scheduler);
    until(|| resources.budgets.used() == 0);
}

#[test]
fn moved_root_refresh_fails_and_explicit_reselection_recovers() {
    let f = Fixture::new();
    fs::write(f.p("root/original"), b"selected").unwrap();
    let (runtime, resources) = setup();
    let epoch = runtime.connect(&context(1), "one").unwrap();
    let root = choose(&runtime, epoch, &f.p("root"));
    fs::rename(f.p("root"), f.p("moved")).unwrap();
    fs::create_dir(f.p("root")).unwrap();
    fs::write(f.p("root/replacement"), b"not selected").unwrap();
    let ticket = runtime
        .begin_refresh(&context(1), epoch, &scope(&root))
        .unwrap();
    let prepared = runtime.prepare_refresh(&ticket);
    let failure = runtime.finish_refresh(ticket, prepared).unwrap_err();
    assert_eq!(failure.error.code, "ENTRY_CHANGED");
    let recovered = choose(&runtime, epoch, &f.p("moved"));
    let current = scope(&recovered);
    let (task, _) = runtime
        .start_listing(
            &context(1),
            epoch,
            &current,
            &recovered.root.entry_id,
            Category::Files,
        )
        .unwrap();
    let rows = collect(&runtime, epoch, &current, &task.task_id);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].display_name, "original");
    runtime
        .close_session(&context(1), epoch, &current.session_id)
        .unwrap();
    until(|| resources.scheduler.handles().open() == 0);
}
