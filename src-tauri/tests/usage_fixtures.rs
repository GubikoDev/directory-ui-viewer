//! Real macOS metadata validation in exclusively owned temporary trees.
//! This path-based source is a test fixture, NEVER a production security adapter.
#![cfg(unix)]
use directory_ui_viewer_lib::{
    domain::{
        controller::Cancellation,
        index::{DirectoryIndex, DirectoryKey, Origin},
        model::{FsIssue, IssueScope, Kind, Measure, Reason},
    },
    scan::usage::{FileIdentity, Metric, Observation, ObservationSource, ScanLimits, UsageScanner},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, ReadDir},
    io::{Seek, SeekFrom, Write},
    os::unix::{
        ffi::OsStrExt,
        fs::{MetadataExt, PermissionsExt, symlink},
    },
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "directory-ui-usage-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path.canonicalize().unwrap())
    }
    fn path(&self, s: &str) -> PathBuf {
        self.0.join(s)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}
fn issue(error: std::io::Error) -> FsIssue {
    FsIssue {
        code: match error.kind() {
            std::io::ErrorKind::PermissionDenied => "PERMISSION_DENIED",
            std::io::ErrorKind::NotFound => "NOT_FOUND",
            _ => "IO_ERROR",
        }
        .into(),
        operation: "fixture-read".into(),
        scope: IssueScope::Entry,
        entry_id: None,
        native_code: error.raw_os_error(),
    }
}
fn observe(path: &Path, node: u64) -> Observation {
    let metadata = fs::symlink_metadata(path).unwrap();
    Observation {
        node,
        raw_name: path.file_name().unwrap().as_bytes().to_vec(),
        kind: if metadata.file_type().is_symlink() {
            Kind::Symlink
        } else if metadata.is_dir() {
            Kind::Directory
        } else if metadata.is_file() {
            Kind::RegularFile
        } else {
            Kind::Other
        },
        identity: Some(FileIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        }),
        link_count: metadata.nlink(),
        mount: 1,
        logical: Metric::Known(metadata.len()),
        allocated: Metric::Known(metadata.blocks().checked_mul(512).unwrap()),
    }
}
struct Source {
    paths: BTreeMap<u64, PathBuf>,
    next: u64,
    opens: Vec<PathBuf>,
    remove_on_open: Option<PathBuf>,
}
impl Source {
    fn new(root: PathBuf) -> Self {
        Self {
            paths: BTreeMap::from([(1, root)]),
            next: 1,
            opens: vec![],
            remove_on_open: None,
        }
    }
}
impl ObservationSource for Source {
    type Cursor = ReadDir;
    fn open(&mut self, directory: &Observation) -> Result<ReadDir, FsIssue> {
        let path = self.paths.get(&directory.node).unwrap();
        self.opens.push(path.clone());
        if self.remove_on_open.as_ref() == Some(path) {
            fs::remove_dir(path).unwrap();
            self.remove_on_open = None;
        }
        fs::read_dir(path).map_err(issue)
    }
    fn next(&mut self, cursor: &mut ReadDir) -> Result<Option<Observation>, FsIssue> {
        let Some(entry) = cursor.next() else {
            return Ok(None);
        };
        let path = entry.map_err(issue)?.path();
        self.next += 1;
        let observation = observe(&path, self.next);
        if observation.kind == Kind::Directory {
            self.paths.insert(self.next, path);
        }
        Ok(Some(observation))
    }
}
fn expected(root: &Path) -> (u128, u128) {
    let mut stack = vec![root.to_owned()];
    let mut seen = BTreeSet::new();
    let (mut logical, mut allocated) = (0_u128, 0_u128);
    while let Some(path) = stack.pop() {
        let m = fs::symlink_metadata(&path).unwrap();
        if !seen.insert((m.dev(), m.ino())) {
            continue;
        }
        logical += m.len() as u128;
        allocated += m.blocks() as u128 * 512;
        if m.is_dir() {
            for entry in fs::read_dir(path).unwrap() {
                stack.push(entry.unwrap().path());
            }
        }
    }
    (logical, allocated)
}
fn measured(measure: &Measure) -> u128 {
    let json = serde_json::to_value(measure).unwrap();
    json.get("bytes")
        .or_else(|| json.get("observedBytes"))
        .unwrap()
        .as_str()
        .unwrap()
        .parse()
        .unwrap()
}
fn scan(root: &Path) -> UsageScanner<Source> {
    let mut scan = UsageScanner::new(
        Source::new(root.to_owned()),
        observe(root, 1),
        DirectoryIndex::new("fixture".into(), 48 * 1024 * 1024, 16 * 1024 * 1024),
        ScanLimits::default(),
        Cancellation::default(),
    )
    .unwrap();
    scan.run("2026-09-13T00:00:00Z");
    scan
}
#[test]
fn real_metadata_sparse_hidden_symlink_and_hardlink_match_independent_subtree_oracle() {
    let f = Fixture::new();
    fs::create_dir_all(f.path("root/B")).unwrap();
    fs::create_dir(f.path("root/C")).unwrap();
    fs::create_dir(f.path("root/empty")).unwrap();
    fs::write(f.path("outside-target"), vec![1; 12345]).unwrap();
    fs::write(f.path("root/.hidden"), vec![2; 9000]).unwrap();
    File::create(f.path("root/zero")).unwrap();
    let mut sparse = File::create(f.path("root/B/sparse")).unwrap();
    sparse.seek(SeekFrom::Start(16 * 1024 * 1024)).unwrap();
    sparse.write_all(&[1]).unwrap();
    drop(sparse);
    let metadata = fs::symlink_metadata(f.path("root/B/sparse")).unwrap();
    assert!(
        metadata.blocks() * 512 < metadata.len(),
        "fixture filesystem must actually support sparse allocation"
    );
    fs::hard_link(f.path("root/B/sparse"), f.path("root/C/shared")).unwrap();
    fs::hard_link(f.path("root/B/sparse"), f.path("root/B/shared-again")).unwrap();
    symlink(f.path("outside-target"), f.path("root/link-outside")).unwrap();
    symlink("missing", f.path("root/broken-link")).unwrap();
    symlink("..", f.path("root/B/cycle-link")).unwrap();
    let scanner = scan(&f.path("root"));
    let root = scanner.root_id().to_owned();
    for (id, path) in [
        (root.clone(), f.path("root")),
        (
            scanner
                .index
                .register(
                    DirectoryKey {
                        parent_id: Some(root.clone()),
                        raw_name: b"B".to_vec(),
                    },
                    Origin::Background,
                )
                .unwrap(),
            f.path("root/B"),
        ),
        (
            scanner
                .index
                .register(
                    DirectoryKey {
                        parent_id: Some(root),
                        raw_name: b"C".to_vec(),
                    },
                    Origin::Background,
                )
                .unwrap(),
            f.path("root/C"),
        ),
    ] {
        let usage = scanner.index.usage(&id).unwrap().unwrap();
        assert!(matches!(usage.logical, Measure::Complete { .. }));
        assert!(matches!(usage.allocated, Measure::Complete { .. }));
        assert_eq!(
            (measured(&usage.logical), measured(&usage.allocated)),
            expected(&path)
        );
    }
    assert!(
        scanner
            .source()
            .opens
            .iter()
            .all(|path| fs::symlink_metadata(path).unwrap().is_dir())
    );
    assert!(!scanner.source().opens.contains(&f.path("outside-target")));
}
#[test]
fn real_permission_denial_is_preserved_without_elevated_bypass() {
    let f = Fixture::new();
    fs::create_dir(f.path("root")).unwrap();
    fs::create_dir(f.path("root/denied")).unwrap();
    fs::create_dir(f.path("root/healthy")).unwrap();
    fs::write(f.path("root/healthy/file"), [1, 2, 3]).unwrap();
    let path = f.path("root/denied");
    fs::set_permissions(&path, fs::Permissions::from_mode(0)).unwrap();
    struct Restore(PathBuf);
    impl Drop for Restore {
        fn drop(&mut self) {
            fs::set_permissions(&self.0, fs::Permissions::from_mode(0o700)).unwrap();
        }
    }
    let _restore = Restore(path.clone());
    assert!(
        matches!(fs::read_dir(&path),Err(ref error)if error.kind()==std::io::ErrorKind::PermissionDenied),
        "test must run as ordinary user and observe genuine denial"
    );
    let scanner = scan(&f.path("root"));
    let usage = scanner.index.usage(scanner.root_id()).unwrap().unwrap();
    assert!(
        matches!(&usage.logical,Measure::Partial{reasons,..}if reasons.contains(&Reason::ReadError))
    );
    assert!(
        scanner
            .issues
            .samples
            .iter()
            .any(|issue| issue.code == "PERMISSION_DENIED" && issue.native_code.is_some())
    );
    assert!(scanner.source().opens.contains(&f.path("root/healthy")));
}

#[test]
fn deletion_between_observation_and_open_preserves_healthy_siblings() {
    let f = Fixture::new();
    fs::create_dir(f.path("root")).unwrap();
    fs::create_dir(f.path("root/removed")).unwrap();
    fs::create_dir(f.path("root/healthy")).unwrap();
    fs::write(f.path("root/healthy/file"), [1, 2, 3]).unwrap();
    let root = f.path("root");
    let mut source = Source::new(root.clone());
    source.remove_on_open = Some(f.path("root/removed"));
    let mut scanner = UsageScanner::new(
        source,
        observe(&root, 1),
        DirectoryIndex::new("fixture".into(), 100_000, 100_000),
        ScanLimits::default(),
        Cancellation::default(),
    )
    .unwrap();
    scanner.run("2026-09-13T00:00:00Z");
    assert!(
        scanner
            .issues
            .samples
            .iter()
            .any(|issue| issue.code == "NOT_FOUND" && issue.native_code.is_some())
    );
    let usage = scanner.index.usage(scanner.root_id()).unwrap().unwrap();
    assert!(
        matches!(&usage.logical, Measure::Partial { reasons, .. } if reasons.contains(&Reason::ReadError))
    );
    assert!(scanner.source().opens.contains(&f.path("root/healthy")));
}
#[test]
fn real_depth_256_keeps_native_open_directory_count_bounded() {
    let f = Fixture::new();
    let root = f.path("root");
    fs::create_dir(&root).unwrap();
    let mut path = root.clone();
    for _ in 0..256 {
        path.push("d");
        fs::create_dir(&path).unwrap();
    }
    let scanner = scan(&root);
    assert_eq!(scanner.stats.peak_open_directories, 128);
    assert!(
        matches!(&scanner.index.usage(scanner.root_id()).unwrap().unwrap().logical,Measure::Partial{reasons,..}if reasons.contains(&Reason::ResourceLimit))
    );
}
