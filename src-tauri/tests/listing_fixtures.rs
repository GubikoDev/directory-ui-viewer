//! Actual directory enumeration of exclusively owned, temporary test fixtures.
//! This path-based source is NOT a production adapter or an S01 security claim.
#![cfg(unix)]
use directory_ui_viewer_lib::{
    domain::{
        controller::Scope,
        index::{DirectoryIndex, DirectoryKey, Origin},
        model::{Category, Coverage, Entry, Field, FollowPolicy, FsIssue, IssueScope, Kind, Phase},
    },
    listing::{
        cache::ListingCache,
        service::{ListedEntry, ListingService, ListingSource},
    },
    scheduler::Scheduler,
};
use std::{
    collections::BTreeSet,
    fs,
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::{MetadataExt, PermissionsExt, symlink},
    },
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
static SERIAL: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "directory-listing-fixture-{}-{}",
            std::process::id(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}
struct Source {
    path: PathBuf,
    expected_root: String,
}
fn io_issue(error: std::io::Error, scope: IssueScope) -> FsIssue {
    FsIssue {
        code: match error.kind() {
            std::io::ErrorKind::PermissionDenied => "PERMISSION_DENIED",
            std::io::ErrorKind::NotFound => "NOT_FOUND",
            _ => "IO_ERROR",
        }
        .into(),
        operation: "list".into(),
        scope,
        entry_id: None,
        native_code: error.raw_os_error(),
    }
}
impl ListingSource for Source {
    type Cursor = fs::ReadDir;
    fn open(&mut self, directory_id: &str) -> Result<fs::ReadDir, FsIssue> {
        assert_eq!(directory_id, self.expected_root);
        fs::read_dir(&self.path).map_err(|e| io_issue(e, IssueScope::Subtree))
    }
    fn next(&mut self, cursor: &mut fs::ReadDir) -> Result<Option<ListedEntry>, FsIssue> {
        let Some(next) = cursor.next() else {
            return Ok(None);
        };
        let next = next.map_err(|e| io_issue(e, IssueScope::Subtree))?;
        let raw_name = next.file_name().as_bytes().to_vec();
        let mut entry = Entry {
            entry_id: String::new(),
            parent_id: None,
            display_name: next.file_name().to_string_lossy().into_owned(),
            kind: Kind::Unknown,
            hidden: Field::Known {
                value: raw_name.starts_with(b"."),
            },
            special_type: Field::Unsupported,
            modified_at: Field::Unknown,
            own_logical_bytes: Field::Unknown,
            own_allocated_bytes: Field::Unknown,
            observed_at: "2026-09-13T00:00:00Z".into(),
            follow_policy: FollowPolicy::Never,
        };
        let issue = match fs::symlink_metadata(next.path()) {
            Ok(metadata) => {
                entry.kind = if metadata.is_dir() {
                    Kind::Directory
                } else if metadata.is_file() {
                    Kind::RegularFile
                } else if metadata.file_type().is_symlink() {
                    Kind::Symlink
                } else {
                    Kind::Other
                };
                entry.own_logical_bytes = Field::Known {
                    value: metadata.len().into(),
                };
                entry.own_allocated_bytes = Field::Known {
                    value: (metadata.blocks() as u128 * 512).into(),
                };
                None
            }
            Err(error) => {
                let issue = io_issue(error, IssueScope::Entry);
                entry.own_logical_bytes = Field::Error {
                    issue: issue.clone(),
                };
                entry.own_allocated_bytes = Field::Error {
                    issue: issue.clone(),
                };
                Some(issue)
            }
        };
        Ok(Some(ListedEntry {
            raw_name,
            entry,
            issue,
        }))
    }
}
fn setup() -> (ListingService, String, Arc<Scheduler>) {
    let mut index = DirectoryIndex::new("fixture".into(), 48 * 1024 * 1024, 16 * 1024 * 1024);
    let root = index
        .register(
            DirectoryKey {
                parent_id: None,
                raw_name: b"root".to_vec(),
            },
            Origin::Foreground,
        )
        .unwrap();
    let scheduler = Arc::new(Scheduler::default());
    (
        ListingService::new(
            Scope {
                session_id: "fixture".into(),
                generation: 1,
            },
            index.into(),
            scheduler.clone(),
            ListingCache::new(48 * 1024 * 1024, 300_000, 1_048_576),
        ),
        root,
        scheduler,
    )
}
fn collect(service: &ListingService, id: &str) -> Vec<Entry> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut cursor = None;
    let mut entries = vec![];
    loop {
        assert!(Instant::now() < deadline, "listing did not terminate");
        let page = service.page(id, cursor.as_deref(), 200, 1).unwrap();
        entries.extend(page.entries);
        if page.next_cursor.is_none() {
            assert_eq!(page.coverage, Coverage::Complete);
            break;
        }
        cursor = page.next_cursor;
        std::thread::yield_now();
    }
    entries
}
#[test]
fn actual_paged_listing_covers_every_entry_and_keeps_link_self_and_raw_names_distinct() {
    let fixture = Fixture::new();
    let mut expected = BTreeSet::new();
    for n in 0..1001 {
        let name = format!("f{n}");
        fs::write(fixture.0.join(&name), []).unwrap();
        expected.insert(name);
    }
    fs::write(fixture.0.join(".hidden"), b"123").unwrap();
    expected.insert(".hidden".into());
    symlink("missing-outside-target", fixture.0.join("broken")).unwrap();
    expected.insert("broken".into());
    fs::create_dir(fixture.0.join("directory")).unwrap();
    // APFS rejects malformed UTF-8 with EILSEQ. Linux fixtures exercise raw
    // non-UTF8 components; macOS exercises actual Unicode component bytes.
    #[cfg(target_os = "linux")]
    let raw_directories = [vec![b'd', 0xfe], vec![b'd', 0xff]];
    #[cfg(not(target_os = "linux"))]
    let raw_directories = ["한글".as_bytes().to_vec(), "日本語".as_bytes().to_vec()];
    for raw in raw_directories {
        fs::create_dir(fixture.0.join(std::ffi::OsString::from_vec(raw))).unwrap();
    }
    let expected_directories: BTreeSet<_> = fs::read_dir(&fixture.0)
        .unwrap()
        .map(Result::unwrap)
        .filter(|entry| entry.file_type().unwrap().is_dir())
        .map(|entry| entry.file_name().as_bytes().to_vec())
        .collect();
    let (service, root, scheduler) = setup();
    let files = service
        .start(
            &root,
            Category::Files,
            200,
            "2026-09-13T00:00:00Z",
            0,
            Source {
                path: fixture.0.clone(),
                expected_root: root.clone(),
            },
        )
        .unwrap();
    let entries = collect(&service, &files.task_id);
    assert_eq!(
        entries
            .iter()
            .map(|e| e.display_name.clone())
            .collect::<BTreeSet<_>>(),
        expected
    );
    assert_eq!(entries.len(), 1003);
    assert_eq!(
        entries
            .iter()
            .map(|e| &e.entry_id)
            .collect::<BTreeSet<_>>()
            .len(),
        entries.len()
    );
    let link = entries.iter().find(|e| e.display_name == "broken").unwrap();
    assert_eq!(link.kind, Kind::Symlink);
    assert_eq!(
        link.own_logical_bytes,
        Field::Known {
            value: 22_u64.into()
        }
    );
    assert_eq!(
        service.work(&files.task_id, 1).unwrap().processed_entries,
        1006_u64.into()
    );
    let directories = service
        .start(
            &root,
            Category::Directories,
            200,
            "2026-09-13T00:00:00Z",
            1,
            Source {
                path: fixture.0.clone(),
                expected_root: root.clone(),
            },
        )
        .unwrap();
    let dirs = collect(&service, &directories.task_id);
    assert_eq!(dirs.len(), 3);
    assert_eq!(
        dirs.iter()
            .map(|e| &e.entry_id)
            .collect::<BTreeSet<_>>()
            .len(),
        3
    );
    let raw = dirs
        .iter()
        .map(|e| service.index.key(&e.entry_id).unwrap().raw_name)
        .collect::<BTreeSet<_>>();
    assert_eq!(raw, expected_directories);
    service.close();
    let deadline = Instant::now() + Duration::from_secs(3);
    while scheduler.handles().open() != 0 {
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
}
#[test]
fn actual_permission_denial_is_a_failed_partial_listing_not_an_empty_directory() {
    let fixture = Fixture::new();
    fs::set_permissions(&fixture.0, fs::Permissions::from_mode(0)).unwrap();
    let (service, root, scheduler) = setup();
    let handle = service
        .start(
            &root,
            Category::Directories,
            200,
            "2026-09-13T00:00:00Z",
            0,
            Source {
                path: fixture.0.clone(),
                expected_root: root.clone(),
            },
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    let work = loop {
        let work = service.work(&handle.task_id, 1).unwrap();
        if work.phase.terminal() {
            break work;
        }
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    };
    fs::set_permissions(&fixture.0, fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(work.phase, Phase::Failed);
    assert!(work.issues.counts.contains_key("PERMISSION_DENIED"));
    assert!(work.issues.samples[0].native_code.is_some());
    let page = service.page(&handle.task_id, None, 200, 1).unwrap();
    assert!(page.entries.is_empty());
    assert_eq!(page.coverage, Coverage::Partial);
    service.close();
    let deadline = Instant::now() + Duration::from_secs(3);
    while scheduler.handles().open() != 0 {
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
}
