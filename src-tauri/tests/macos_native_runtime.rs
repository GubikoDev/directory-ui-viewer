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
    let resources = RuntimeResources {
        scheduler: Arc::new(Scheduler::default()),
        budgets: ManagedBudgets::default(),
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
        rows.extend(page.entries);
        cursor = page.next_cursor;
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
    fs::rename(f.p("root"), f.p("original")).unwrap();
    fs::create_dir(f.p("root")).unwrap();
    fs::write(f.p("root/replacement-secret"), b"wrong").unwrap();
    fs::write(f.p("original/current"), b"yes").unwrap();
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
        .reserve(resources.budgets.work.limit() - resources.budgets.work.used())
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
        .reserve(resources.budgets.work.limit() - resources.budgets.work.used())
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
        "refresh never reopens root pathname"
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
    assert_eq!(resources.budgets.work.used(), 32 * 1024);
    drop(runtime);
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
    assert_eq!(resources.budgets.work.used(), 32 * 1024);
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
