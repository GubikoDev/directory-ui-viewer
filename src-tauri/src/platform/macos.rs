//! Native approval and metadata checks around walkdir directory enumeration.
//! Paths originate only from the native picker and validated child components.
//! Moving a directory can expire access; it does not approve its replacement.
use crate::{
    runtime::memory::{ByteBudget, Reservation},
    scheduler::{HandleBudget, HandlePermit},
};
use std::{
    ffi::{CStr, CString},
    io,
    mem::MaybeUninit,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::ffi::OsStrExt,
    },
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

const O_RESOLVE_BENEATH: i32 = 0x00001000; // Installed macOS SDK sys/fcntl.h.
const O_NOFOLLOW_ANY: i32 = 0x20000000;
const FLAGS: i32 = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | O_NOFOLLOW_ANY;
const HANDLE_BYTES: usize = 2048;

#[derive(Debug)]
pub enum BoundaryError {
    Native(io::Error),
    EntryNative(io::Error),
    InvalidComponent,
    Revoked,
    ResourceLimit,
    ChangedObject,
    MountBoundary,
}
type Result<T> = std::result::Result<T, BoundaryError>;
impl From<io::Error> for BoundaryError {
    fn from(e: io::Error) -> Self {
        Self::Native(e)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Identity {
    device: i32,
    inode: u64,
    generation: u32,
    birth_sec: i64,
    birth_nsec: i64,
}
impl Identity {
    pub fn fingerprint(&self) -> crate::domain::index::DirectoryFingerprint {
        crate::domain::index::DirectoryFingerprint([
            self.device as u32 as u64,
            self.inode,
            self.generation as u64,
            self.birth_sec as u64,
            self.birth_nsec as u64,
        ])
    }
    fn of(s: &libc::stat) -> Self {
        Self {
            device: s.st_dev,
            inode: s.st_ino,
            generation: s.st_gen,
            birth_sec: s.st_birthtime,
            birth_nsec: s.st_birthtime_nsec,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct Mount {
    fsid: [i32; 2],
    point: Vec<u8>,
}
struct Grant {
    revoked: AtomicBool,
    handles: HandleBudget,
    memory: ByteBudget,
}
impl Grant {
    fn check(&self) -> Result<()> {
        if self.revoked.load(Ordering::Acquire) {
            Err(BoundaryError::Revoked)
        } else {
            Ok(())
        }
    }
    fn reserve(&self) -> Result<(HandlePermit, Reservation)> {
        self.check()?;
        let permit = self
            .handles
            .acquire()
            .map_err(|_| BoundaryError::ResourceLimit)?;
        let reservation = self
            .memory
            .reserve(HANDLE_BYTES)
            .map_err(|_| BoundaryError::ResourceLimit)?;
        Ok((permit, reservation))
    }
}
// A tag identifies the exact parent capability, never just its display path.
struct ParentTag;
pub struct Directory {
    path: PathBuf,
    fd: OwnedFd,
    _permit: HandlePermit,
    grant: Arc<Grant>,
    mount: Mount,
    identity: Identity,
    tag: Arc<ParentTag>,
    _memory: Reservation,
}
pub struct ApprovedRoot {
    directory: Directory,
}
pub struct ChildObservation {
    name: CString,
    parent: Arc<ParentTag>,
    stat: libc::stat,
}
impl ChildObservation {
    pub fn identity(&self) -> Identity {
        Identity::of(&self.stat)
    }
    pub fn metadata(&self) -> &libc::stat {
        &self.stat
    }
    pub fn raw_name(&self) -> &[u8] {
        self.name.as_bytes()
    }
}
fn component(name: &[u8]) -> Result<CString> {
    if name.is_empty()
        || name.len() > crate::domain::index::MAX_COMPONENT_BYTES
        || name == b"."
        || name == b".."
        || name.contains(&b'/')
    {
        return Err(BoundaryError::InvalidComponent);
    }
    CString::new(name).map_err(|_| BoundaryError::InvalidComponent)
}
fn stat_fd(fd: i32) -> Result<libc::stat> {
    let mut stat = MaybeUninit::uninit();
    if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } < 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(unsafe { stat.assume_init() })
}
fn mount_fd(fd: i32) -> Result<Mount> {
    let mut stat = MaybeUninit::uninit();
    if unsafe { libc::fstatfs(fd, stat.as_mut_ptr()) } < 0 {
        return Err(io::Error::last_os_error().into());
    }
    let stat = unsafe { stat.assume_init() };
    Ok(Mount {
        // libc 0.2.189 fsid_t is repr(C), one private [i32; 2] field.
        // This conversion is compile-time size checked; do not infer st_dev.
        fsid: unsafe { std::mem::transmute::<libc::fsid_t, [i32; 2]>(stat.f_fsid) },
        point: unsafe { CStr::from_ptr(stat.f_mntonname.as_ptr()) }
            .to_bytes()
            .to_vec(),
    })
}
fn owned(fd: i32) -> Result<OwnedFd> {
    if fd < 0 {
        Err(io::Error::last_os_error().into())
    } else {
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}
impl ApprovedRoot {
    /// Invoke only on a fixed native I/O worker with a trusted picker result.
    /// Approval refers to the successfully opened candidate, not click-time inode.
    pub fn open_picker_candidate(
        path: &Path,
        handles: HandleBudget,
        memory: ByteBudget,
    ) -> Result<Self> {
        let grant = Arc::new(Grant {
            revoked: AtomicBool::new(false),
            handles,
            memory,
        });
        let (permit, reservation) = grant.reserve()?;
        let canonical = path.canonicalize()?;
        let path = CString::new(canonical.as_os_str().as_bytes())
            .map_err(|_| BoundaryError::InvalidComponent)?;
        let fd = owned(unsafe { libc::open(path.as_ptr(), FLAGS) })?;
        let identity = Identity::of(&stat_fd(fd.as_raw_fd())?);
        let mount = mount_fd(fd.as_raw_fd())?;
        Ok(Self {
            directory: Directory {
                path: canonical,
                fd,
                _permit: permit,
                _memory: reservation,
                grant,
                mount,
                identity,
                tag: Arc::new(ParentTag),
            },
        })
    }
    pub fn directory(&self) -> &Directory {
        &self.directory
    }
    /// Nonblocking revocation. Owned handles remain charged until their owners
    /// drop; runtime cancellation must promptly retire parked/inactive owners.
    pub fn revoke(&self) {
        self.directory.grant.revoked.store(true, Ordering::Release);
    }
}
impl Drop for ApprovedRoot {
    fn drop(&mut self) {
        self.revoke()
    }
}
impl Directory {
    pub fn work_budget(&self) -> &ByteBudget {
        &self.grant.memory
    }
    pub fn handle_budget(&self) -> &HandleBudget {
        &self.grant.handles
    }
    pub fn identity(&self) -> Identity {
        self.identity
    }
    /// Check that the saved path still names this directory. Temporary handles
    /// are scoped to this call; no path is sent over IPC or included in errors.
    fn validate_path(&self) -> Result<()> {
        self.grant.check()?;
        let path = CString::new(self.path.as_os_str().as_bytes())
            .map_err(|_| BoundaryError::InvalidComponent)?;
        let fd = owned(unsafe { libc::open(path.as_ptr(), FLAGS) })?;
        if Identity::of(&stat_fd(fd.as_raw_fd())?) != self.identity {
            return Err(BoundaryError::ChangedObject);
        }
        self.grant.check()
    }
    pub fn metadata(&self) -> Result<libc::stat> {
        self.validate_path()?;
        let stat = stat_fd(self.fd.as_raw_fd())?;
        self.grant.check()?;
        Ok(stat)
    }
    /// Only the link's own metadata is queried. No target open/readlink follows.
    pub fn observe(&self, name: &[u8]) -> Result<ChildObservation> {
        let name = component(name)?;
        self.grant.check()?;
        let mut stat = MaybeUninit::uninit();
        let result = unsafe {
            libc::fstatat(
                self.fd.as_raw_fd(),
                name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        let error = io::Error::last_os_error();
        self.grant.check()?;
        if result < 0 {
            return Err(error.into());
        }
        Ok(ChildObservation {
            name,
            parent: self.tag.clone(),
            stat: unsafe { stat.assume_init() },
        })
    }
    pub fn open_observed(&self, child: &ChildObservation) -> Result<Directory> {
        if !Arc::ptr_eq(&self.tag, &child.parent) {
            return Err(BoundaryError::ChangedObject);
        }
        if child.stat.st_mode & libc::S_IFMT != libc::S_IFDIR {
            return Err(BoundaryError::ChangedObject);
        }
        self.open_component(&child.name, Some(Identity::of(&child.stat)))
    }
    fn open_component(&self, name: &CStr, expected: Option<Identity>) -> Result<Directory> {
        self.validate_path()?;
        let (permit, reservation) = self.grant.reserve()?;
        // Operations admitted before revoke may drain. Neither errors nor
        // successful results from that interval are published after revocation.
        let fd = owned(unsafe {
            libc::openat(
                self.fd.as_raw_fd(),
                name.as_ptr(),
                FLAGS | O_RESOLVE_BENEATH,
            )
        });
        self.grant.check()?;
        let fd = fd?;
        let identity = Identity::of(&stat_fd(fd.as_raw_fd())?);
        if expected.is_some_and(|expected| expected != identity) {
            return Err(BoundaryError::ChangedObject);
        }
        let mount = mount_fd(fd.as_raw_fd())?;
        if mount != self.mount {
            return Err(BoundaryError::MountBoundary);
        }
        self.grant.check()?;
        Ok(Directory {
            path: if name.to_bytes() == b"." {
                self.path.clone()
            } else {
                self.path.join(std::ffi::OsStr::from_bytes(name.to_bytes()))
            },
            fd,
            _permit: permit,
            _memory: reservation,
            grant: self.grant.clone(),
            mount,
            identity,
            tag: Arc::new(ParentTag),
        })
    }
    /// An independent open file description avoids dup's shared directory offset.
    pub fn cursor(&self) -> Result<DirectoryCursor> {
        let directory = self.open_component(c".", Some(self.identity))?;
        directory.into_cursor()
    }
    pub fn into_cursor(self) -> Result<DirectoryCursor> {
        self.validate_path()?;
        let entries = walkdir::WalkDir::new(&self.path)
            .min_depth(1)
            .max_depth(1)
            .follow_links(false)
            .follow_root_links(false)
            .max_open(1)
            .into_iter();
        Ok(DirectoryCursor {
            entries,
            directory: Arc::new(self),
            ended: false,
        })
    }
}
/// A streaming walkdir cursor. UsageScanner owns subtree scheduling/aggregation,
/// so max_depth(1) avoids a second recursive walk and preserves lazy listings.
/// Library allocations are not included in our application's reservation meter.
pub struct DirectoryCursor {
    entries: walkdir::IntoIter,
    directory: Arc<Directory>,
    ended: bool,
}
impl DirectoryCursor {
    pub fn parent_handle(&self) -> Arc<Directory> {
        self.directory.clone()
    }
    pub fn directory(&self) -> &Directory {
        &self.directory
    }
    pub fn next_name(&mut self) -> Result<Option<Vec<u8>>> {
        self.directory.grant.check()?;
        if self.ended {
            return Ok(None);
        }
        // Reject ordinary move/replacement and permission changes. This is not
        // an atomic snapshot across concurrent filesystem mutations.
        if let Err(error) = self.directory.validate_path() {
            self.ended = true;
            return Err(error);
        }
        let next = self.entries.next();
        self.directory.grant.check()?;
        match next {
            Some(Ok(entry)) => {
                self.directory
                    .validate_path()
                    .inspect_err(|_| self.ended = true)?;
                let name = entry.file_name().as_bytes();
                component(name)?;
                Ok(Some(name.to_vec()))
            }
            Some(Err(error)) => {
                let entry_only = error.depth() > 0;
                let error = error
                    .into_io_error()
                    .unwrap_or_else(|| io::Error::from_raw_os_error(libc::EIO));
                if entry_only {
                    Err(BoundaryError::EntryNative(error))
                } else {
                    self.ended = true;
                    Err(BoundaryError::Native(error))
                }
            }
            None => {
                self.ended = true;
                Ok(None)
            }
        }
    }
}

pub fn fs_issue(
    error: BoundaryError,
    scope: crate::domain::model::IssueScope,
    operation: &str,
) -> crate::domain::model::FsIssue {
    let (code, native_code) = match error {
        BoundaryError::Revoked => ("SESSION_CLOSED", None),
        BoundaryError::ResourceLimit => ("RESOURCE_LIMIT", None),
        BoundaryError::ChangedObject => ("ENTRY_CHANGED", None),
        BoundaryError::MountBoundary => ("OUTSIDE_ROOT", None),
        BoundaryError::InvalidComponent => ("INVALID_ARGUMENT", None),
        BoundaryError::Native(e) | BoundaryError::EntryNative(e) => {
            let code = match e.raw_os_error() {
                Some(libc::EACCES | libc::EPERM) => "PERMISSION_DENIED",
                Some(libc::ENOENT) => "NOT_FOUND",
                Some(libc::ENOTDIR) => "NOT_DIRECTORY",
                Some(libc::ELOOP) => "LINK_TRAVERSAL_BLOCKED",
                _ => "IO_ERROR",
            };
            (code, e.raw_os_error())
        }
    };
    crate::domain::model::FsIssue {
        code: code.into(),
        operation: operation.into(),
        scope,
        entry_id: None,
        native_code,
    }
}
