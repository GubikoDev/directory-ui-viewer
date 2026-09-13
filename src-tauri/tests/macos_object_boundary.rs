//! Current object-approval policy tests, using only self-created temp fixtures.
#![cfg(target_os = "macos")]
use directory_ui_viewer_lib::{
    platform::macos::{ApprovedRoot, BoundaryError, Directory},
    runtime::memory::ByteBudget,
    scheduler::HandleBudget,
};
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "duv-object-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        for name in ["root", "outside"] {
            fs::create_dir(path.join(name)).unwrap()
        }
        Self(path.canonicalize().unwrap())
    }
    fn p(&self, p: &str) -> PathBuf {
        self.0.join(p)
    }
    fn approve(&self) -> ApprovedRoot {
        ApprovedRoot::open_picker_candidate(
            &self.p("root"),
            HandleBudget::new(320),
            ByteBudget::new(4 * 1024 * 1024),
        )
        .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap()
    }
}
fn names(dir: &Directory) -> Vec<Vec<u8>> {
    let mut cursor = dir.cursor().unwrap();
    let mut names = vec![];
    while let Some(name) = cursor.next_name().unwrap() {
        names.push(name)
    }
    names.sort();
    names
}
#[test]
fn s01_components_and_symlink_targets_are_not_opened() {
    let f = Fixture::new();
    let root = f.approve();
    fs::write(f.p("outside/secret"), b"outside").unwrap();
    for (name, target) in [
        ("outside", f.p("outside")),
        ("broken", f.p("missing")),
        ("cycle", f.p("root/cycle")),
    ] {
        symlink(target, f.p(&format!("root/{name}"))).unwrap();
        let link = root.directory().observe(name.as_bytes()).unwrap();
        assert_eq!(link.metadata().st_mode & libc::S_IFMT, libc::S_IFLNK);
        assert!(matches!(
            root.directory().open_observed(&link),
            Err(BoundaryError::ChangedObject)
        ));
    }
    for name in [
        b"../outside".as_slice(),
        b"/tmp",
        b".",
        b"..",
        b"a/b",
        b"a\0b",
        b"",
    ] {
        assert!(matches!(
            root.directory().observe(name),
            Err(BoundaryError::InvalidComponent)
        ));
    }
    assert_eq!(
        names(root.directory()),
        vec![b"broken".to_vec(), b"cycle".to_vec(), b"outside".to_vec()]
    );
}
#[test]
fn s01_acquired_object_survives_move_and_aba_but_stale_name_does_not_rebind() {
    let f = Fixture::new();
    fs::create_dir(f.p("root/child")).unwrap();
    let root = f.approve();
    let observed = root.directory().observe(b"child").unwrap();
    let child = root.directory().open_observed(&observed).unwrap();
    std::thread::scope(|s| {
        s.spawn(|| {
            fs::rename(f.p("root/child"), f.p("outside/moved")).unwrap();
            fs::write(f.p("outside/moved/marker"), b"object").unwrap();
            fs::create_dir(f.p("root/child")).unwrap();
        })
        .join()
        .unwrap();
    });
    assert_eq!(names(&child), vec![b"marker".to_vec()]);
    assert_eq!(child.observe(b"marker").unwrap().metadata().st_size, 6);
    assert!(matches!(
        root.directory().open_observed(&observed),
        Err(BoundaryError::ChangedObject)
    ));
    fs::remove_dir(f.p("root/child")).unwrap();
    fs::rename(f.p("outside/moved"), f.p("root/child")).unwrap();
    assert_eq!(names(&child), vec![b"marker".to_vec()]);
    assert_eq!(
        root.directory()
            .open_observed(&observed)
            .unwrap()
            .identity(),
        child.identity()
    );
}
#[test]
fn s02_root_refresh_preserves_original_object_and_cursor_offsets_are_independent() {
    let f = Fixture::new();
    fs::write(f.p("root/original"), b"").unwrap();
    let root = f.approve();
    let id = root.directory().identity();
    fs::rename(f.p("root"), f.p("outside/moved-root")).unwrap();
    fs::create_dir(f.p("root")).unwrap();
    fs::write(f.p("root/replacement-secret"), b"").unwrap();
    for _ in 0..3 {
        assert_eq!(names(root.directory()), vec![b"original".to_vec()]);
        assert_eq!(root.directory().identity(), id)
    }
    let second = f.approve();
    assert_ne!(second.directory().identity(), id);
    assert_eq!(
        names(second.directory()),
        vec![b"replacement-secret".to_vec()]
    );
}
#[test]
fn parent_provenance_and_revocation_are_enforced_and_handles_remain_charged() {
    let f = Fixture::new();
    fs::create_dir(f.p("root/a")).unwrap();
    fs::create_dir(f.p("root/b")).unwrap();
    let handles = HandleBudget::new(3);
    let bytes = ByteBudget::new(4 * 1024 * 1024);
    let root =
        ApprovedRoot::open_picker_candidate(&f.p("root"), handles.clone(), bytes.clone()).unwrap();
    let observed = root.directory().observe(b"a").unwrap();
    let b = root
        .directory()
        .open_observed(&root.directory().observe(b"b").unwrap())
        .unwrap();
    assert!(matches!(
        b.open_observed(&observed),
        Err(BoundaryError::ChangedObject)
    ));
    let mut cursor = root.directory().cursor().unwrap();
    assert_eq!(handles.open(), 3);
    assert!(matches!(
        root.directory().cursor(),
        Err(BoundaryError::ResourceLimit)
    ));
    let charged = bytes.used();
    root.revoke();
    assert_eq!(bytes.used(), charged);
    assert_eq!(handles.open(), 3);
    assert!(matches!(cursor.next_name(), Err(BoundaryError::Revoked)));
    assert!(matches!(b.metadata(), Err(BoundaryError::Revoked)));
    assert!(matches!(
        root.directory().observe(b"a"),
        Err(BoundaryError::Revoked)
    ));
    drop(root);
    drop(b);
    drop(cursor);
    assert_eq!(handles.open(), 0);
    assert_eq!(bytes.used(), 0);
}
#[test]
fn native_permission_errors_and_large_bounded_cursor_are_preserved() {
    let f = Fixture::new();
    fs::create_dir(f.p("root/denied")).unwrap();
    let root = f.approve();
    let observed = root.directory().observe(b"denied").unwrap();
    fs::set_permissions(f.p("root/denied"), fs::Permissions::from_mode(0)).unwrap();
    let result = root.directory().open_observed(&observed);
    fs::set_permissions(f.p("root/denied"), fs::Permissions::from_mode(0o700)).unwrap();
    assert!(
        matches!(result,Err(BoundaryError::Native(ref e)) if e.raw_os_error()==Some(libc::EACCES))
    );
    for i in 0..2000 {
        fs::write(f.p(&format!("root/항목-{i:04}")), b"").unwrap()
    }
    let handles = HandleBudget::new(2);
    let memory = ByteBudget::new(40 * 1024);
    let root =
        ApprovedRoot::open_picker_candidate(&f.p("root"), handles.clone(), memory.clone()).unwrap();
    let baseline = memory.used();
    let mut cursor = root.directory().cursor().unwrap();
    let cost = memory.used();
    let mut count = 0;
    while cursor.next_name().unwrap().is_some() {
        count += 1;
        assert_eq!(memory.used(), cost)
    }
    assert_eq!(count, 2001);
    assert!(cost <= 40 * 1024);
    drop(cursor);
    assert_eq!(memory.used(), baseline);
    drop(root);
    assert_eq!(handles.open(), 0);
    assert_eq!(memory.used(), 0);
}
#[test]
fn repeated_directory_to_symlink_substitution_never_opens_target() {
    let f = Fixture::new();
    fs::create_dir(f.p("root/child")).unwrap();
    fs::write(f.p("outside/secret"), b"").unwrap();
    let root = f.approve();
    for _ in 0..250 {
        let observed = root.directory().observe(b"child").unwrap();
        std::thread::scope(|s| {
            s.spawn(|| {
                fs::rename(f.p("root/child"), f.p("saved")).unwrap();
                symlink(f.p("outside"), f.p("root/child")).unwrap();
            })
            .join()
            .unwrap();
        });
        assert!(matches!(
            root.directory().open_observed(&observed),
            Err(BoundaryError::Native(_))
        ));
        fs::remove_file(f.p("root/child")).unwrap();
        fs::rename(f.p("saved"), f.p("root/child")).unwrap();
    }
}

#[test]
#[ignore = "Creates and detaches its own temporary HFS+ disk image; run explicitly on macOS"]
fn s03_actual_disk_image_mount_is_not_traversed_without_separate_approval() {
    use std::process::Command;
    let f = Fixture::new();
    fs::create_dir(f.p("root/mounted")).unwrap();
    let created = Command::new("hdiutil")
        .args([
            "create",
            "-size",
            "32m",
            "-fs",
            "HFS+",
            "-volname",
            "DUVTest",
            "-nospotlight",
            "-quiet",
        ])
        .arg(f.p("fixture.dmg"))
        .output()
        .unwrap();
    assert!(created.status.success(), "disk image creation failed");
    let attached = Command::new("hdiutil")
        .arg("attach")
        .arg(f.p("fixture.dmg"))
        .arg("-mountpoint")
        .arg(f.p("root/mounted"))
        .args(["-noautoopen", "-quiet"])
        .output()
        .unwrap();
    assert!(
        attached.status.success(),
        "temporary disk image attach failed"
    );
    // Keep the fixture inside the guard: if detach fails, never recursively
    // remove a still-mounted tree. Report failure and leave it for diagnosis.
    struct Mounted(Option<Fixture>);
    impl Drop for Mounted {
        fn drop(&mut self) {
            let fixture = self.0.take().unwrap();
            let detached = Command::new("hdiutil")
                .arg("detach")
                .arg(fixture.p("root/mounted"))
                .arg("-quiet")
                .status();
            if !detached.is_ok_and(|s| s.success()) {
                std::mem::forget(fixture);
                panic!("temporary mount detach failed; fixture preserved");
            }
        }
    }
    let mounted = Mounted(Some(f));
    let f = mounted.0.as_ref().unwrap();
    fs::write(f.p("root/mounted/inside-mount"), b"").unwrap();
    let root = f.approve();
    let observed = root.directory().observe(b"mounted").unwrap();
    assert!(matches!(
        root.directory().open_observed(&observed),
        Err(BoundaryError::MountBoundary)
    ));
    let separate = ApprovedRoot::open_picker_candidate(
        &f.p("root/mounted"),
        HandleBudget::new(5),
        ByteBudget::new(128 * 1024),
    )
    .unwrap();
    assert!(names(separate.directory()).contains(&b"inside-mount".to_vec()));
    drop(separate);
    drop(root);
    drop(mounted);
}

#[test]
fn concurrent_symlink_swap_keeps_every_successful_handle_on_original_object() {
    use std::sync::{Arc, Barrier};
    let f = Fixture::new();
    fs::create_dir(f.p("root/child")).unwrap();
    fs::write(f.p("root/child/safe"), b"").unwrap();
    fs::write(f.p("outside/secret"), b"").unwrap();
    let root = f.approve();
    let observed = root.directory().observe(b"child").unwrap();
    let original = root.directory().open_observed(&observed).unwrap();
    let id = original.identity();
    let barrier = Arc::new(Barrier::new(2));
    let successes = std::thread::scope(|s| {
        let barrier2 = barrier.clone();
        let f = &f;
        let writer = s.spawn(move || {
            barrier2.wait();
            for _ in 0..500 {
                fs::rename(f.p("root/child"), f.p("saved")).unwrap();
                symlink(f.p("outside"), f.p("root/child")).unwrap();
                std::thread::yield_now();
                fs::remove_file(f.p("root/child")).unwrap();
                fs::rename(f.p("saved"), f.p("root/child")).unwrap();
            }
        });
        barrier.wait();
        let mut success = 0;
        for _ in 0..1000 {
            match root.directory().open_observed(&observed) {
                Ok(child) => {
                    assert_eq!(child.identity(), id);
                    assert_eq!(names(&child), vec![b"safe".to_vec()]);
                    success += 1
                }
                Err(BoundaryError::Native(_)) => {}
                Err(e) => panic!("unexpected boundary error: {e:?}"),
            }
        }
        writer.join().unwrap();
        success
    });
    assert_eq!(names(&original), vec![b"safe".to_vec()]);
    println!(
        "concurrent swap: 1000 acquisitions, {successes} original-object successes, all others rejected"
    );
}

#[test]
fn legal_apfs_multibyte_component_longer_than_255_bytes_is_not_rejected() {
    use directory_ui_viewer_lib::domain::index::{DirectoryIndex, DirectoryKey, Origin};
    let f = Fixture::new();
    let name = "한".repeat(255);
    assert_eq!(name.len(), 765);
    fs::create_dir(f.p(&format!("root/{name}"))).unwrap();
    let root = f.approve();
    assert_eq!(names(root.directory()), vec![name.as_bytes().to_vec()]);
    let observed = root.directory().observe(name.as_bytes()).unwrap();
    assert!(root.directory().open_observed(&observed).is_ok());
    let mut index = DirectoryIndex::new("scope".into(), 1024 * 1024, 1024 * 1024);
    let id = index
        .register(
            DirectoryKey {
                parent_id: None,
                raw_name: vec![],
            },
            Origin::Background,
        )
        .unwrap();
    assert!(
        index
            .register(
                DirectoryKey {
                    parent_id: Some(id),
                    raw_name: name.into_bytes()
                },
                Origin::Foreground
            )
            .is_ok()
    );
}

#[test]
fn permission_revoked_after_cursor_open_is_not_empty_success_or_infinite_retry() {
    let f = Fixture::new();
    fs::create_dir(f.p("root/child")).unwrap();
    fs::write(f.p("root/child/item"), b"").unwrap();
    let root = f.approve();
    let child = root
        .directory()
        .open_observed(&root.directory().observe(b"child").unwrap())
        .unwrap();
    let mut cursor = child.into_cursor().unwrap();
    fs::set_permissions(f.p("root/child"), fs::Permissions::from_mode(0)).unwrap();
    let error = cursor.next_name();
    fs::set_permissions(f.p("root/child"), fs::Permissions::from_mode(0o700)).unwrap();
    assert!(
        matches!(error,Err(BoundaryError::Native(ref e)) if e.raw_os_error()==Some(libc::EACCES))
    );
    assert!(cursor.next_name().unwrap().is_none());
    assert_eq!(names(cursor.directory()), vec![b"item".to_vec()]);
}
