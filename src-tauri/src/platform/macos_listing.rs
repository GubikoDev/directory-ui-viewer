//! Demand-driven native listing with budgeted, identity-checked reacquisition.
use super::macos::{ApprovedRoot, BoundaryError, ChildObservation, DirectoryCursor, fs_issue};
use crate::{
    domain::{
        controller::Cancellation,
        index::{DirectoryFingerprint, SharedDirectoryIndex},
        model::{Bytes, Entry, Field, FollowPolicy, FsIssue, IssueScope, Kind},
    },
    listing::service::{ListedEntry, ListingSource},
    runtime::{clock::Clock, memory::Reservation},
    scheduler::HandleBudget,
};
use std::sync::Arc;
const SOURCE_BYTES: usize = 8192;
fn issue(error: BoundaryError, scope: IssueScope) -> FsIssue {
    fs_issue(error, scope, "list")
}
fn changed() -> FsIssue {
    issue(BoundaryError::ChangedObject, IssueScope::Subtree)
}
struct Component {
    name: Vec<u8>,
    identity: DirectoryFingerprint,
}
pub struct NativeListingSource {
    pending: Option<ChildObservation>,
    root: Arc<ApprovedRoot>,
    index: SharedDirectoryIndex,
    root_id: String,
    clock: Arc<dyn Clock>,
    // Last field: owned strings/tokens are freed before reservation returns.
    memory: Reservation,
}
impl NativeListingSource {
    /// No I/O. The root ID must already be bound by trusted root setup or scan.
    pub fn new(
        root: Arc<ApprovedRoot>,
        index: SharedDirectoryIndex,
        root_id: String,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, FsIssue> {
        if root_id.len() > 256
            || index
                .key(&root_id)
                .map_err(|_| changed())?
                .parent_id
                .is_some()
        {
            return Err(changed());
        }
        let memory = root
            .directory()
            .work_budget()
            .reserve(SOURCE_BYTES)
            .map_err(|_| issue(BoundaryError::ResourceLimit, IssueScope::Subtree))?;
        Ok(Self {
            pending: None,
            root,
            index,
            root_id,
            clock,
            memory,
        })
    }
    fn resolve(&mut self, id: &str, cancel: &Cancellation) -> Result<DirectoryCursor, FsIssue> {
        let result = self.resolve_inner(id, cancel);
        // resolve_inner's component vector and temporary handles are now gone.
        self.memory
            .resize(SOURCE_BYTES)
            .expect("shrinking source reservation");
        result
    }
    fn resolve_inner(
        &mut self,
        id: &str,
        cancel: &Cancellation,
    ) -> Result<DirectoryCursor, FsIssue> {
        if id.len() > 256 {
            return Err(changed());
        }
        let mut path = Vec::new();
        let mut current = id.to_owned();
        let mut charged = SOURCE_BYTES;
        loop {
            if cancel.is_cancelled() {
                return Err(issue(BoundaryError::Revoked, IssueScope::Subtree));
            }
            let key = self.index.key(&current).map_err(|_| changed())?;
            let fingerprint = self
                .index
                .identity(&current)
                .map_err(|_| changed())?
                .ok_or_else(changed)?;
            if current == self.root_id {
                if key.parent_id.is_some()
                    || fingerprint != self.root.directory().identity().fingerprint()
                {
                    return Err(changed());
                }
                break;
            }
            let parent = key.parent_id.ok_or_else(changed)?;
            // The immutable index's parent links only point to older records;
            // this cannot cycle. Every stored component is charged before push.
            charged = charged
                .checked_add(256 + key.raw_name.capacity() + parent.capacity())
                .ok_or_else(|| issue(BoundaryError::ResourceLimit, IssueScope::Subtree))?;
            self.memory
                .resize(charged)
                .map_err(|_| issue(BoundaryError::ResourceLimit, IssueScope::Subtree))?;
            path.try_reserve_exact(1)
                .map_err(|_| issue(BoundaryError::ResourceLimit, IssueScope::Subtree))?;
            path.push(Component {
                name: key.raw_name,
                identity: fingerprint,
            });
            current = parent;
        }
        if cancel.is_cancelled() {
            return Err(issue(BoundaryError::Revoked, IssueScope::Subtree));
        }
        if path.is_empty() {
            return self
                .root
                .directory()
                .cursor()
                .map_err(|e| issue(e, IssueScope::Subtree));
        }
        let mut owned = None;
        for component in path.iter().rev() {
            if cancel.is_cancelled() {
                return Err(issue(BoundaryError::Revoked, IssueScope::Subtree));
            }
            let parent = owned.as_ref().unwrap_or(self.root.directory());
            let observed = parent
                .observe(&component.name)
                .map_err(|e| issue(e, IssueScope::Subtree))?;
            if cancel.is_cancelled() {
                return Err(issue(BoundaryError::Revoked, IssueScope::Subtree));
            }
            if observed.identity().fingerprint() != component.identity {
                return Err(changed());
            }
            let next = parent
                .open_observed(&observed)
                .map_err(|e| issue(e, IssueScope::Subtree))?;
            if cancel.is_cancelled() {
                return Err(issue(BoundaryError::Revoked, IssueScope::Subtree));
            }
            owned = Some(next);
        }
        owned
            .unwrap()
            .into_cursor()
            .map_err(|e| issue(e, IssueScope::Subtree))
    }
}
impl ListingSource for NativeListingSource {
    type Cursor = DirectoryCursor;
    fn handle_budget(&self) -> Option<&HandleBudget> {
        Some(self.root.directory().handle_budget())
    }
    fn open(
        &mut self,
        id: &str,
        index: &SharedDirectoryIndex,
        cancel: &Cancellation,
    ) -> Result<Self::Cursor, FsIssue> {
        if !self.index.same_index(index) {
            return Err(changed());
        }
        self.resolve(id, cancel)
    }
    fn bind_directory(
        &mut self,
        id: &str,
        row: &ListedEntry,
        index: &SharedDirectoryIndex,
    ) -> Result<(), FsIssue> {
        if !self.index.same_index(index) {
            return Err(changed());
        }
        let pending = self
            .pending
            .as_ref()
            .filter(|p| p.raw_name() == row.raw_name)
            .ok_or_else(changed)?;
        index
            .bind_identity(id, pending.identity().fingerprint())
            .map_err(|_| issue(BoundaryError::ChangedObject, IssueScope::Entry))
    }
    fn next(&mut self, cursor: &mut Self::Cursor) -> Result<Option<ListedEntry>, FsIssue> {
        self.pending = None;
        let Some(name) = cursor.next_name().map_err(|e| {
            let scope = if matches!(
                e,
                BoundaryError::EntryNative(_) | BoundaryError::InvalidComponent
            ) {
                IssueScope::Entry
            } else {
                IssueScope::Subtree
            };
            issue(e, scope)
        })?
        else {
            return Ok(None);
        };
        let stamp = self.clock.sample().observed_at;
        match cursor.directory().observe(&name) {
            Ok(observation) => {
                let entry = metadata_entry(&name, observation.metadata(), stamp);
                let metadata_error = if matches!(entry.own_logical_bytes, Field::Error { .. })
                    || matches!(entry.own_allocated_bytes, Field::Error { .. })
                {
                    Some(issue(
                        BoundaryError::Native(std::io::Error::from_raw_os_error(libc::EIO)),
                        IssueScope::Entry,
                    ))
                } else {
                    None
                };
                if entry.kind == Kind::Directory {
                    self.pending = Some(observation)
                }
                Ok(Some(ListedEntry {
                    raw_name: name,
                    entry,
                    issue: metadata_error,
                }))
            }
            Err(error) => {
                let error = issue(error, IssueScope::Entry);
                let entry = Entry {
                    entry_id: String::new(),
                    parent_id: None,
                    display_name: String::from_utf8_lossy(&name).into_owned(),
                    kind: Kind::Unknown,
                    hidden: if name.starts_with(b".") {
                        Field::Known { value: true }
                    } else {
                        Field::Unknown
                    },
                    special_type: Field::Unsupported,
                    modified_at: Field::Error {
                        issue: error.clone(),
                    },
                    own_logical_bytes: Field::Error {
                        issue: error.clone(),
                    },
                    own_allocated_bytes: Field::Error {
                        issue: error.clone(),
                    },
                    observed_at: stamp,
                    follow_policy: FollowPolicy::Never,
                };
                Ok(Some(ListedEntry {
                    raw_name: name,
                    entry,
                    issue: Some(error),
                }))
            }
        }
    }
}
pub fn metadata_entry(name: &[u8], stat: &libc::stat, observed_at: String) -> Entry {
    fn bytes(n: Option<u64>) -> Field<Bytes> {
        n.map(|n| Field::Known { value: n.into() })
            .unwrap_or_else(|| Field::Error {
                issue: issue(
                    BoundaryError::Native(std::io::Error::from_raw_os_error(libc::EIO)),
                    IssueScope::Entry,
                ),
            })
    }
    let modified = u32::try_from(stat.st_mtime_nsec)
        .ok()
        .and_then(|n| chrono::DateTime::from_timestamp(stat.st_mtime, n))
        .map(|t| Field::Known {
            value: t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        })
        .unwrap_or(Field::Unknown);
    Entry {
        entry_id: String::new(),
        parent_id: None,
        display_name: String::from_utf8_lossy(name).into_owned(),
        kind: match stat.st_mode & libc::S_IFMT {
            libc::S_IFDIR => Kind::Directory,
            libc::S_IFREG => Kind::RegularFile,
            libc::S_IFLNK => Kind::Symlink,
            _ => Kind::Other,
        },
        hidden: Field::Known {
            value: name.starts_with(b".") || stat.st_flags & libc::UF_HIDDEN != 0,
        },
        special_type: Field::Unsupported,
        modified_at: modified,
        own_logical_bytes: bytes(u64::try_from(stat.st_size).ok()),
        own_allocated_bytes: bytes(
            u64::try_from(stat.st_blocks)
                .ok()
                .and_then(|n| n.checked_mul(512)),
        ),
        observed_at,
        follow_policy: FollowPolicy::Never,
    }
}
