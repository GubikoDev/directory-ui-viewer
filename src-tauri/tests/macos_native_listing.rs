#![cfg(target_os = "macos")]
use directory_ui_viewer_lib::{
    domain::{
        controller::{Cancellation, Scope},
        index::{DirectoryIndex, DirectoryKey, Origin, SharedDirectoryIndex},
        model::{Category, Coverage, Entry, Field, Kind},
    },
    listing::{
        cache::{ListingCache, ListingHandle},
        service::{ListingService, ListingSource},
    },
    platform::{macos::ApprovedRoot, macos_listing::NativeListingSource},
    runtime::{clock::SystemClock, memory::ManagedBudgets},
    scheduler::Scheduler,
};
use std::{
    fs,
    os::unix::fs::symlink,
    path::PathBuf,
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
        let p = std::env::temp_dir().join(format!(
            "duv-native-list-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&p).unwrap();
        fs::create_dir(p.join("root")).unwrap();
        Self(p.canonicalize().unwrap())
    }
    fn p(&self, p: &str) -> PathBuf {
        self.0.join(p)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap()
    }
}
struct Setup {
    root: Arc<ApprovedRoot>,
    index: SharedDirectoryIndex,
    id: String,
    scheduler: Arc<Scheduler>,
    service: ListingService,
    budgets: ManagedBudgets,
}
impl Setup {
    fn new(f: &Fixture, ttl: u64, handles: usize) -> Self {
        let budgets = ManagedBudgets::default();
        let scheduler = Arc::new(Scheduler::new(64, 64, handles));
        let root = Arc::new(
            ApprovedRoot::open_picker_candidate(
                &f.p("root"),
                scheduler.handles(),
                budgets.work.clone(),
            )
            .unwrap(),
        );
        let index: SharedDirectoryIndex = DirectoryIndex::with_budgets(
            "native-list".into(),
            budgets.background_index.clone(),
            budgets.foreground_index.clone(),
        )
        .into();
        let id = index
            .register(
                DirectoryKey {
                    parent_id: None,
                    raw_name: vec![],
                },
                Origin::Foreground,
            )
            .unwrap();
        index
            .bind_identity(&id, root.directory().identity().fingerprint())
            .unwrap();
        let cache = ListingCache::with_budget(budgets.listings.clone(), ttl, 1_048_576);
        let service = ListingService::new(
            Scope {
                session_id: "session".into(),
                generation: 1,
            },
            index.clone(),
            scheduler.clone(),
            cache,
        );
        Self {
            root,
            index,
            id,
            scheduler,
            service,
            budgets,
        }
    }
    fn source(&self) -> NativeListingSource {
        NativeListingSource::new(
            self.root.clone(),
            self.index.clone(),
            self.id.clone(),
            Arc::new(SystemClock::default()),
        )
        .unwrap()
    }
    fn start(&self, id: &str, category: Category, now: u64) -> ListingHandle {
        self.service
            .start(
                id,
                category,
                200,
                "2026-09-14T00:00:00Z",
                now,
                self.source(),
            )
            .unwrap()
    }
    fn collect(&self, handle: &ListingHandle, now: u64) -> (Vec<Entry>, Coverage) {
        let mut rows = vec![];
        let mut cursor = None;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let page = self
                .service
                .page(&handle.task_id, cursor.as_deref(), 200, now)
                .unwrap();
            rows.extend(page.entries);
            cursor = page.next_cursor;
            if cursor.is_none() {
                return (rows, page.coverage);
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}
#[test]
fn native_pages_cover_long_names_links_hidden_and_lazy_child_listing() {
    let f = Fixture::new();
    fs::create_dir(f.p("root/a")).unwrap();
    fs::create_dir(f.p("root/a/b")).unwrap();
    fs::write(f.p("root/a/inside"), b"child").unwrap();
    for i in 0..1001 {
        fs::write(f.p(&format!("root/file-{i:04}")), b"x").unwrap()
    }
    let long = "한".repeat(255);
    fs::write(f.p(&format!("root/{long}")), b"long").unwrap();
    fs::write(f.p("root/.hidden"), b"").unwrap();
    fs::write(f.p("outside"), b"secret").unwrap();
    symlink(f.p("outside"), f.p("root/link")).unwrap();
    let setup = Setup::new(&f, 300_000, 320);
    let dirs = setup.start(&setup.id, Category::Directories, 0);
    let (directories, coverage) = setup.collect(&dirs, 0);
    assert_eq!(coverage, Coverage::Complete);
    assert_eq!(directories.len(), 1);
    assert_eq!(directories[0].display_name, "a");
    let a = directories[0].entry_id.clone();
    assert_eq!(setup.index.len(), 2, "unopened b stays undiscovered");
    let files = setup.start(&setup.id, Category::Files, 0);
    let (files, coverage) = setup.collect(&files, 0);
    assert_eq!(coverage, Coverage::Complete);
    assert_eq!(files.len(), 1004);
    assert!(files.iter().any(|e| e.display_name == long));
    assert!(
        files
            .iter()
            .any(|e| e.display_name == ".hidden" && e.hidden == Field::Known { value: true })
    );
    assert!(
        files
            .iter()
            .any(|e| e.display_name == "link" && e.kind == Kind::Symlink)
    );
    assert!(!files.iter().any(|e| e.display_name == "inside"));
    let handle = setup.start(&a, Category::Directories, 0);
    let (children, coverage) = setup.collect(&handle, 0);
    assert_eq!(coverage, Coverage::Complete);
    assert_eq!(children.len(), 1);
    assert_eq!(children[0].display_name, "b");
    let child_files = setup.start(&a, Category::Files, 0);
    let (children, coverage) = setup.collect(&child_files, 0);
    assert_eq!(coverage, Coverage::Complete);
    assert_eq!(children.len(), 1);
    assert_eq!(children[0].display_name, "inside");
    setup.service.close();
    assert!(setup.scheduler.handles().peak() <= 3);
}
#[test]
fn replacement_is_not_exposed_under_old_id_and_ancestor_replacement_blocks_reacquisition() {
    let f = Fixture::new();
    fs::create_dir(f.p("root/a")).unwrap();
    fs::create_dir(f.p("root/a/b")).unwrap();
    let setup = Setup::new(&f, 1, 320);
    let handle = setup.start(&setup.id, Category::Directories, 0);
    let (dirs, _) = setup.collect(&handle, 0);
    let a = dirs[0].entry_id.clone();
    let prior = setup.index.entry(&a).unwrap();
    let child = setup.start(&a, Category::Directories, 0);
    let (dirs, _) = setup.collect(&child, 0);
    let b = dirs[0].entry_id.clone();
    fs::rename(f.p("root/a"), f.p("old-a")).unwrap();
    fs::create_dir(f.p("root/a")).unwrap();
    fs::create_dir(f.p("root/a/b")).unwrap();
    fs::write(f.p("root/a/b/replacement-secret"), b"").unwrap();
    fs::create_dir(f.p("root/healthy")).unwrap();
    let mut source = setup.source();
    assert_eq!(
        source
            .open(&b, &setup.index, &Cancellation::default())
            .err()
            .unwrap()
            .code,
        "ENTRY_CHANGED"
    );
    assert!(setup.service.work(&handle.task_id, 2).is_err());
    let fresh = setup.start(&setup.id, Category::Directories, 3);
    let (rows, coverage) = setup.collect(&fresh, 3);
    assert_eq!(coverage, Coverage::Partial);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].display_name, "healthy");
    assert_eq!(setup.index.entry(&a).unwrap(), prior);
    assert!(
        setup
            .service
            .work(&fresh.task_id, 3)
            .unwrap()
            .issues
            .samples
            .iter()
            .any(|e| e.code == "ENTRY_CHANGED")
    );
}
#[test]
fn root_move_requires_reselection_and_pre_cancelled_resolution_opens_nothing() {
    let f = Fixture::new();
    fs::write(f.p("root/original"), b"").unwrap();
    let setup = Setup::new(&f, 300_000, 2);
    fs::rename(f.p("root"), f.p("moved-root")).unwrap();
    fs::create_dir(f.p("root")).unwrap();
    fs::write(f.p("root/replacement"), b"").unwrap();
    let mut source = setup.source();
    let cancel = Cancellation::default();
    cancel.cancel();
    assert_eq!(
        source
            .open(&setup.id, &setup.index, &cancel)
            .err()
            .unwrap()
            .code,
        "SESSION_CLOSED"
    );
    assert_eq!(setup.scheduler.handles().open(), 1);
    let handle = setup.start(&setup.id, Category::Files, 0);
    let (rows, coverage) = setup.collect(&handle, 0);
    assert_eq!(coverage, Coverage::Partial);
    assert!(rows.is_empty());
    assert!(
        setup
            .service
            .work(&handle.task_id, 0)
            .unwrap()
            .issues
            .samples
            .iter()
            .any(|error| error.code == "ENTRY_CHANGED")
    );
}
#[test]
fn wrong_index_and_work_budget_pressure_fail_without_fallback() {
    let f = Fixture::new();
    let setup = Setup::new(&f, 300_000, 320);
    let mut source = setup.source();
    let other: SharedDirectoryIndex = DirectoryIndex::new("other".into(), 100_000, 100_000).into();
    assert_eq!(
        source
            .open(&setup.id, &other, &Cancellation::default())
            .err()
            .unwrap()
            .code,
        "ENTRY_CHANGED"
    );
    let reservation = setup
        .budgets
        .work
        .reserve(setup.budgets.work.available())
        .unwrap();
    assert_eq!(
        source
            .open(&setup.id, &setup.index, &Cancellation::default())
            .err()
            .unwrap()
            .code,
        "RESOURCE_LIMIT"
    );
    assert_eq!(setup.scheduler.handles().open(), 1);
    drop(reservation);
    assert!(
        source
            .open(&setup.id, &setup.index, &Cancellation::default())
            .is_ok()
    );
}

fn until(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(Instant::now() < deadline, "native fixture timed out");
        std::thread::sleep(Duration::from_millis(1));
    }
}
fn usage_service(setup: &Setup) -> directory_ui_viewer_lib::scan::service::UsageService {
    use directory_ui_viewer_lib::{runtime::events::DiscardEvents, scan::service::UsageService};
    UsageService::new(
        Scope {
            session_id: "session".into(),
            generation: 1,
        },
        setup.id.clone(),
        setup.index.clone(),
        setup.scheduler.clone(),
        Arc::new(SystemClock::default()),
        Arc::new(DiscardEvents),
    )
    .unwrap()
}
#[test]
fn native_listing_and_usage_share_ids_in_both_discovery_orders_and_while_parked() {
    use directory_ui_viewer_lib::{
        domain::model::{Measure, Phase},
        platform::macos_usage::NativeUsageSource,
        scan::usage::ScanLimits,
    };
    use std::os::unix::fs::MetadataExt;
    let f = Fixture::new();
    for i in 0..450 {
        fs::create_dir(f.p(&format!("root/d{i:03}"))).unwrap();
        fs::write(f.p(&format!("root/d{i:03}/value")), b"value").unwrap();
    }
    for scan_first in [false, true] {
        let setup = Setup::new(&f, 300_000, 8);
        let usage = usage_service(&setup);
        let mut listing = None;
        let mut first_ids = vec![];
        if !scan_first {
            let handle = setup.start(&setup.id, Category::Directories, 0);
            until(|| setup.scheduler.counts().2 == 1);
            assert_eq!(setup.scheduler.handles().open(), 2);
            assert_eq!(
                setup.index.len(),
                401,
                "only demanded page and one lookahead"
            );
            let first = setup.service.page(&handle.task_id, None, 200, 0).unwrap();
            first_ids = first.entries;
            listing = Some(handle);
        }
        let task = usage
            .start(
                NativeUsageSource::root_request(),
                NativeUsageSource::new(setup.root.clone(), &setup.budgets.work).unwrap(),
                ScanLimits::default(),
            )
            .unwrap();
        until(|| usage.work(&task.task_id).unwrap().phase.terminal());
        let work = usage.work(&task.task_id).unwrap();
        assert_eq!(work.phase, Phase::Completed);
        assert!(work.issues.samples.is_empty());
        assert_eq!(setup.index.len(), 451);
        let handle = listing.unwrap_or_else(|| setup.start(&setup.id, Category::Directories, 0));
        let (rows, coverage) = setup.collect(&handle, 0);
        assert_eq!(coverage, Coverage::Complete);
        assert_eq!(rows.len(), 450);
        for first in first_ids {
            assert!(rows.iter().any(
                |row| row.entry_id == first.entry_id && row.display_name == first.display_name
            ));
        }
        let summaries = usage
            .read_usage(&rows.iter().map(|e| e.entry_id.clone()).collect::<Vec<_>>())
            .unwrap();
        for (row, summary) in rows.iter().zip(summaries) {
            assert_eq!(row.entry_id, summary.directory_id);
            let directory =
                fs::symlink_metadata(f.p(&format!("root/{}", row.display_name))).unwrap();
            let file =
                fs::symlink_metadata(f.p(&format!("root/{}/value", row.display_name))).unwrap();
            assert_eq!(
                summary.logical,
                Measure::Complete {
                    bytes: (directory.len() + file.len()).into()
                }
            );
            assert_eq!(
                summary.allocated,
                Measure::Complete {
                    bytes: ((directory.blocks() + file.blocks()) * 512).into()
                }
            );
        }
        usage.close();
        setup.service.close();
        until(|| setup.scheduler.handles().open() == 1);
        assert!(setup.scheduler.handles().peak() <= if scan_first { 3 } else { 4 });
    }
}
#[test]
fn parked_native_cursor_releases_on_cancel_ttl_and_close_without_returning_more_entries() {
    use directory_ui_viewer_lib::{domain::model::Phase, listing::cache::CacheError};
    let f = Fixture::new();
    for i in 0..601 {
        fs::write(f.p(&format!("root/f{i:04}")), b"x").unwrap();
    }
    for action in ["cancel", "ttl", "close"] {
        let setup = Setup::new(&f, 10, 2);
        let handle = setup.start(&setup.id, Category::Files, 0);
        until(|| setup.scheduler.counts().2 == 1);
        assert_eq!(setup.scheduler.handles().open(), 2);
        let page = setup.service.page(&handle.task_id, None, 200, 0).unwrap();
        let observed = setup
            .service
            .work(&handle.task_id, 0)
            .unwrap()
            .processed_entries;
        assert_eq!(page.entries.len(), 200);
        for _ in 0..5 {
            assert_eq!(
                setup
                    .service
                    .page(&handle.task_id, None, 200, 0)
                    .unwrap()
                    .entries,
                page.entries
            );
        }
        assert_eq!(
            setup
                .service
                .work(&handle.task_id, 0)
                .unwrap()
                .processed_entries,
            observed
        );
        match action {
            "cancel" => {
                setup.service.cancel(&handle.task_id).unwrap();
                until(|| {
                    setup
                        .service
                        .work(&handle.task_id, 0)
                        .unwrap()
                        .phase
                        .terminal()
                });
                assert_eq!(
                    setup.service.work(&handle.task_id, 0).unwrap().phase,
                    Phase::Cancelled
                );
                let (rows, coverage) = setup.collect(&handle, 0);
                assert_eq!(rows.len(), 400);
                assert_eq!(coverage, Coverage::Partial);
                assert_eq!(
                    setup
                        .service
                        .work(&handle.task_id, 0)
                        .unwrap()
                        .processed_entries,
                    observed
                );
            }
            "ttl" => {
                setup.service.expire(10);
                assert_eq!(
                    setup.service.work(&handle.task_id, 10),
                    Err(CacheError::TaskExpired)
                );
                assert_eq!(
                    setup
                        .service
                        .page(&handle.task_id, page.next_cursor.as_deref(), 200, 10),
                    Err(CacheError::CursorExpired)
                );
            }
            "close" => {
                setup.service.close();
                assert!(setup.service.page(&handle.task_id, None, 200, 0).is_err());
            }
            _ => unreachable!(),
        }
        until(|| setup.scheduler.handles().open() == 1);
        setup.service.close();
        assert_eq!(setup.budgets.listings.used(), 0);
        // Only the pinned approval remains; no cursor or source allocation leaks.
        assert_eq!(setup.budgets.work.used(), 2048);
    }
}
