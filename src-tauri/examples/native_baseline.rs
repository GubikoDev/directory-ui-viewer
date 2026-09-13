//! Own-fixture engine measurements. Excludes Tauri IPC/WebView and never accepts a user path.
//! Run with --release and --trials=10 (the default), optionally a fixed profile:
//! flat10k, flat100k, wide, deep, hardlinks. JSONL stdout contains aggregate data only.
//! RSS/FD are sampled maxima, not allocator high-water marks. OS cache is uncontrolled.
#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("macOS only");
}
#[cfg(target_os = "macos")]
fn main() {
    macos::run();
}
#[cfg(target_os = "macos")]
mod macos {
    use directory_ui_viewer_lib::{
        domain::{
            controller::{NativeContext, Scope},
            model::{Category, Measure, Phase, Reason, RootData},
        },
        platform::macos_runtime::{NativeRuntime, RuntimeResources},
        runtime::{clock::SystemClock, events::DiscardEvents, memory::ManagedBudgets},
        scan::service::CancelStatus,
        scheduler::Scheduler,
    };
    use serde::Serialize;
    use std::{
        collections::BTreeSet,
        fs,
        os::unix::fs::MetadataExt,
        path::{Path, PathBuf},
        sync::Arc,
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };
    struct Fixture {
        path: PathBuf,
        entries: u64,
        category: Category,
        eligible: bool,
        partial: bool,
        oracle: (u128, u128),
    }
    impl Fixture {
        fn new(profile: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "duv-benchmark-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir(&path).unwrap();
            let mut fixture = Self {
                path,
                entries: 0,
                category: Category::Files,
                eligible: true,
                partial: false,
                oracle: (0, 0),
            };
            match profile {
                "flat10k" | "flat100k" => {
                    let count = if profile == "flat10k" {
                        10_000
                    } else {
                        100_000
                    };
                    for n in 0..count {
                        fs::File::create(fixture.path.join(format!("f-{n:06}"))).unwrap();
                    }
                    fixture.entries = count;
                }
                "wide" => {
                    fixture.category = Category::Directories;
                    for d in 0..1000 {
                        let dir = fixture.path.join(format!("d-{d:04}"));
                        fs::create_dir(&dir).unwrap();
                        for n in 0..100 {
                            fs::File::create(dir.join(format!("f-{n:03}"))).unwrap();
                        }
                    }
                    fixture.entries = 101_000;
                }
                "deep" => {
                    fixture.category = Category::Directories;
                    fixture.eligible = false;
                    fixture.partial = true;
                    let mut dir = fixture.path.clone();
                    for _ in 0..256 {
                        dir = dir.join("d");
                        fs::create_dir(&dir).unwrap();
                    }
                    fixture.entries = 256;
                }
                "hardlinks" => {
                    fixture.category = Category::Directories;
                    for d in 0..200 {
                        fs::create_dir(fixture.path.join(format!("d-{d:03}"))).unwrap();
                    }
                    for n in 0..5000 {
                        let a = fixture.path.join(format!("d-{:03}/f-{n:04}", n / 50));
                        let b = fixture.path.join(format!("d-{:03}/f-{n:04}", n / 50 + 100));
                        fs::write(&a, b"benchmark shared").unwrap();
                        fs::hard_link(a, b).unwrap();
                    }
                    fixture.entries = 10_200;
                }
                _ => panic!("unknown fixed profile"),
            }
            fixture.oracle = oracle(&fixture.path, &mut BTreeSet::new());
            fixture
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.path).expect("remove owned benchmark fixture");
        }
    }
    fn oracle(path: &Path, visited: &mut BTreeSet<(u64, u64)>) -> (u128, u128) {
        let m = fs::symlink_metadata(path).unwrap();
        let mut total = if visited.insert((m.dev(), m.ino())) {
            (m.size() as u128, m.blocks() as u128 * 512)
        } else {
            (0, 0)
        };
        if m.is_dir() {
            for e in fs::read_dir(path).unwrap() {
                let child = oracle(&e.unwrap().path(), visited);
                total.0 += child.0;
                total.1 += child.1;
            }
        }
        total
    }
    fn context() -> NativeContext {
        NativeContext {
            window: "main".into(),
            document: 1,
        }
    }
    fn scope(root: &RootData) -> Scope {
        Scope {
            session_id: root.session.session_id.clone(),
            generation: root.session.generation,
        }
    }
    fn rss() -> u64 {
        let mut info: libc::rusage_info_v2 = unsafe { std::mem::zeroed() };
        assert_eq!(
            unsafe {
                libc::proc_pid_rusage(
                    std::process::id() as i32,
                    libc::RUSAGE_INFO_V2,
                    (&mut info as *mut libc::rusage_info_v2).cast(),
                )
            },
            0
        );
        info.ri_resident_size
    }
    fn fd_count() -> usize {
        let mut entries: [libc::proc_fdinfo; 1024] = unsafe { std::mem::zeroed() };
        let capacity = std::mem::size_of_val(&entries);
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
        bytes as usize / std::mem::size_of::<libc::proc_fdinfo>()
    }
    #[derive(Default, Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Samples {
        managed_peak: usize,
        work_peak: usize,
        hardlink_peak: usize,
        resident_max_sampled: u64,
        os_fd_max_sampled: usize,
        managed_fd_max_sampled: usize,
    }
    impl Samples {
        fn observe(&mut self, resources: &RuntimeResources) {
            self.managed_peak = self.managed_peak.max(resources.budgets.used());
            self.work_peak = self.work_peak.max(resources.budgets.work.used());
            self.hardlink_peak = self.hardlink_peak.max(resources.budgets.hardlinks.used());
            self.resident_max_sampled = self.resident_max_sampled.max(rss());
            self.os_fd_max_sampled = self.os_fd_max_sampled.max(fd_count());
            self.managed_fd_max_sampled = self
                .managed_fd_max_sampled
                .max(resources.scheduler.handles().open());
        }
    }
    fn wait(
        stage: &str,
        resources: &RuntimeResources,
        samples: &mut Samples,
        mut ready: impl FnMut() -> bool,
    ) {
        eprintln!("stage: {stage}");
        let deadline = Instant::now() + Duration::from_secs(120);
        while !ready() {
            assert!(Instant::now() < deadline, "fixture timed out at {stage}");
            samples.observe(resources);
            std::thread::sleep(Duration::from_micros(200));
        }
        samples.observe(resources);
    }
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Trial {
        profile: String,
        trial: usize,
        layer: &'static str,
        cache_condition: &'static str,
        entries: u64,
        first_page_eligible: bool,
        first_page_ms: f64,
        first_200_entries_ms: f64,
        full_scan_ms: f64,
        cancel_accept_ms: f64,
        cancel_was_running: bool,
        cancel_accepted: bool,
        samples: Samples,
        logical: Measure,
        allocated: Measure,
    }
    fn trial(profile: &str, n: usize, fixture: &Fixture) -> Trial {
        let resources = RuntimeResources {
            scheduler: Arc::new(Scheduler::default()),
            budgets: ManagedBudgets::default(),
            clock: Arc::new(SystemClock::default()),
            events: Arc::new(DiscardEvents),
        };
        let runtime = Arc::new(NativeRuntime::new(context(), resources.clone()).unwrap());
        let baseline_bytes = resources.budgets.used();
        let baseline_fd = fd_count();
        let epoch = runtime.connect(&context(), "benchmark").unwrap();
        let ticket = runtime.begin_selection(&context(), epoch).unwrap();
        let root = tauri::async_runtime::block_on(
            runtime
                .clone()
                .select_path(ticket, Some(fixture.path.clone())),
        )
        .unwrap()
        .unwrap();
        let mut current = scope(&root);
        let mut samples = Samples::default();
        let probe = runtime.start_usage(&context(), epoch, &current).unwrap();
        wait("cancel probe start", &resources, &mut samples, || {
            runtime
                .read_task(&context(), epoch, &current, &probe.task_id)
                .unwrap()
                .phase
                != Phase::Queued
        });
        let before = runtime
            .read_task(&context(), epoch, &current, &probe.task_id)
            .unwrap();
        let start = Instant::now();
        let (cancel, _) = runtime
            .cancel_task(&context(), epoch, &current, &probe.task_id)
            .unwrap();
        let cancel_ms = start.elapsed().as_secs_f64() * 1000.;
        wait("cancel terminal", &resources, &mut samples, || {
            runtime
                .read_task(&context(), epoch, &current, &probe.task_id)
                .unwrap()
                .phase
                .terminal()
        });
        let root =
            tauri::async_runtime::block_on(runtime.clone().refresh(context(), epoch, current))
                .unwrap();
        current = scope(&root);
        let started = Instant::now();
        let (listing, _) = runtime
            .start_listing(
                &context(),
                epoch,
                &current,
                &root.root.entry_id,
                fixture.category,
            )
            .unwrap();
        let usage = runtime.start_usage(&context(), epoch, &current).unwrap();
        let mut cursor = None;
        let mut first_entries = 0;
        let mut first_page_ms = None;
        wait("first page", &resources, &mut samples, || {
            let page = runtime
                .read_listing(
                    &context(),
                    epoch,
                    &current,
                    &listing.task_id,
                    cursor.as_deref(),
                    200,
                )
                .unwrap();
            if !page.entries.is_empty() && first_page_ms.is_none() {
                first_page_ms = Some(started.elapsed().as_secs_f64() * 1000.);
            }
            first_entries += page.entries.len();
            cursor = page.next_cursor;
            if page.work.phase.terminal() && fixture.eligible && first_entries < 200 {
                panic!(
                    "insufficient eligible entries: {first_entries}, {:?}",
                    page.work
                );
            }
            first_entries >= 200 || (!fixture.eligible && page.work.phase.terminal())
        });
        let first_ms = started.elapsed().as_secs_f64() * 1000.;
        wait("usage terminal", &resources, &mut samples, || {
            runtime
                .read_task(&context(), epoch, &current, &usage.task_id)
                .unwrap()
                .phase
                .terminal()
        });
        let full_ms = started.elapsed().as_secs_f64() * 1000.;
        let work = runtime
            .read_task(&context(), epoch, &current, &usage.task_id)
            .unwrap();
        let result = runtime
            .read_usage(&context(), epoch, &current, &[root.root.entry_id])
            .unwrap()
            .remove(0);
        if fixture.partial {
            assert!(
                matches!(&result.logical, Measure::Partial { reasons, .. } if reasons.contains(&Reason::ResourceLimit))
            );
        } else {
            assert_eq!(work.processed_entries, fixture.entries.into());
            assert_eq!(
                result.logical,
                Measure::Complete {
                    bytes: fixture.oracle.0.into()
                }
            );
            assert_eq!(
                result.allocated,
                Measure::Complete {
                    bytes: fixture.oracle.1.into()
                }
            );
        }
        runtime
            .close_session(&context(), epoch, &current.session_id)
            .unwrap();
        wait("resource drain", &resources, &mut samples, || {
            resources.budgets.used() == baseline_bytes && resources.scheduler.handles().open() == 0
        });
        assert_eq!(fd_count(), baseline_fd);
        drop(runtime);
        assert_eq!(resources.budgets.used(), 0);
        Trial {
            profile: profile.into(),
            trial: n,
            layer: "native-runtime-without-ipc-or-webview",
            cache_condition: "OS cache uncontrolled; fixture creation/oracle/cancellation probe precede full scan",
            entries: fixture.entries,
            first_page_eligible: fixture.eligible,
            first_page_ms: first_page_ms.unwrap_or(first_ms),
            first_200_entries_ms: first_ms,
            full_scan_ms: full_ms,
            cancel_accept_ms: cancel_ms,
            cancel_was_running: before.phase == Phase::Running,
            cancel_accepted: cancel == CancelStatus::Accepted,
            samples,
            logical: result.logical,
            allocated: result.allocated,
        }
    }
    pub fn run() {
        tauri::async_runtime::block_on(async {});
        let args: Vec<_> = std::env::args().skip(1).collect();
        let count = args
            .iter()
            .find_map(|a| a.strip_prefix("--trials="))
            .map(|s| s.parse::<usize>().unwrap())
            .unwrap_or(10);
        assert!((1..=10).contains(&count));
        assert!(
            args.iter()
                .all(|arg| !arg.starts_with("--") || arg.starts_with("--trials="))
        );
        let profiles: Vec<&str> = args
            .iter()
            .filter(|s| !s.starts_with("--"))
            .map(String::as_str)
            .collect();
        let profiles = if profiles.is_empty() {
            vec!["flat10k", "flat100k", "wide", "deep", "hardlinks"]
        } else {
            profiles
        };
        for profile in profiles {
            eprintln!("creating owned {profile} fixture");
            let fixture = Fixture::new(profile);
            for n in 1..=count {
                eprintln!("measuring {profile} trial {n}");
                println!(
                    "{}",
                    serde_json::to_string(&trial(profile, n, &fixture)).unwrap()
                );
            }
        }
    }
}
