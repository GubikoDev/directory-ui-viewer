//! Isolated single-thread allocation probes for production data structures.
//! Counts Rust System allocations and malloc_size rounding, not RSS, allocator
//! metadata, foreign allocations, or allocations hidden inside realloc itself.

#[cfg(target_os = "macos")]
mod mac {
    use directory_ui_viewer_lib::{
        domain::{
            controller::Cancellation,
            index::{DirectoryIndex, DirectoryKey, IndexError, Origin},
            model::{
                Category, DirectoryUsage, Entry, Field, FollowPolicy, FsIssue, Issues, Kind,
                Measure, Operation, Phase, ScanState, SpecialType, WorkRecord,
            },
        },
        listing::cache::ListingCache,
        runtime::memory::ManagedBudgets,
        scan::usage::{
            FileIdentity, Metric, Observation, ObservationSource, ScanLimits, UsageScanner,
        },
    };
    use serde::Serialize;
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

    static REQUESTED: AtomicUsize = AtomicUsize::new(0);
    static USABLE: AtomicUsize = AtomicUsize::new(0);
    static PEAK: AtomicUsize = AtomicUsize::new(0);
    pub struct Counted;
    fn change(
        requested_before: usize,
        requested_after: usize,
        usable_before: usize,
        usable_after: usize,
    ) {
        REQUESTED.fetch_add(requested_after, SeqCst);
        REQUESTED.fetch_sub(requested_before, SeqCst);
        // Sequential probes only. A replacement is counted after System.realloc
        // returns; temporary internal allocations during that call are excluded.
        let current = if usable_after >= usable_before {
            let delta = usable_after - usable_before;
            USABLE.fetch_add(delta, SeqCst) + delta
        } else {
            let delta = usable_before - usable_after;
            USABLE.fetch_sub(delta, SeqCst) - delta
        };
        PEAK.fetch_max(current, SeqCst);
    }
    unsafe impl GlobalAlloc for Counted {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            let ptr = unsafe { System.alloc(layout) };
            if !ptr.is_null() {
                change(0, layout.size(), 0, unsafe {
                    libc::malloc_size(ptr.cast())
                });
            }
            ptr
        }
        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            let ptr = unsafe { System.alloc_zeroed(layout) };
            if !ptr.is_null() {
                change(0, layout.size(), 0, unsafe {
                    libc::malloc_size(ptr.cast())
                });
            }
            ptr
        }
        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            let usable = unsafe { libc::malloc_size(ptr.cast()) };
            unsafe { System.dealloc(ptr, layout) };
            change(layout.size(), 0, usable, 0);
        }
        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
            let old = unsafe { libc::malloc_size(ptr.cast()) };
            let next = unsafe { System.realloc(ptr, layout, size) };
            if !next.is_null() {
                change(layout.size(), size, old, unsafe {
                    libc::malloc_size(next.cast())
                });
            }
            next
        }
    }
    #[derive(Clone, Copy)]
    struct Baseline {
        requested: usize,
        usable: usize,
    }
    impl Baseline {
        fn begin() -> Self {
            let usable = USABLE.load(SeqCst);
            PEAK.store(usable, SeqCst);
            Self {
                requested: REQUESTED.load(SeqCst),
                usable,
            }
        }
        fn report(self, case: &'static str, items: usize, managed: usize, limited: bool) -> Report {
            Report {
                case,
                items,
                managed_delta: managed,
                limited,
                fully_exposed_items: None,
                requested_live_delta: REQUESTED.load(SeqCst) as i128 - self.requested as i128,
                malloc_usable_live_delta: USABLE.load(SeqCst) as i128 - self.usable as i128,
                malloc_usable_peak_delta_observed: PEAK.load(SeqCst) - self.usable,
            }
        }
    }
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Report {
        case: &'static str,
        items: usize,
        limited: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        fully_exposed_items: Option<usize>,
        requested_live_delta: i128,
        malloc_usable_live_delta: i128,
        malloc_usable_peak_delta_observed: usize,
        managed_delta: usize,
    }
    fn known<T>(value: T) -> Field<T> {
        Field::Known { value }
    }
    const TIME: &str = "2026-09-14T00:00:00Z";
    fn index(case: &'static str, count: usize, name_bytes: usize) {
        let before_all = USABLE.load(SeqCst);
        let report = {
            let budgets = ManagedBudgets::default();
            let mut index = DirectoryIndex::with_budgets(
                "cost".into(),
                budgets.background_index.clone(),
                budgets.foreground_index.clone(),
            );
            let root = index
                .register(
                    DirectoryKey {
                        parent_id: None,
                        raw_name: vec![],
                    },
                    Origin::Foreground,
                )
                .unwrap();
            let reserved_before = budgets.used();
            let baseline = Baseline::begin();
            let mut admitted = 0;
            let mut limited = false;
            for item in 0..count {
                let mut name = format!("d-{item:08}");
                name.extend(std::iter::repeat_n('x', name_bytes - name.len()));
                let id = match index.register(
                    DirectoryKey {
                        parent_id: Some(root.clone()),
                        raw_name: name.as_bytes().to_vec(),
                    },
                    Origin::Background,
                ) {
                    Ok(id) => id,
                    Err(IndexError::ResourceLimit) => {
                        limited = true;
                        break;
                    }
                    Err(other) => panic!("unexpected index registration failure: {other:?}"),
                };
                let entry = Entry {
                    entry_id: id.clone(),
                    parent_id: Some(root.clone()),
                    display_name: name,
                    kind: Kind::Directory,
                    hidden: known(false),
                    special_type: known(SpecialType::None),
                    modified_at: known(TIME.into()),
                    own_logical_bytes: known(64_u64.into()),
                    own_allocated_bytes: known(4096_u64.into()),
                    observed_at: TIME.into(),
                    follow_policy: FollowPolicy::Never,
                };
                match index.expose(&id, entry) {
                    Ok(()) => {}
                    Err(IndexError::ResourceLimit) => {
                        limited = true;
                        break;
                    }
                    Err(other) => panic!("unexpected index exposure failure: {other:?}"),
                }
                index
                    .set_usage(DirectoryUsage {
                        directory_id: id,
                        usage_revision: 1,
                        scan_state: ScanState::Settled,
                        logical: Measure::Complete {
                            bytes: u128::MAX.into(),
                        },
                        allocated: Measure::Complete {
                            bytes: u128::MAX.into(),
                        },
                        observed_at: TIME.into(),
                    })
                    .unwrap();
                admitted += 1;
            }
            let mut report = baseline.report(
                case,
                index.len() - 1,
                budgets.used() - reserved_before,
                limited,
            );
            report.fully_exposed_items = Some(admitted);
            report
        };
        assert_eq!(
            USABLE.load(SeqCst),
            before_all,
            "index probe retained an allocation"
        );
        assert!(report.malloc_usable_live_delta <= report.managed_delta as i128);
        println!("{}", serde_json::to_string(&report).unwrap());
    }
    struct Links {
        unique: usize,
    }
    impl ObservationSource for Links {
        type Cursor = usize;
        fn open(&mut self, _: &Observation) -> Result<usize, FsIssue> {
            Ok(0)
        }
        fn next(&mut self, cursor: &mut usize) -> Result<Option<Observation>, FsIssue> {
            if *cursor == self.unique * 2 {
                return Ok(None);
            }
            let identity = *cursor % self.unique;
            *cursor += 1;
            Ok(Some(Observation {
                node: *cursor as u64 + 1,
                raw_name: format!("file-{}", *cursor).into_bytes(),
                kind: Kind::RegularFile,
                identity: Some(FileIdentity {
                    device: 1,
                    inode: identity as u64 + 2,
                }),
                link_count: 2,
                mount: 1,
                logical: Metric::Known(16),
                allocated: Metric::Known(4096),
            }))
        }
    }
    fn hardlinks(unique: usize) {
        let before_all = USABLE.load(SeqCst);
        let reports = {
            let budgets = ManagedBudgets::default();
            let index = DirectoryIndex::with_budgets(
                "cost".into(),
                budgets.background_index.clone(),
                budgets.foreground_index.clone(),
            );
            let root = Observation {
                node: 0,
                raw_name: vec![],
                kind: Kind::Directory,
                identity: Some(FileIdentity {
                    device: 1,
                    inode: 1,
                }),
                link_count: 1,
                mount: 1,
                logical: Metric::Known(64),
                allocated: Metric::Known(0),
            };
            let mut scan = UsageScanner::with_budgets(
                Links { unique },
                root,
                index,
                ScanLimits::default(),
                Cancellation::default(),
                &budgets,
            )
            .unwrap();
            let baseline = Baseline::begin();
            let reserved_before = budgets.hardlinks.used();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            // step also has a time quantum, so advance until all first copies
            // are consumed. Do not finalize/drop the live root aggregate yet.
            while scan.stats.processed_entries < unique as u64 {
                assert!(
                    std::time::Instant::now() < deadline,
                    "hardlink probe did not progress"
                );
                let remaining = unique as u64 - scan.stats.processed_entries;
                scan.step(remaining as usize, TIME);
            }
            let first = baseline.report(
                "hardlink-first-copies",
                unique,
                budgets.hardlinks.used() - reserved_before,
                false,
            );
            let baseline = Baseline::begin();
            let reserved_before = budgets.hardlinks.used();
            while scan.stats.processed_entries < (unique * 2) as u64 {
                assert!(
                    std::time::Instant::now() < deadline,
                    "hardlink probe did not progress"
                );
                let remaining = (unique * 2) as u64 - scan.stats.processed_entries;
                scan.step(remaining as usize, TIME);
            }
            let duplicates = baseline.report(
                "hardlink-second-copies",
                unique,
                budgets.hardlinks.used() - reserved_before,
                false,
            );
            (first, duplicates)
        };
        assert_eq!(
            USABLE.load(SeqCst),
            before_all,
            "hardlink probe retained an allocation"
        );
        assert!(reports.0.malloc_usable_live_delta <= reports.0.managed_delta as i128);
        assert_eq!(reports.1.managed_delta, 0);
        println!("{}", serde_json::to_string(&reports.0).unwrap());
        println!("{}", serde_json::to_string(&reports.1).unwrap());
    }
    pub fn run() {
        println!(
            "{{\"layer\":\"isolated Rust allocations\",\"rss\":false,\"entryLayoutBytes\":{},\"directoryUsageLayoutBytes\":{},\"fileIdentityLayoutBytes\":{}}}",
            std::mem::size_of::<Entry>(),
            std::mem::size_of::<DirectoryUsage>(),
            std::mem::size_of::<FileIdentity>()
        );
        calibrate();
        index("index-200-name64", 200, 64);
        index("index-200-name1023", 200, 1023);
        index("index-10000-name64", 10_000, 64);
        hardlinks(5000);
        listing();
        frames("fixture-frames32-with-index", 32);
        frames("fixture-frames64-with-index", 64);
        frames("fixture-frames127-with-index", 127);
    }

    struct Chain;
    impl ObservationSource for Chain {
        type Cursor = (u64, bool);
        fn open(&mut self, directory: &Observation) -> Result<Self::Cursor, FsIssue> {
            Ok((directory.node, false))
        }
        fn next(&mut self, cursor: &mut Self::Cursor) -> Result<Option<Observation>, FsIssue> {
            if cursor.1 || cursor.0 == 256 {
                return Ok(None);
            }
            cursor.1 = true;
            Ok(Some(directory(cursor.0 + 1)))
        }
    }
    fn directory(node: u64) -> Observation {
        Observation {
            node,
            raw_name: if node == 0 { vec![] } else { b"d".to_vec() },
            kind: Kind::Directory,
            identity: Some(FileIdentity {
                device: 1,
                inode: node + 1,
            }),
            link_count: 1,
            mount: 1,
            logical: Metric::Known(64),
            allocated: Metric::Known(0),
        }
    }
    fn frames(case: &'static str, depth: usize) {
        let before_all = USABLE.load(SeqCst);
        let report = {
            let budgets = ManagedBudgets::default();
            let index = DirectoryIndex::with_budgets(
                "cost".into(),
                budgets.background_index.clone(),
                budgets.foreground_index.clone(),
            );
            let mut scan = UsageScanner::with_budgets(
                Chain,
                directory(0),
                index,
                ScanLimits::default(),
                Cancellation::default(),
                &budgets,
            )
            .unwrap();
            let baseline = Baseline::begin();
            let reserved_before = budgets.used();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            while scan.stats.processed_entries < depth as u64 {
                assert!(
                    std::time::Instant::now() < deadline,
                    "frame probe did not progress"
                );
                scan.step(depth - scan.stats.processed_entries as usize, TIME);
            }
            assert_eq!(scan.stats.peak_open_directories, depth + 1);
            baseline.report(case, depth, budgets.used() - reserved_before, false)
        };
        assert_eq!(
            USABLE.load(SeqCst),
            before_all,
            "frame probe retained an allocation"
        );
        assert!(report.malloc_usable_live_delta <= report.managed_delta as i128);
        println!("{}", serde_json::to_string(&report).unwrap());
    }

    fn listing() {
        let before_all = USABLE.load(SeqCst);
        let reports = {
            let budgets = ManagedBudgets::default();
            let mut cache = ListingCache::with_budget(budgets.listings.clone(), 300_000, 1_048_576);
            let task = WorkRecord {
                session_id: "session".into(),
                generation: 1,
                task_id: "task".into(),
                operation: Operation::ListFiles,
                target_id: "root".into(),
                phase: Phase::Running,
                wait_reason: None,
                sequence: 0,
                processed_entries: 0_u64.into(),
                processed_directories: 0_u64.into(),
                observed_at: TIME.into(),
                issues: Issues::default(),
            };
            let handle = cache.start(task, Category::Files, 0).unwrap();
            let baseline = Baseline::begin();
            let reserved_before = budgets.listings.used();
            for i in 0..200 {
                let mut name = format!("file-{i:08}");
                name.extend(std::iter::repeat_n('x', 255 - name.len()));
                assert!(
                    cache
                        .append(
                            &handle.task_id,
                            Entry {
                                entry_id: format!("entry-{i}"),
                                parent_id: Some("root".into()),
                                display_name: name,
                                kind: Kind::RegularFile,
                                hidden: known(false),
                                special_type: known(SpecialType::None),
                                modified_at: known(TIME.into()),
                                own_logical_bytes: known(16_u64.into()),
                                own_allocated_bytes: known(4096_u64.into()),
                                observed_at: TIME.into(),
                                follow_policy: FollowPolicy::Never,
                            }
                        )
                        .unwrap()
                );
            }
            cache
                .finish(&handle.task_id, Phase::Completed, None)
                .unwrap();
            let retained = baseline.report(
                "listing-200-name255",
                200,
                budgets.listings.used() - reserved_before,
                false,
            );
            let baseline = Baseline::begin();
            let reserved_before = budgets.used();
            let page = cache
                .charged_page(&handle.task_id, None, 200, 1, &budgets.work)
                .unwrap();
            assert_eq!(page.entries.len(), 200);
            let response = baseline.report(
                "typed-page-200-name255",
                200,
                budgets.used() - reserved_before,
                false,
            );
            drop(page);
            assert_eq!(budgets.used(), reserved_before);
            (retained, response)
        };
        assert_eq!(
            USABLE.load(SeqCst),
            before_all,
            "listing probe retained an allocation"
        );
        for report in [reports.0, reports.1] {
            assert!(report.malloc_usable_live_delta <= report.managed_delta as i128);
            println!("{}", serde_json::to_string(&report).unwrap());
        }
    }

    fn calibrate() {
        let baseline = Baseline::begin();
        let mut value = Vec::<u8>::with_capacity(1);
        value.push(7);
        let check = |value: &Vec<u8>| {
            assert_eq!(
                REQUESTED.load(SeqCst) - baseline.requested,
                value.capacity()
            );
            assert_eq!(USABLE.load(SeqCst) - baseline.usable, unsafe {
                libc::malloc_size(value.as_ptr().cast())
            });
        };
        check(&value);
        value.reserve_exact(4095);
        check(&value);
        value.shrink_to_fit();
        check(&value);
        drop(value);
        let zeroed = vec![0_u8; 4096];
        check(&zeroed);
        drop(zeroed);
        assert_eq!(REQUESTED.load(SeqCst), baseline.requested);
        assert_eq!(USABLE.load(SeqCst), baseline.usable);
    }
}
#[cfg(target_os = "macos")]
#[global_allocator]
static ALLOCATOR: mac::Counted = mac::Counted;
#[cfg(target_os = "macos")]
fn main() {
    mac::run();
}
#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("managed_costs requires macOS");
    std::process::exit(1);
}
