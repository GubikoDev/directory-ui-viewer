#![cfg(target_os = "macos")]
use directory_ui_viewer_lib::{
    domain::{
        controller::{Cancellation, Scope},
        index::{DirectoryIndex, DirectoryKey, Origin, SharedDirectoryIndex},
        model::{Measure, Phase, Reason},
    },
    platform::{macos::ApprovedRoot, macos_usage::NativeUsageSource},
    runtime::{clock::SystemClock, events::DiscardEvents, memory::ManagedBudgets},
    scan::{
        budgeted_source::BudgetedSource,
        service::UsageService,
        usage::{ObservationSource, ScanLimits, ScanPhase, UsageScanner},
    },
    scheduler::{HandleBudget, Scheduler},
};
use std::{
    collections::BTreeSet,
    fs::{self, File},
    os::unix::fs::{MetadataExt, symlink},
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
        let p = std::env::temp_dir().join(format!(
            "duv-native-usage-{}-{}",
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
    fn approve(&self, handles: &HandleBudget, budgets: &ManagedBudgets) -> Arc<ApprovedRoot> {
        Arc::new(
            ApprovedRoot::open_picker_candidate(
                &self.p("root"),
                handles.clone(),
                budgets.work.clone(),
            )
            .unwrap(),
        )
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap()
    }
}
fn index(b: &ManagedBudgets) -> SharedDirectoryIndex {
    DirectoryIndex::with_budgets(
        "native".into(),
        b.background_index.clone(),
        b.foreground_index.clone(),
    )
    .into()
}
fn oracle(path: &Path) -> (u128, u128) {
    fn visit(p: &Path, seen: &mut BTreeSet<(u64, u64)>) -> (u128, u128) {
        let m = fs::symlink_metadata(p).unwrap();
        let mut n = (0, 0);
        if seen.insert((m.dev(), m.ino())) {
            n = (m.len() as u128, m.blocks() as u128 * 512)
        }
        if m.is_dir() {
            for e in fs::read_dir(p).unwrap() {
                let child = visit(&e.unwrap().path(), seen);
                n.0 += child.0;
                n.1 += child.1;
            }
        }
        n
    }
    visit(path, &mut BTreeSet::new())
}
fn exact(m: Measure) -> u128 {
    match m {
        Measure::Complete { bytes } => serde_json::to_value(bytes)
            .unwrap()
            .as_str()
            .unwrap()
            .parse()
            .unwrap(),
        other => panic!("not complete: {other:?}"),
    }
}
fn run_scan(
    root: Arc<ApprovedRoot>,
    handles: &HandleBudget,
    b: &ManagedBudgets,
    idx: SharedDirectoryIndex,
) -> UsageScanner<BudgetedSource<NativeUsageSource>> {
    let source = NativeUsageSource::new(root, &b.work).unwrap();
    let mut scan = UsageScanner::new(
        BudgetedSource::new(source, handles.clone()),
        NativeUsageSource::root_request(),
        idx,
        ScanLimits::default(),
        Cancellation::default(),
    )
    .unwrap();
    while scan.step(128, "2026-09-14T00:00:00Z") == ScanPhase::Running {}
    scan
}
#[test]
fn production_capability_source_matches_independent_sparse_link_and_subtree_oracle() {
    let f = Fixture::new();
    for d in ["a", "b"] {
        fs::create_dir(f.p(&format!("root/{d}"))).unwrap()
    }
    fs::write(f.p("root/a/shared"), b"shared bytes").unwrap();
    fs::hard_link(f.p("root/a/shared"), f.p("root/b/shared")).unwrap();
    fs::hard_link(f.p("root/a/shared"), f.p("root/a/again")).unwrap();
    File::create(f.p("root/.sparse"))
        .unwrap()
        .set_len(16 * 1024 * 1024)
        .unwrap();
    fs::write(f.p("outside-secret"), b"never counted").unwrap();
    symlink(f.p("outside-secret"), f.p("root/link")).unwrap();
    symlink(f.p("root"), f.p("root/cycle")).unwrap();
    // FIFO contents must never be opened: a content read would block without a writer.
    use std::os::unix::ffi::OsStrExt;
    let fifo = std::ffi::CString::new(f.p("root/pipe").as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    let b = ManagedBudgets::default();
    let handles = HandleBudget::new(320);
    let root = f.approve(&handles, &b);
    let idx = index(&b);
    let scan = run_scan(root.clone(), &handles, &b, idx.clone());
    let root_id = scan.root_id().to_owned();
    for (relative, id) in [
        ("root".to_string(), root_id.clone()),
        (
            "root/a".into(),
            idx.register(
                DirectoryKey {
                    parent_id: Some(root_id.clone()),
                    raw_name: b"a".to_vec(),
                },
                Origin::Background,
            )
            .unwrap(),
        ),
        (
            "root/b".into(),
            idx.register(
                DirectoryKey {
                    parent_id: Some(root_id),
                    raw_name: b"b".to_vec(),
                },
                Origin::Background,
            )
            .unwrap(),
        ),
    ] {
        let actual = idx.usage(&id).unwrap().unwrap();
        let expected = oracle(&f.p(&relative));
        assert_eq!((exact(actual.logical), exact(actual.allocated)), expected);
    }
    assert_eq!(scan.stats.processed_directories, 3);
    assert!(scan.issues.samples.is_empty());
    assert_eq!(
        handles.open(),
        1,
        "only original approval stays open after traversal"
    );
    drop(scan);
    drop(root);
    assert_eq!(handles.open(), 0);
    assert_eq!(b.work.used(), 0);
}
#[test]
fn refresh_rejects_replaced_root_without_scanning_replacement() {
    let f = Fixture::new();
    let b = ManagedBudgets::default();
    let handles = HandleBudget::new(5);
    let root = f.approve(&handles, &b);
    fs::rename(f.p("root"), f.p("original")).unwrap();
    fs::create_dir(f.p("root")).unwrap();
    fs::write(f.p("root/wrong"), vec![0; 100_000]).unwrap();
    fs::write(f.p("original/new"), b"fresh").unwrap();
    let scan = run_scan(root.clone(), &handles, &b, index(&b));
    let actual = scan.index.usage(scan.root_id()).unwrap().unwrap();
    assert!(scan.root_failed());
    assert_eq!(scan.stats.processed_entries, 0);
    assert!(matches!(actual.logical, Measure::Partial { .. }));
    assert_eq!(scan.issues.samples[0].code, "ENTRY_CHANGED");
}
#[test]
fn fixed_worker_service_uses_the_same_fd_pool_without_double_reserving() {
    let f = Fixture::new();
    fs::write(f.p("root/file"), b"abc").unwrap();
    let b = ManagedBudgets::default();
    let scheduler = Arc::new(Scheduler::new(64, 64, 2));
    let handles = scheduler.handles();
    let root = f.approve(&handles, &b);
    let idx = index(&b);
    let id = idx
        .register(
            DirectoryKey {
                parent_id: None,
                raw_name: vec![],
            },
            Origin::Background,
        )
        .unwrap();
    let service = UsageService::new(
        Scope {
            session_id: "native-session".into(),
            generation: 1,
        },
        id.clone(),
        idx,
        scheduler.clone(),
        Arc::new(SystemClock::default()),
        Arc::new(DiscardEvents),
    )
    .unwrap();
    let started = service
        .start(
            NativeUsageSource::root_request(),
            NativeUsageSource::new(root.clone(), &b.work).unwrap(),
            ScanLimits::default(),
        )
        .unwrap();
    let timeout = Instant::now() + Duration::from_secs(5);
    loop {
        let work = service.work(&started.task_id).unwrap();
        if work.phase.terminal() {
            assert_eq!(work.phase, Phase::Completed);
            assert!(work.issues.samples.is_empty());
            break;
        }
        assert!(Instant::now() < timeout);
        std::thread::sleep(Duration::from_millis(1));
    }
    let result = service.read_usage(&[id]).unwrap();
    assert_eq!(exact(result[0].logical.clone()), oracle(&f.p("root")).0);
    assert_eq!(handles.peak(), 2);
}
#[test]
fn different_fd_pool_is_rejected_and_depth_is_bounded() {
    let f = Fixture::new();
    let b = ManagedBudgets::default();
    let handles = HandleBudget::new(320);
    let root = f.approve(&handles, &b);
    assert!(
        matches!(NativeUsageSource::new(root.clone(), &ManagedBudgets::default().work), Err(e) if e.code == "INVALID_ARGUMENT")
    );
    let mut wrong = BudgetedSource::new(
        NativeUsageSource::new(root.clone(), &b.work).unwrap(),
        HandleBudget::new(320),
    );
    assert_eq!(
        wrong
            .refresh_root(&NativeUsageSource::root_request())
            .unwrap_err()
            .code,
        "INVALID_ARGUMENT"
    );
    drop(wrong);
    let mut p = f.p("root");
    for _ in 0..256 {
        p = p.join("d");
        fs::create_dir(&p).unwrap();
    }
    let scan = run_scan(root.clone(), &handles, &b, index(&b));
    assert_eq!(scan.stats.peak_open_directories, 128);
    assert!(handles.peak() <= 129);
    let summary = scan.index.usage(scan.root_id()).unwrap().unwrap();
    assert!(
        matches!(summary.logical,Measure::Partial{reasons,..} if reasons.contains(&Reason::ResourceLimit))
    );
    drop(scan);
    drop(root);
    assert_eq!(handles.open(), 0);
    assert_eq!(b.work.used(), 0);
}

#[test]
#[ignore = "Process-wide FD measurement; run alone with --ignored --test-threads=1"]
fn native_os_fd_count_includes_walkdir_handles_and_returns_to_baseline() {
    fn fd_count() -> usize {
        let mut entries: Vec<libc::proc_fdinfo> =
            (0..1024).map(|_| unsafe { std::mem::zeroed() }).collect();
        let capacity = std::mem::size_of_val(entries.as_slice());
        let bytes = unsafe {
            libc::proc_pidinfo(
                std::process::id() as i32,
                libc::PROC_PIDLISTFDS,
                0,
                entries.as_mut_ptr().cast(),
                capacity as i32,
            )
        };
        assert!(bytes > 0 && (bytes as usize) < capacity);
        assert_eq!(bytes as usize % std::mem::size_of::<libc::proc_fdinfo>(), 0);
        bytes as usize / std::mem::size_of::<libc::proc_fdinfo>()
    }
    let f = Fixture::new();
    let mut p = f.p("root");
    for _ in 0..40 {
        p = p.join("d");
        fs::create_dir(&p).unwrap();
    }
    let baseline = fd_count();
    let b = ManagedBudgets::default();
    let handles = HandleBudget::new(320);
    let root = f.approve(&handles, &b);
    assert_eq!(fd_count(), baseline + 1);
    let source = NativeUsageSource::new(root.clone(), &b.work).unwrap();
    let mut scan = UsageScanner::new(
        BudgetedSource::new(source, handles.clone()),
        NativeUsageSource::root_request(),
        index(&b),
        ScanLimits::default(),
        Cancellation::default(),
    )
    .unwrap();
    let mut peak = fd_count();
    loop {
        let phase = scan.step(1, "2026-09-14T00:00:00Z");
        let actual = fd_count();
        // Each active native directory may also own one walkdir ReadDir.
        assert!(actual >= baseline + handles.open());
        assert!(actual <= baseline + 2 * handles.open());
        peak = peak.max(actual);
        if phase != ScanPhase::Running {
            break;
        }
    }
    assert!(peak - baseline >= 42 && peak - baseline <= 84);
    assert_eq!(handles.peak(), 42);
    drop(scan);
    drop(root);
    assert_eq!(fd_count(), baseline);
    assert_eq!(b.work.used(), 0);
    println!(
        "OS FD evidence: baseline {baseline}, peak delta {}, final delta 0; charged peak {}",
        peak - baseline,
        handles.peak()
    );
}

#[test]
fn actual_child_permission_failure_keeps_healthy_sibling_bytes_and_native_code() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    fs::create_dir(f.p("root/denied")).unwrap();
    fs::write(f.p("root/denied/secret"), vec![0; 10_000]).unwrap();
    fs::write(f.p("root/healthy"), b"healthy bytes").unwrap();
    struct Restore(PathBuf);
    impl Drop for Restore {
        fn drop(&mut self) {
            fs::set_permissions(&self.0, fs::Permissions::from_mode(0o700)).unwrap();
        }
    }
    let restore = Restore(f.p("root/denied"));
    fs::set_permissions(&restore.0, fs::Permissions::from_mode(0)).unwrap();
    let b = ManagedBudgets::default();
    let handles = HandleBudget::new(320);
    let root = f.approve(&handles, &b);
    let scan = run_scan(root, &handles, &b, index(&b));
    drop(restore);
    let expected = ["root", "root/denied", "root/healthy"]
        .into_iter()
        .map(|p| fs::symlink_metadata(f.p(p)).unwrap().len() as u128)
        .sum::<u128>();
    let actual = scan.index.usage(scan.root_id()).unwrap().unwrap();
    assert!(
        matches!(actual.logical,Measure::Partial{observed_bytes,reasons} if observed_bytes == expected.into() && reasons.contains(&Reason::ReadError))
    );
    assert!(
        scan.issues
            .samples
            .iter()
            .any(|i| i.code == "PERMISSION_DENIED" && i.native_code == Some(libc::EACCES))
    );
}

#[test]
fn an_existing_directory_id_cannot_be_rebound_to_a_replacement_object() {
    let f = Fixture::new();
    fs::create_dir(f.p("root/child")).unwrap();
    let b = ManagedBudgets::default();
    let handles = HandleBudget::new(320);
    let root = f.approve(&handles, &b);
    let idx = index(&b);
    let root_id = idx
        .register(
            DirectoryKey {
                parent_id: None,
                raw_name: vec![],
            },
            Origin::Background,
        )
        .unwrap();
    let child_id = idx
        .register(
            DirectoryKey {
                parent_id: Some(root_id),
                raw_name: b"child".to_vec(),
            },
            Origin::Foreground,
        )
        .unwrap();
    let old = root
        .directory()
        .observe(b"child")
        .unwrap()
        .identity()
        .fingerprint();
    idx.bind_identity(&child_id, old).unwrap();
    let before = b.foreground_index.used();
    idx.bind_identity(&child_id, old).unwrap();
    assert_eq!(b.foreground_index.used(), before);
    fs::rename(f.p("root/child"), f.p("old")).unwrap();
    fs::create_dir(f.p("root/child")).unwrap();
    fs::write(f.p("root/child/new-object-data"), vec![0; 10_000]).unwrap();
    let scan = run_scan(root, &handles, &b, idx.clone());
    assert_eq!(idx.identity(&child_id).unwrap(), Some(old));
    assert!(idx.usage(&child_id).unwrap().is_none());
    let summary = idx.usage(scan.root_id()).unwrap().unwrap();
    assert!(
        matches!(summary.logical,Measure::Partial{reasons,..} if reasons.contains(&Reason::ChangedDuringScan))
    );
    assert!(
        scan.issues
            .samples
            .iter()
            .any(|i| i.code == "ENTRY_CHANGED")
    );
    assert_eq!(
        scan.stats.processed_entries, 1,
        "replacement contents were not traversed"
    );
}
#[test]
fn mismatched_root_index_binding_fails_before_root_cursor_is_opened() {
    use directory_ui_viewer_lib::domain::index::DirectoryFingerprint;
    let f = Fixture::new();
    fs::write(f.p("root/data"), b"do not traverse").unwrap();
    let b = ManagedBudgets::default();
    let handles = HandleBudget::new(320);
    let root = f.approve(&handles, &b);
    let idx = index(&b);
    let id = idx
        .register(
            DirectoryKey {
                parent_id: None,
                raw_name: vec![],
            },
            Origin::Background,
        )
        .unwrap();
    idx.bind_identity(&id, DirectoryFingerprint([0; 5]))
        .unwrap();
    let scan = run_scan(root, &handles, &b, idx);
    assert!(scan.root_failed());
    assert_eq!(scan.stats.processed_entries, 0);
    assert_eq!(handles.peak(), 1);
    assert_eq!(scan.issues.samples[0].code, "ENTRY_CHANGED");
    assert_eq!(
        scan.issues.samples[0].scope,
        directory_ui_viewer_lib::domain::model::IssueScope::Root
    );
}
