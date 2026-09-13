//! Object capabilities for trusted native code; none of these types deserialize.
//! Paths are accepted only for the native picker's initial candidate. Directory
//! reads and child acquisition subsequently use owned handles and raw components.
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
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

const O_RESOLVE_BENEATH: i32 = 0x00001000; // Installed macOS SDK sys/fcntl.h.
const ATTR_CMN_ERROR: u32 = 0x20000000; // Installed SDK sys/attr.h.
const O_NOFOLLOW_ANY: i32 = 0x20000000;
const FLAGS: i32 = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | O_NOFOLLOW_ANY;
// Fixed syscall buffer avoids libc fdopendir union-stack read-all allocation.
const BUFFER_BYTES: usize = 16 * 1024;
const HANDLE_BYTES: usize = 2048;

#[derive(Debug)]
pub enum BoundaryError {
    Native(io::Error),
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
    fd: OwnedFd,
    _permit: HandlePermit,
    _memory: Reservation,
    grant: Arc<Grant>,
    mount: Mount,
    identity: Identity,
    tag: Arc<ParentTag>,
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
    pub fn identity(&self) -> Identity {
        self.identity
    }
    pub fn metadata(&self) -> Result<libc::stat> {
        self.grant.check()?;
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
    pub fn into_cursor(mut self) -> Result<DirectoryCursor> {
        self.grant.check()?;
        self._memory
            .resize(HANDLE_BYTES + BUFFER_BYTES)
            .map_err(|_| BoundaryError::ResourceLimit)?;
        Ok(DirectoryCursor {
            directory: self,
            buffer: Box::new([0; BUFFER_BYTES / 8]),
            offset: 0,
            remaining: 0,
            ended: false,
        })
    }
}
/// Exclusively owned cursor with a fixed aligned buffer. No libc DIR caches.
pub struct DirectoryCursor {
    directory: Directory,
    buffer: Box<[u64; BUFFER_BYTES / 8]>,
    offset: usize,
    remaining: usize,
    ended: bool,
}
impl DirectoryCursor {
    pub fn directory(&self) -> &Directory {
        &self.directory
    }
    pub fn next_name(&mut self) -> Result<Option<Vec<u8>>> {
        loop {
            self.directory.grant.check()?;
            if self.remaining == 0 {
                if self.ended {
                    return Ok(None);
                }
                let mut attrs = libc::attrlist {
                    bitmapcount: libc::ATTR_BIT_MAP_COUNT as u16,
                    reserved: 0,
                    commonattr: libc::ATTR_CMN_NAME
                        | libc::ATTR_CMN_RETURNED_ATTRS
                        | ATTR_CMN_ERROR,
                    volattr: 0,
                    dirattr: 0,
                    fileattr: 0,
                    forkattr: 0,
                };
                self.buffer.fill(0);
                let count = unsafe {
                    libc::getattrlistbulk(
                        self.directory.fd.as_raw_fd(),
                        (&mut attrs as *mut libc::attrlist).cast(),
                        self.buffer.as_mut_ptr().cast(),
                        BUFFER_BYTES,
                        libc::FSOPT_PACK_INVAL_ATTRS as u64,
                    )
                };
                let error = io::Error::last_os_error();
                self.directory.grant.check()?;
                if count < 0 {
                    // A failed bulk read has no trustworthy continuation.
                    self.ended = true;
                    return Err(error.into());
                }
                self.remaining = count as usize;
                self.offset = 0;
                if count == 0 {
                    self.ended = true;
                    return Ok(None);
                }
            }
            let bytes = unsafe {
                std::slice::from_raw_parts(self.buffer.as_ptr().cast::<u8>(), BUFFER_BYTES)
            };
            let parsed = parse_name(bytes, self.offset);
            match parsed {
                Ok((length, name)) => {
                    self.offset += length;
                    self.remaining -= 1;
                    let name = name?;
                    if name == b"." || name == b".." {
                        continue;
                    }
                    // The record boundary is intact: reject this component only
                    // and allow later siblings. Malformed records end the cursor.
                    component(name)?;
                    return Ok(Some(name.to_vec()));
                }
                Err(error) => {
                    self.remaining = 0;
                    self.ended = true;
                    return Err(error);
                }
            }
        }
    }
}
// Packed output: uint32 length, attribute_set_t (5 uint32), error, name ref.
// Parse integers from slices so malformed offsets cannot cause pointer UB.
fn parse_name(bytes: &[u8], offset: usize) -> Result<(usize, Result<&[u8]>)> {
    fn bad() -> BoundaryError {
        BoundaryError::Native(io::Error::from_raw_os_error(libc::EIO))
    }
    fn word(bytes: &[u8], at: usize) -> Result<u32> {
        let raw = bytes
            .get(at..at.checked_add(4).ok_or_else(bad)?)
            .ok_or_else(bad)?;
        Ok(u32::from_ne_bytes(raw.try_into().map_err(|_| bad())?))
    }
    let record = bytes.get(offset..).ok_or_else(bad)?;
    let len = word(record, 0)? as usize;
    if len < 36 || len % 8 != 0 {
        return Err(bad());
    }
    let record = record.get(..len).ok_or_else(bad)?;
    let returned = word(record, 4)?;
    let error = word(record, 24)?;
    if returned & ATTR_CMN_ERROR != 0 && error != 0 {
        return Ok((len, Err(io::Error::from_raw_os_error(error as i32).into())));
    }
    if returned & libc::ATTR_CMN_NAME == 0 {
        return Err(bad());
    }
    let displacement = word(record, 28)? as i32;
    let name_len = word(record, 32)? as usize;
    let start = 28usize
        .checked_add_signed(displacement as isize)
        .ok_or_else(bad)?;
    let end = start.checked_add(name_len).ok_or_else(bad)?;
    if start < 36 || name_len < 1 {
        return Err(bad());
    }
    let name = record.get(start..end).ok_or_else(bad)?;
    if name.last() != Some(&0) {
        return Err(bad());
    }
    Ok((len, Ok(&name[..name_len - 1])))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parser_checks_record_name_offsets_and_error_rows() {
        fn put(b: &mut [u8], i: usize, v: u32) {
            b[i..i + 4].copy_from_slice(&v.to_ne_bytes())
        }
        let mut b = [0u8; 48];
        put(&mut b, 0, 48);
        put(&mut b, 4, libc::ATTR_CMN_NAME | ATTR_CMN_ERROR);
        put(&mut b, 28, 8);
        put(&mut b, 32, 5);
        b[36..41].copy_from_slice(b"test\0");
        assert_eq!(parse_name(&b, 0).unwrap().1.unwrap(), b"test");
        for (i, v) in [
            (0, 0),
            (0, 49),
            (0, 32),
            (28, u32::MAX),
            (28, 100),
            (32, 0),
            (32, 100),
            (4, 0),
        ] {
            let mut invalid = b;
            put(&mut invalid, i, v);
            assert!(parse_name(&invalid, 0).is_err());
        }
        let mut invalid = b;
        invalid[40] = 1;
        assert!(parse_name(&invalid, 0).is_err());
        for offset in [1, 47, 48, usize::MAX] {
            assert!(parse_name(&b, offset).is_err())
        }
        put(&mut b, 24, libc::EACCES as u32);
        assert!(
            matches!(parse_name(&b,0).unwrap().1,Err(BoundaryError::Native(e)) if e.raw_os_error()==Some(libc::EACCES))
        );
    }
}
