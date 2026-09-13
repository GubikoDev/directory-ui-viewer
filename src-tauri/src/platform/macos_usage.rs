//! Native DFS adapter. A single pending child capability observation replaces an
//! unbounded token-to-path map. All access stays relative to approved handles.
use super::macos::{ApprovedRoot, BoundaryError, ChildObservation, Directory, DirectoryCursor};
use crate::{
    domain::model::{FsIssue, IssueScope, Kind},
    runtime::memory::{ByteBudget, Reservation},
    scan::usage::{FileIdentity, Metric, Observation, ObservationSource},
    scheduler::HandleBudget,
};
use std::sync::Arc;
struct Pending {
    node: u64,
    parent: Arc<Directory>,
    child: ChildObservation,
}
pub struct NativeUsageSource {
    pending: Option<Pending>,
    root: Arc<ApprovedRoot>,
    next_node: u64,
    _memory: Reservation,
}
impl NativeUsageSource {
    /// No filesystem I/O. Reserve the source and its one pending raw component.
    pub fn new(root: Arc<ApprovedRoot>, work: &ByteBudget) -> Result<Self, FsIssue> {
        if !work.same_pool(root.directory().work_budget()) {
            return Err(issue(BoundaryError::InvalidComponent, IssueScope::Root));
        }
        let memory = work
            .reserve(4096)
            .map_err(|_| issue(BoundaryError::ResourceLimit, IssueScope::Root))?;
        Ok(Self {
            pending: None,
            root,
            next_node: 1,
            _memory: memory,
        })
    }
    /// Root identity and bytes are sampled by refresh_root on the fixed worker.
    pub fn root_request() -> Observation {
        Observation {
            node: 0,
            raw_name: vec![],
            kind: Kind::Directory,
            identity: None,
            link_count: 1,
            mount: 0,
            // No bytes observed yet. Failure/cancellation keeps these only
            // as partial zero, never as a completed root measurement.
            logical: Metric::Known(0),
            allocated: Metric::Known(0),
        }
    }
}
fn issue(error: BoundaryError, scope: IssueScope) -> FsIssue {
    super::macos::fs_issue(error, scope, "scan")
}

fn observation(node: u64, name: Vec<u8>, stat: &libc::stat) -> Observation {
    let kind = match stat.st_mode & libc::S_IFMT {
        libc::S_IFDIR => Kind::Directory,
        libc::S_IFREG => Kind::RegularFile,
        libc::S_IFLNK => Kind::Symlink,
        _ => Kind::Other,
    };
    Observation {
        node,
        raw_name: name,
        kind,
        identity: (stat.st_ino != 0).then_some(FileIdentity {
            device: stat.st_dev as u32 as u64,
            inode: stat.st_ino,
        }),
        link_count: stat.st_nlink as u64,
        // stat has no mount-point identity. Defer native mount exclusion to
        // nofollow open + fstatfs in open_observed; st_dev is insufficient.
        mount: 0,
        logical: u64::try_from(stat.st_size).map_or(Metric::Error, Metric::Known),
        allocated: u64::try_from(stat.st_blocks)
            .ok()
            .and_then(|n| n.checked_mul(512))
            .map_or(Metric::Error, Metric::Known),
    }
}
impl ObservationSource for NativeUsageSource {
    type Cursor = DirectoryCursor;
    fn bind_directory(
        &mut self,
        observed: &Observation,
        id: &str,
        index: &crate::domain::index::SharedDirectoryIndex,
    ) -> Result<(), FsIssue> {
        let fingerprint = if observed.node == 0 {
            self.root.directory().identity().fingerprint()
        } else {
            let pending = self
                .pending
                .as_ref()
                .filter(|p| p.node == observed.node && p.child.raw_name() == observed.raw_name)
                .ok_or_else(|| issue(BoundaryError::ChangedObject, IssueScope::Entry))?;
            pending.child.identity().fingerprint()
        };
        index.bind_identity(id, fingerprint).map_err(|_| {
            issue(
                BoundaryError::ChangedObject,
                if observed.node == 0 {
                    IssueScope::Root
                } else {
                    IssueScope::Entry
                },
            )
        })
    }
    fn handle_budget(&self) -> Option<&HandleBudget> {
        Some(self.root.directory().handle_budget())
    }
    fn refresh_root(&mut self, root: &Observation) -> Result<Observation, FsIssue> {
        if root.node != 0 || !root.raw_name.is_empty() {
            return Err(issue(BoundaryError::InvalidComponent, IssueScope::Root));
        }
        let stat = self
            .root
            .directory()
            .metadata()
            .map_err(|e| issue(e, IssueScope::Root))?;
        Ok(observation(0, vec![], &stat))
    }
    fn open(&mut self, directory: &Observation) -> Result<Self::Cursor, FsIssue> {
        if directory.node == 0 {
            if !directory.raw_name.is_empty() {
                return Err(issue(BoundaryError::InvalidComponent, IssueScope::Root));
            }
            return self
                .root
                .directory()
                .cursor()
                .map_err(|e| issue(e, IssueScope::Root));
        }
        let Some(pending) = self.pending.take() else {
            return Err(issue(BoundaryError::ChangedObject, IssueScope::Subtree));
        };
        if pending.node != directory.node || pending.child.raw_name() != directory.raw_name {
            return Err(issue(BoundaryError::ChangedObject, IssueScope::Subtree));
        }
        pending
            .parent
            .open_observed(&pending.child)
            .and_then(Directory::into_cursor)
            .map_err(|e| issue(e, IssueScope::Subtree))
    }
    fn next(&mut self, cursor: &mut Self::Cursor) -> Result<Option<Observation>, FsIssue> {
        // Previous child may have been excluded by the scanner's depth/budget
        // policy. Its token is now expired and cannot be used on another branch.
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
        let child = cursor
            .directory()
            .observe(&name)
            .map_err(|e| issue(e, IssueScope::Entry))?;
        let node = self.next_node;
        self.next_node = self
            .next_node
            .checked_add(1)
            .ok_or_else(|| issue(BoundaryError::ResourceLimit, IssueScope::Subtree))?;
        let observed = observation(node, name, child.metadata());
        if observed.kind == Kind::Directory {
            self.pending = Some(Pending {
                node,
                parent: cursor.parent_handle(),
                child,
            });
        }
        Ok(Some(observed))
    }
}
