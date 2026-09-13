//! Iterative, resumable aggregation over a typed observation source. No OS paths.
use crate::domain::{
    controller::Cancellation,
    index::{DirectoryKey, Origin, SharedDirectoryIndex},
    model::{DirectoryUsage, FsIssue, Issues, Kind, Measure, Reason, ScanState},
};
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct FileIdentity {
    pub device: u64,
    pub inode: u64,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Metric {
    Known(u64),
    Unsupported,
    Error,
}
#[derive(Debug, Clone)]
pub struct Observation {
    /// Source-owned opaque node token. Never accepted from IPC as a path.
    pub node: u64,
    pub raw_name: Vec<u8>,
    pub kind: Kind,
    pub identity: Option<FileIdentity>,
    pub link_count: u64,
    pub mount: u64,
    pub logical: Metric,
    pub allocated: Metric,
}
fn source_error_reason(issue: &FsIssue) -> Reason {
    if issue.code == "RESOURCE_LIMIT" {
        Reason::ResourceLimit
    } else {
        Reason::ReadError
    }
}
pub trait ObservationSource {
    type Cursor;
    fn open(&mut self, directory: &Observation) -> Result<Self::Cursor, FsIssue>;
    fn next(&mut self, cursor: &mut Self::Cursor) -> Result<Option<Observation>, FsIssue>;
}
#[derive(Debug, Clone)]
pub struct ScanLimits {
    pub max_open_directories: usize,
    pub hardlink_bytes: usize,
    pub working_bytes: usize,
}
impl Default for ScanLimits {
    fn default() -> Self {
        Self {
            max_open_directories: 128,
            hardlink_bytes: 12 * 1024 * 1024,
            working_bytes: 4 * 1024 * 1024,
        }
    }
}
#[derive(Debug, Clone, Default)]
struct Sum {
    bytes: u128,
    known: bool,
    reasons: Vec<Reason>,
}
impl Sum {
    fn reason(&mut self, reason: Reason) {
        if !self.reasons.contains(&reason) {
            self.reasons.push(reason);
        }
    }
    fn add_number(&mut self, value: u128) {
        match self.bytes.checked_add(value) {
            Some(sum) => self.bytes = sum,
            None => self.reason(Reason::Overflow),
        }
    }
    fn add(&mut self, value: Metric) {
        match value {
            Metric::Known(value) => {
                self.known = true;
                self.add_number(value as u128);
            }
            Metric::Unsupported => self.reason(Reason::Unsupported),
            Metric::Error => self.reason(Reason::ReadError),
        }
    }
    fn merge(&mut self, other: &Self) {
        self.add_number(other.bytes);
        self.known |= other.known;
        for reason in &other.reasons {
            self.reason(*reason);
        }
    }
    fn measure(&self, running: bool) -> Measure {
        let mut reasons = self.reasons.clone();
        if running && !reasons.contains(&Reason::Scanning) {
            reasons.push(Reason::Scanning);
        }
        reasons.sort();
        if !self.known && !running && reasons == [Reason::Unsupported] {
            return Measure::Unavailable {
                reason: Reason::Unsupported,
            };
        }
        if reasons.is_empty() {
            Measure::Complete {
                bytes: self.bytes.into(),
            }
        } else {
            Measure::Partial {
                observed_bytes: self.bytes.into(),
                reasons,
            }
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SharedSize {
    logical: Metric,
    allocated: Metric,
}
#[derive(Default)]
struct Aggregate {
    logical: Sum,
    allocated: Sum,
    shared: BTreeMap<FileIdentity, SharedSize>,
}
const SHARED_BYTES: usize = 160; // BTree node, identity, two tagged metrics, allocator overhead
const VISITED_BYTES: usize = 96;
const FRAME_BYTES: usize = 2048;
impl Aggregate {
    fn reason(&mut self, reason: Reason) {
        self.logical.reason(reason);
        self.allocated.reason(reason);
    }
    fn add_own(&mut self, observation: &Observation) {
        self.logical.add(observation.logical);
        self.allocated.add(observation.allocated);
    }
    /// Shared contributions are included once in each subtree. On a conflicting
    /// repeated observation choose component-wise minima, making final totals
    /// independent of visit order while marking changedDuringScan.
    fn reconcile(sum: &mut Sum, previous: Metric, current: Metric) -> Metric {
        if previous == current {
            return previous;
        }
        let merged = match (previous, current) {
            (Metric::Known(a), Metric::Known(b)) => {
                sum.reason(Reason::ChangedDuringScan);
                Metric::Known(a.min(b))
            }
            (Metric::Known(value), Metric::Unsupported)
            | (Metric::Unsupported, Metric::Known(value)) => {
                sum.reason(Reason::Unsupported);
                Metric::Known(value)
            }
            (Metric::Known(value), Metric::Error) | (Metric::Error, Metric::Known(value)) => {
                sum.reason(Reason::ReadError);
                Metric::Known(value)
            }
            _ => {
                sum.reason(Reason::ReadError);
                sum.reason(Reason::Unsupported);
                Metric::Error
            }
        };
        if let Metric::Known(value) = previous {
            sum.bytes = sum.bytes.saturating_sub(value as u128);
        }
        sum.add(merged);
        merged
    }
    fn file(&mut self, observation: &Observation, shared_bytes: &mut usize, limit: usize) -> bool {
        if observation.link_count <= 1 {
            self.add_own(observation);
            return true;
        }
        let Some(identity) = observation.identity else {
            self.reason(Reason::Unsupported);
            return true;
        };
        let size = SharedSize {
            logical: observation.logical,
            allocated: observation.allocated,
        };
        if let Some(existing) = self.shared.get_mut(&identity) {
            existing.logical = Self::reconcile(&mut self.logical, existing.logical, size.logical);
            existing.allocated =
                Self::reconcile(&mut self.allocated, existing.allocated, size.allocated);
            return true;
        }
        if shared_bytes.saturating_add(SHARED_BYTES) > limit {
            self.reason(Reason::ResourceLimit);
            return false;
        }
        *shared_bytes += SHARED_BYTES;
        self.shared.insert(identity, size);
        self.add_own(observation);
        true
    }
    fn merge(&mut self, mut child: Self, shared_bytes: &mut usize) {
        // Merge ordinary and shared totals, then subtract duplicate shared
        // contributions. Moving map nodes avoids cloning all ancestor sets.
        self.logical.merge(&child.logical);
        self.allocated.merge(&child.allocated);
        for (identity, size) in std::mem::take(&mut child.shared) {
            if let Some(existing) = self.shared.get_mut(&identity) {
                if let Metric::Known(value) = size.logical {
                    self.logical.bytes = self.logical.bytes.saturating_sub(value as u128);
                }
                if let Metric::Known(value) = size.allocated {
                    self.allocated.bytes = self.allocated.bytes.saturating_sub(value as u128);
                }
                existing.logical =
                    Self::reconcile(&mut self.logical, existing.logical, size.logical);
                existing.allocated =
                    Self::reconcile(&mut self.allocated, existing.allocated, size.allocated);
                *shared_bytes -= SHARED_BYTES;
            } else {
                self.shared.insert(identity, size);
            }
        }
    }
}
struct Frame<C> {
    id: String,
    cursor: Option<C>,
    aggregate: Aggregate,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanPhase {
    Running,
    Completed,
    Cancelled,
}
#[derive(Debug, Clone, Default)]
pub struct ScanStats {
    pub processed_entries: u64,
    pub processed_directories: u64,
    pub peak_open_directories: usize,
    pub peak_hardlink_bytes: usize,
    pub peak_working_bytes: usize,
}
pub struct UsageScanner<S: ObservationSource> {
    source: S,
    stack: Vec<Frame<S::Cursor>>,
    visited: BTreeSet<FileIdentity>,
    pub index: SharedDirectoryIndex,
    root_id: String,
    root_mount: u64,
    root_open_failed: bool,
    limits: ScanLimits,
    cancellation: Cancellation,
    phase: ScanPhase,
    shared_bytes: usize,
    revision: u64,
    pub stats: ScanStats,
    pub issues: Issues,
}
impl<S: ObservationSource> UsageScanner<S> {
    pub fn new(
        mut source: S,
        root: Observation,
        index: impl Into<SharedDirectoryIndex>,
        limits: ScanLimits,
        cancellation: Cancellation,
    ) -> Result<Self, &'static str> {
        let index = index.into();
        if root.kind != Kind::Directory {
            return Err("NOT_DIRECTORY");
        }
        let root_id = index
            .register(
                DirectoryKey {
                    parent_id: None,
                    raw_name: root.raw_name.clone(),
                },
                Origin::Background,
            )
            .map_err(|_| "RESOURCE_LIMIT")?;
        let mut aggregate = Aggregate::default();
        aggregate.add_own(&root);
        let mut issues = Issues::default();
        let mut root_open_failed = false;
        let cursor = if cancellation.is_cancelled() {
            aggregate.reason(Reason::Cancelled);
            None
        } else if limits.max_open_directories == 0
            || limits.working_bytes < FRAME_BYTES + VISITED_BYTES
        {
            aggregate.reason(Reason::ResourceLimit);
            None
        } else {
            let opened = source.open(&root);
            if cancellation.is_cancelled() {
                aggregate.reason(Reason::Cancelled);
                None
            } else {
                match opened {
                    Ok(cursor) => Some(cursor),
                    Err(issue) => {
                        root_open_failed = issue.code != "RESOURCE_LIMIT";
                        aggregate.reason(source_error_reason(&issue));
                        issues.record(issue);
                        None
                    }
                }
            }
        };
        let mut visited = BTreeSet::new();
        if let Some(identity) = root.identity {
            visited.insert(identity);
        }
        let root_open = usize::from(cursor.is_some());
        Ok(Self {
            source,
            stack: vec![Frame {
                id: root_id.clone(),
                cursor,
                aggregate,
            }],
            visited,
            index,
            root_id,
            root_mount: root.mount,
            root_open_failed,
            limits,
            cancellation,
            phase: ScanPhase::Running,
            shared_bytes: 0,
            revision: 0,
            stats: ScanStats {
                processed_directories: 1,
                peak_open_directories: root_open,
                peak_working_bytes: FRAME_BYTES + VISITED_BYTES,
                ..ScanStats::default()
            },
            issues,
        })
    }
    pub fn root_id(&self) -> &str {
        &self.root_id
    }
    pub fn root_failed(&self) -> bool {
        self.root_open_failed
    }
    /// Finalize collected observations without performing any further source I/O.
    pub fn abort(&mut self, reason: Reason, observed_at: &str) {
        if self.phase == ScanPhase::Running {
            self.stop_partial(reason, observed_at);
        }
    }
    pub fn phase(&self) -> ScanPhase {
        self.phase
    }
    pub fn cancel(&self) {
        self.cancellation.cancel();
    }
    pub fn source(&self) -> &S {
        &self.source
    }
    fn publish(&mut self, frame: &Frame<S::Cursor>, running: bool, observed_at: &str) {
        self.revision += 1;
        let usage = DirectoryUsage {
            directory_id: frame.id.clone(),
            usage_revision: self.revision,
            scan_state: if running {
                ScanState::Running
            } else if self.phase == ScanPhase::Cancelled {
                ScanState::Cancelled
            } else if self.root_open_failed && frame.id == self.root_id {
                ScanState::Failed
            } else {
                ScanState::Settled
            },
            logical: frame.aggregate.logical.measure(running),
            allocated: frame.aggregate.allocated.measure(running),
            observed_at: observed_at.into(),
        };
        // Each registered directory reserves summary space. A protocol-sized
        // summary must fit; propagate a failed invariant instead of losing data.
        self.index
            .set_usage(usage)
            .expect("reserved directory summary capacity");
    }
    fn pop(&mut self, observed_at: &str) {
        let frame = self.stack.pop().unwrap();
        self.publish(&frame, false, observed_at);
        if let Some(parent) = self.stack.last_mut() {
            parent
                .aggregate
                .merge(frame.aggregate, &mut self.shared_bytes);
        } else {
            self.shared_bytes = 0;
            if self.phase != ScanPhase::Cancelled {
                self.phase = ScanPhase::Completed;
            }
        }
    }
    fn stop_partial(&mut self, reason: Reason, observed_at: &str) {
        if reason == Reason::Cancelled {
            self.phase = ScanPhase::Cancelled;
        }
        for frame in &mut self.stack {
            frame.aggregate.reason(reason);
        }
        while !self.stack.is_empty() {
            self.pop(observed_at);
        }
    }
    /// A scheduling quantum performs at most `items` observations; cancellation
    /// is checked before I/O and again immediately after every source call.
    pub fn step(&mut self, items: usize, observed_at: &str) -> ScanPhase {
        if self.phase != ScanPhase::Running {
            return self.phase;
        }
        let quantum_started = Instant::now();
        for iteration in 0..items {
            if iteration > 0 && quantum_started.elapsed() >= Duration::from_millis(20) {
                break;
            }
            if self.cancellation.is_cancelled() {
                self.stop_partial(Reason::Cancelled, observed_at);
                break;
            }
            let next = {
                let frame = self.stack.last_mut().unwrap();
                match frame.cursor.as_mut() {
                    Some(cursor) => self.source.next(cursor),
                    None => Ok(None),
                }
            };
            if self.cancellation.is_cancelled() {
                self.stop_partial(Reason::Cancelled, observed_at);
                break;
            }
            let observation = match next {
                Ok(Some(observation)) => observation,
                Ok(None) => {
                    self.pop(observed_at);
                    if self.stack.is_empty() {
                        break;
                    } else {
                        continue;
                    }
                }
                Err(issue) => {
                    let reason = source_error_reason(&issue);
                    self.issues.record(issue);
                    let frame = self.stack.last_mut().unwrap();
                    frame.aggregate.reason(reason);
                    frame.cursor = None;
                    continue;
                }
            };
            self.stats.processed_entries = self.stats.processed_entries.saturating_add(1);
            if observation.kind != Kind::Directory {
                let frame = self.stack.last_mut().unwrap();
                if !frame.aggregate.file(
                    &observation,
                    &mut self.shared_bytes,
                    self.limits.hardlink_bytes,
                ) {
                    self.stop_partial(Reason::ResourceLimit, observed_at);
                    break;
                }
                self.stats.peak_hardlink_bytes =
                    self.stats.peak_hardlink_bytes.max(self.shared_bytes);
                continue;
            }
            let parent_id = self.stack.last().unwrap().id.clone();
            let id = match self.index.register(
                DirectoryKey {
                    parent_id: Some(parent_id),
                    raw_name: observation.raw_name.clone(),
                },
                Origin::Background,
            ) {
                Ok(id) => id,
                Err(_) => {
                    self.stop_partial(Reason::ResourceLimit, observed_at);
                    break;
                }
            };
            let mut aggregate = Aggregate::default();
            aggregate.add_own(&observation);
            let excluded = observation.mount != self.root_mount
                || observation
                    .identity
                    .is_some_and(|identity| self.visited.contains(&identity));
            let working =
                (self.stack.len() + 1) * FRAME_BYTES + (self.visited.len() + 1) * VISITED_BYTES;
            let limited = self.stack.len() >= self.limits.max_open_directories
                || working > self.limits.working_bytes;
            if excluded || limited || observation.identity.is_none() {
                aggregate.reason(if excluded {
                    Reason::ExcludedByPolicy
                } else if limited {
                    Reason::ResourceLimit
                } else {
                    Reason::Unsupported
                });
                let frame = Frame {
                    id,
                    cursor: None,
                    aggregate,
                };
                self.publish(&frame, false, observed_at);
                self.stack
                    .last_mut()
                    .unwrap()
                    .aggregate
                    .merge(frame.aggregate, &mut self.shared_bytes);
                continue;
            }
            self.visited.insert(observation.identity.unwrap());
            let cursor = match self.source.open(&observation) {
                Ok(cursor) => Some(cursor),
                Err(issue) => {
                    aggregate.reason(source_error_reason(&issue));
                    self.issues.record(issue);
                    None
                }
            };
            if self.cancellation.is_cancelled() {
                aggregate.reason(Reason::Cancelled);
            }
            self.stack.push(Frame {
                id,
                cursor,
                aggregate,
            });
            self.stats.processed_directories = self.stats.processed_directories.saturating_add(1);
            self.stats.peak_open_directories = self.stats.peak_open_directories.max(
                self.stack
                    .iter()
                    .filter(|frame| frame.cursor.is_some())
                    .count(),
            );
            self.stats.peak_working_bytes = self.stats.peak_working_bytes.max(working);
        }
        if self.phase == ScanPhase::Running {
            for frame in &self.stack {
                self.revision += 1;
                let usage = DirectoryUsage {
                    directory_id: frame.id.clone(),
                    usage_revision: self.revision,
                    scan_state: ScanState::Running,
                    logical: frame.aggregate.logical.measure(true),
                    allocated: frame.aggregate.allocated.measure(true),
                    observed_at: observed_at.into(),
                };
                self.index
                    .set_usage(usage)
                    .expect("reserved directory summary capacity");
            }
        }
        self.phase
    }
    pub fn run(&mut self, observed_at: &str) -> ScanPhase {
        while self.phase == ScanPhase::Running {
            self.step(128, observed_at);
        }
        self.phase
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::index::DirectoryIndex;
    const STAMP: &str = "2026-09-13T00:00:00Z";
    #[derive(Default)]
    struct Source {
        children: BTreeMap<u64, Vec<Observation>>,
        denied: BTreeSet<u64>,
        opened: Vec<u64>,
        cancel_on_next: Option<Cancellation>,
    }
    struct Cursor {
        node: u64,
        offset: usize,
    }
    impl ObservationSource for Source {
        type Cursor = Cursor;
        fn open(&mut self, directory: &Observation) -> Result<Cursor, FsIssue> {
            self.opened.push(directory.node);
            if self.denied.contains(&directory.node) {
                return Err(FsIssue {
                    code: "PERMISSION_DENIED".into(),
                    operation: "open".into(),
                    scope: crate::domain::model::IssueScope::Entry,
                    entry_id: None,
                    native_code: Some(13),
                });
            }
            Ok(Cursor {
                node: directory.node,
                offset: 0,
            })
        }
        fn next(&mut self, cursor: &mut Cursor) -> Result<Option<Observation>, FsIssue> {
            if let Some(token) = &self.cancel_on_next {
                token.cancel();
            }
            let result = self
                .children
                .get(&cursor.node)
                .and_then(|children| children.get(cursor.offset))
                .cloned();
            cursor.offset += 1;
            Ok(result)
        }
    }
    fn node(id: u64, name: &str, kind: Kind, logical: u64, allocated: u64) -> Observation {
        Observation {
            node: id,
            raw_name: name.as_bytes().to_vec(),
            kind,
            identity: Some(FileIdentity {
                device: 1,
                inode: id,
            }),
            link_count: 1,
            mount: 1,
            logical: Metric::Known(logical),
            allocated: Metric::Known(allocated),
        }
    }
    fn scanner(source: Source, limits: ScanLimits) -> UsageScanner<Source> {
        UsageScanner::new(
            source,
            node(1, "root", Kind::Directory, 10, 20),
            DirectoryIndex::new("g1".into(), 48 * 1024 * 1024, 16 * 1024 * 1024),
            limits,
            Cancellation::default(),
        )
        .unwrap()
    }
    fn bytes(measure: &Measure) -> u128 {
        let value = serde_json::to_value(measure).unwrap();
        value
            .get("bytes")
            .or_else(|| value.get("observedBytes"))
            .unwrap()
            .as_str()
            .unwrap()
            .parse()
            .unwrap()
    }
    fn complete(scanner: &UsageScanner<Source>, id: &str) -> (u128, u128) {
        let usage = scanner.index.usage(id).unwrap().unwrap();
        assert!(matches!(usage.logical, Measure::Complete { .. }));
        assert!(matches!(usage.allocated, Measure::Complete { .. }));
        (bytes(&usage.logical), bytes(&usage.allocated))
    }
    fn id(scanner: &mut UsageScanner<Source>, parent: &str, name: &str) -> String {
        scanner
            .index
            .register(
                DirectoryKey {
                    parent_id: Some(parent.into()),
                    raw_name: name.as_bytes().to_vec(),
                },
                Origin::Background,
            )
            .unwrap()
    }
    #[test]
    fn includes_directory_own_bytes_hidden_empty_and_link_self_without_opening_target() {
        let mut source = Source::default();
        source.children.insert(
            1,
            vec![
                node(2, ".hidden", Kind::RegularFile, 9, 16),
                node(3, "empty", Kind::RegularFile, 0, 0),
                node(4, "external-link", Kind::Symlink, 12, 8),
                node(5, "package", Kind::Directory, 3, 4),
            ],
        );
        let mut scan = scanner(source, ScanLimits::default());
        scan.run(STAMP);
        assert_eq!(complete(&scan, scan.root_id()), (34, 48));
        assert_eq!(scan.source().opened, vec![1, 5]);
    }
    #[test]
    fn hardlinks_are_independent_in_siblings_and_unique_in_common_parent_in_both_orders() {
        for reverse in [false, true] {
            let mut source = Source::default();
            let b = node(2, "B", Kind::Directory, 2, 2);
            let c = node(3, "C", Kind::Directory, 3, 3);
            source
                .children
                .insert(1, if reverse { vec![c, b] } else { vec![b, c] });
            let mut shared = node(4, "shared", Kind::RegularFile, 100, 128);
            shared.link_count = 3;
            let mut alias = shared.clone();
            alias.node = 5;
            alias.raw_name = b"alias".to_vec();
            source.children.insert(2, vec![shared.clone(), alias]);
            source.children.insert(3, vec![shared]);
            let mut scan = scanner(source, ScanLimits::default());
            scan.run(STAMP);
            let root = scan.root_id().to_owned();
            let b = id(&mut scan, &root, "B");
            let c = id(&mut scan, &root, "C");
            assert_eq!(complete(&scan, &b), (102, 130));
            assert_eq!(complete(&scan, &c), (103, 131));
            assert_eq!(complete(&scan, &root), (115, 153));
            assert!(scan.stats.peak_hardlink_bytes <= 2 * SHARED_BYTES);
        }
    }
    #[test]
    fn unsupported_allocated_is_independent_and_permission_errors_preserve_other_branches() {
        let mut source = Source::default();
        let mut file = node(2, "f", Kind::RegularFile, 50, 0);
        file.allocated = Metric::Unsupported;
        source.children.insert(1, vec![file]);
        let mut scan = scanner(source, ScanLimits::default());
        scan.run(STAMP);
        let usage = scan.index.usage(scan.root_id()).unwrap().unwrap();
        assert_eq!(bytes(&usage.logical), 60);
        assert!(matches!(usage.logical, Measure::Complete { .. }));
        assert!(
            matches!(&usage.allocated,Measure::Partial{reasons,..} if reasons.contains(&Reason::Unsupported))
        );
        let mut source = Source::default();
        source.denied.insert(2);
        source.children.insert(
            1,
            vec![
                node(2, "denied", Kind::Directory, 2, 2),
                node(3, "healthy", Kind::Directory, 3, 3),
            ],
        );
        source
            .children
            .insert(3, vec![node(4, "file", Kind::RegularFile, 7, 8)]);
        let mut scan = scanner(source, ScanLimits::default());
        scan.run(STAMP);
        let usage = scan.index.usage(scan.root_id()).unwrap().unwrap();
        assert_eq!(bytes(&usage.logical), 22);
        assert!(
            matches!(&usage.logical,Measure::Partial{reasons,..} if reasons.contains(&Reason::ReadError))
        );
        assert_eq!(scan.issues.samples[0].native_code, Some(13));
        assert!(scan.source().opened.contains(&3));
    }
    #[test]
    fn same_device_different_mount_and_repeated_directory_are_excluded_without_open() {
        let mut source = Source::default();
        let mut other = node(2, "mount", Kind::Directory, 2, 2);
        other.mount = 2;
        let mut repeated = node(3, "repeat", Kind::Directory, 3, 3);
        repeated.identity = Some(FileIdentity {
            device: 1,
            inode: 1,
        });
        source.children.insert(1, vec![other, repeated]);
        let mut scan = scanner(source, ScanLimits::default());
        scan.run(STAMP);
        assert_eq!(scan.source().opened, vec![1]);
        assert!(
            matches!(&scan.index.usage(scan.root_id()).unwrap().unwrap().logical,Measure::Partial{reasons,..} if reasons.contains(&Reason::ExcludedByPolicy))
        );
    }
    #[test]
    fn cancellation_after_source_call_discards_that_result_and_publishes_partial() {
        let token = Cancellation::default();
        let mut source = Source {
            cancel_on_next: Some(token.clone()),
            ..Source::default()
        };
        source
            .children
            .insert(1, vec![node(2, "late", Kind::RegularFile, 1000, 1000)]);
        let mut scan = UsageScanner::new(
            source,
            node(1, "root", Kind::Directory, 10, 20),
            DirectoryIndex::new("g".into(), 100_000, 100_000),
            ScanLimits::default(),
            token,
        )
        .unwrap();
        assert_eq!(scan.run(STAMP), ScanPhase::Cancelled);
        let usage = scan.index.usage(scan.root_id()).unwrap().unwrap();
        assert_eq!(bytes(&usage.logical), 10);
        assert_eq!(usage.scan_state, ScanState::Cancelled);
        assert!(
            matches!(&usage.logical,Measure::Partial{reasons,..} if reasons.contains(&Reason::Cancelled))
        );
    }
    #[test]
    fn bounded_depth_and_hardlink_memory_return_explicit_partial_not_silent_undercount() {
        let mut source = Source::default();
        for n in 1..256 {
            source
                .children
                .insert(n, vec![node(n + 1, "child", Kind::Directory, 1, 1)]);
        }
        let mut scan = scanner(
            source,
            ScanLimits {
                max_open_directories: 8,
                ..ScanLimits::default()
            },
        );
        scan.run(STAMP);
        assert!(scan.stats.peak_open_directories <= 8);
        assert!(
            matches!(&scan.index.usage(scan.root_id()).unwrap().unwrap().logical,Measure::Partial{reasons,..} if reasons.contains(&Reason::ResourceLimit))
        );
        let mut source = Source::default();
        let mut shared = node(2, "shared", Kind::RegularFile, 999, 999);
        shared.link_count = 2;
        source.children.insert(1, vec![shared]);
        let mut scan = scanner(
            source,
            ScanLimits {
                hardlink_bytes: 0,
                ..ScanLimits::default()
            },
        );
        scan.run(STAMP);
        let usage = scan.index.usage(scan.root_id()).unwrap().unwrap();
        assert_eq!(bytes(&usage.logical), 10);
        assert!(
            matches!(&usage.logical,Measure::Partial{reasons,..} if reasons.contains(&Reason::ResourceLimit))
        );
    }
    #[test]
    fn shared_measurement_keeps_known_observation_when_another_link_is_unsupported() {
        for reverse in [false, true] {
            let mut source = Source::default();
            let mut known = node(2, "known", Kind::RegularFile, 100, 128);
            known.link_count = 2;
            let mut unknown = known.clone();
            unknown.raw_name = b"unknown".to_vec();
            unknown.allocated = Metric::Unsupported;
            source.children.insert(
                1,
                if reverse {
                    vec![unknown, known]
                } else {
                    vec![known, unknown]
                },
            );
            let mut scan = scanner(source, ScanLimits::default());
            scan.run(STAMP);
            let usage = scan.index.usage(scan.root_id()).unwrap().unwrap();
            assert_eq!(bytes(&usage.allocated), 148);
            assert!(
                matches!(&usage.allocated, Measure::Partial { reasons, .. } if reasons.contains(&Reason::Unsupported))
            );
            assert_eq!(bytes(&usage.logical), 110);
        }
    }
    #[test]
    fn pre_cancelled_and_zero_handle_budget_never_open_a_directory() {
        let token = Cancellation::default();
        token.cancel();
        let mut scan = UsageScanner::new(
            Source::default(),
            node(1, "root", Kind::Directory, 10, 20),
            DirectoryIndex::new("g".into(), 100_000, 100_000),
            ScanLimits::default(),
            token,
        )
        .unwrap();
        scan.run(STAMP);
        assert!(scan.source().opened.is_empty());
        assert_eq!(scan.stats.peak_open_directories, 0);
        let mut scan = scanner(
            Source::default(),
            ScanLimits {
                max_open_directories: 0,
                ..ScanLimits::default()
            },
        );
        scan.run(STAMP);
        assert!(scan.source().opened.is_empty());
    }
    #[test]
    fn arithmetic_overflow_remains_explicit() {
        let mut sum = Sum {
            bytes: u128::MAX,
            known: true,
            reasons: vec![],
        };
        sum.add_number(1);
        assert!(
            matches!(sum.measure(false), Measure::Partial { reasons, .. } if reasons.contains(&Reason::Overflow))
        );
    }
    #[test]
    fn step_publishes_partial_progress_and_large_sums_remain_exact() {
        let mut source = Source::default();
        source.children.insert(
            1,
            vec![
                node(2, "large", Kind::RegularFile, u64::MAX, u64::MAX),
                node(3, "larger", Kind::RegularFile, u64::MAX, u64::MAX),
            ],
        );
        let mut scan = scanner(source, ScanLimits::default());
        scan.step(1, STAMP);
        let usage = scan.index.usage(scan.root_id()).unwrap().unwrap();
        assert_eq!(usage.scan_state, ScanState::Running);
        assert!(
            matches!(&usage.logical,Measure::Partial{reasons,..} if reasons.contains(&Reason::Scanning))
        );
        scan.run(STAMP);
        assert_eq!(complete(&scan, scan.root_id()).0, u64::MAX as u128 * 2 + 10);
    }
}
