//! Executable S01 investigation, NOT a production filesystem adapter.
//! Successful counterexample tests mean the candidate fails the security gate.
#![cfg(target_os = "macos")]
use std::{
    ffi::{CStr, CString},
    fs::{self, File},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::fs::symlink,
    },
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "directory-ui-boundary-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        let path = path.canonicalize().unwrap();
        fs::create_dir(path.join("approved")).unwrap();
        fs::create_dir(path.join("outside")).unwrap();
        Self(path)
    }
    fn path(&self, relative: &str) -> PathBuf {
        self.0.join(relative)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}
fn open_beneath(parent: &impl AsRawFd, name: &str) -> std::io::Result<OwnedFd> {
    let name = CString::new(name).unwrap();
    // Values come from the installed SDK's sys/fcntl.h. libc does not necessarily
    // publish newly introduced Darwin flags; never fall back if the OS rejects them.
    const O_RESOLVE_BENEATH: i32 = 0x00001000;
    const O_NOFOLLOW_ANY: i32 = 0x20000000;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY
                | libc::O_DIRECTORY
                | libc::O_CLOEXEC
                | O_RESOLVE_BENEATH
                | O_NOFOLLOW_ANY,
        )
    };
    if fd < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}
fn get_path(fd: &impl AsRawFd) -> PathBuf {
    let mut buffer = [0_i8; libc::PATH_MAX as usize];
    assert_eq!(
        unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETPATH, buffer.as_mut_ptr()) },
        0
    );
    PathBuf::from(unsafe { CStr::from_ptr(buffer.as_ptr()) }.to_str().unwrap())
}
fn has_marker(fd: &impl AsRawFd) -> bool {
    let duplicate = unsafe { libc::dup(fd.as_raw_fd()) };
    assert!(duplicate >= 0);
    let directory = unsafe { libc::fdopendir(duplicate) };
    assert!(!directory.is_null());
    let mut found = false;
    loop {
        let entry = unsafe { libc::readdir(directory) };
        if entry.is_null() {
            break;
        }
        if unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes() == b"external-marker" {
            found = true;
        }
    }
    assert_eq!(unsafe { libc::closedir(directory) }, 0);
    found
}
fn marker_stat_succeeds(fd: &impl AsRawFd) -> bool {
    let mut value = std::mem::MaybeUninit::<libc::stat>::uninit();
    unsafe {
        libc::fstatat(
            fd.as_raw_fd(),
            c"external-marker".as_ptr(),
            value.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        ) == 0
    }
}
#[test]
fn s01_candidate_flags_block_symlink_and_parent_escape() {
    let fixture = Fixture::new();
    symlink(fixture.path("outside"), fixture.path("approved/link")).unwrap();
    let root = File::open(fixture.path("approved")).unwrap();
    assert!(open_beneath(&root, "link").is_err());
    assert!(open_beneath(&root, "../outside").is_err());
    assert!(open_beneath(&root, fixture.path("outside").to_str().unwrap()).is_err());
    fs::create_dir(fixture.path("approved/child")).unwrap();
    assert!(open_beneath(&root, "child").is_ok());
}
#[test]
fn s01_counterexample_moved_open_directory_allows_external_io() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.path("approved/child")).unwrap();
    let root = File::open(fixture.path("approved")).unwrap();
    let child = open_beneath(&root, "child").unwrap();
    assert!(get_path(&child).starts_with(fixture.path("approved")));
    // Deterministic barrier: after the last check, before the first read. The
    // writer models a non-cooperating process; the reader holds no rename lock.
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                fs::rename(
                    fixture.path("approved/child"),
                    fixture.path("outside/child"),
                )
                .unwrap();
                File::create(fixture.path("outside/child/external-marker")).unwrap();
            })
            .join()
            .unwrap();
    });
    assert!(
        marker_stat_succeeds(&child),
        "candidate unexpectedly prevented the metadata read"
    );
    assert!(
        has_marker(&child),
        "candidate unexpectedly prevented directory enumeration"
    );
    assert!(!get_path(&child).starts_with(fixture.path("approved")));
    assert!(open_beneath(&root, "child").is_err());
    println!(
        "S01 CANDIDATE FAIL: precheck accepted, rename barrier, external fstatat=success, external readdir=success, postcheck detected; external I/O count=2"
    );
}
#[test]
fn s01_counterexample_aba_move_defeats_pre_and_post_path_checks() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.path("approved/child")).unwrap();
    let root = File::open(fixture.path("approved")).unwrap();
    let child = open_beneath(&root, "child").unwrap();
    let before = get_path(&child);
    fs::rename(
        fixture.path("approved/child"),
        fixture.path("outside/child"),
    )
    .unwrap();
    File::create(fixture.path("outside/child/external-marker")).unwrap();
    assert!(marker_stat_succeeds(&child));
    assert!(has_marker(&child));
    fs::rename(
        fixture.path("outside/child"),
        fixture.path("approved/child"),
    )
    .unwrap();
    assert_eq!(get_path(&child), before);
    println!(
        "S01 CANDIDATE FAIL: ABA pre/post path equal; external metadata and listing reads succeeded"
    );
}
#[test]
fn s02_root_path_replacement_does_not_replace_open_object() {
    use std::os::unix::fs::MetadataExt;
    let fixture = Fixture::new();
    let root = File::open(fixture.path("approved")).unwrap();
    let identity = root.metadata().unwrap().ino();
    fs::rename(fixture.path("approved"), fixture.path("outside/original")).unwrap();
    fs::create_dir(fixture.path("approved")).unwrap();
    assert_ne!(
        fs::symlink_metadata(fixture.path("approved"))
            .unwrap()
            .ino(),
        identity
    );
    assert_eq!(root.metadata().unwrap().ino(), identity);
    assert!(!Path::new(&get_path(&root)).starts_with(fixture.path("approved")));
}
