//! Demand-driven, bounded listing observations. Contains no filesystem I/O.
//! Snapshots and work records have one lifetime; native integration is gated by S01.
use crate::domain::model::{
    Category, Coverage, Entry, FsIssue, IssueScope, ListingPage, Phase, WorkRecord,
};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheError {
    TaskExpired,
    CursorExpired,
    InvalidArgument,
    ResourceLimit,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListingHandle {
    pub task_id: String,
    pub revision: String,
}
struct PublishedPage {
    end: usize,
    next_cursor: Option<String>,
}
struct Snapshot {
    work: WorkRecord,
    revision: String,
    category: Category,
    entries: Vec<Entry>,
    pages: BTreeMap<(usize, usize), PublishedPage>,
    page_limit: Option<usize>,
    cursors: BTreeMap<String, (usize, usize)>,
    last_used: u64,
    coverage: Coverage,
    exhausted: bool,
    bytes: usize,
}
/// Budget estimates deliberately overcount serialization, container storage, and
/// spare allocation capacity. They are managed-data budgets, not process RSS.
fn estimate<T: serde::Serialize>(value: &T) -> usize {
    serde_json::to_vec(value).map_or(usize::MAX, |json| {
        json.len()
            .saturating_mul(2)
            .saturating_add(std::mem::size_of::<T>() * 2 + 256)
    })
}
pub struct ListingCache {
    snapshots: BTreeMap<String, Snapshot>,
    serial: u64,
    used: usize,
    limit: usize,
    ttl_ms: u64,
    message_limit: usize,
}
impl ListingCache {
    pub fn new(limit: usize, ttl_ms: u64, message_limit: usize) -> Self {
        Self {
            snapshots: BTreeMap::new(),
            serial: 0,
            used: 0,
            limit,
            ttl_ms,
            message_limit,
        }
    }
    pub fn managed_bytes(&self) -> usize {
        self.used
    }
    pub fn len(&self) -> usize {
        self.snapshots.len()
    }
    pub fn is_empty(&self) -> bool {
        self.snapshots.is_empty()
    }
    pub fn expire(&mut self, now: u64) {
        let expired: Vec<_> = self
            .snapshots
            .iter()
            .filter(|(_, s)| now.saturating_sub(s.last_used) >= self.ttl_ms)
            .map(|(id, _)| id.clone())
            .collect();
        for id in expired {
            self.remove(&id);
        }
    }
    fn remove(&mut self, id: &str) {
        if let Some(snapshot) = self.snapshots.remove(id) {
            self.used -= snapshot.bytes;
        }
    }
    fn reserve(&mut self, bytes: usize, protected: Option<&str>) -> bool {
        if bytes > self.limit {
            return false;
        }
        while self.used.saturating_add(bytes) > self.limit {
            let oldest = self
                .snapshots
                .iter()
                .filter(|(id, _)| Some(id.as_str()) != protected)
                .min_by_key(|(_, s)| s.last_used)
                .map(|(id, _)| id.clone());
            let Some(oldest) = oldest else {
                return false;
            };
            self.remove(&oldest);
        }
        true
    }
    pub fn start(
        &mut self,
        mut work: WorkRecord,
        category: Category,
        now: u64,
    ) -> Result<ListingHandle, CacheError> {
        self.expire(now);
        if let Some(snapshot) = self.snapshots.values_mut().find(|s| {
            s.work.session_id == work.session_id
                && s.work.generation == work.generation
                && s.work.target_id == work.target_id
                && s.category == category
        }) {
            snapshot.last_used = now;
            return Ok(ListingHandle {
                task_id: snapshot.work.task_id.clone(),
                revision: snapshot.revision.clone(),
            });
        }
        if self.snapshots.contains_key(&work.task_id) {
            return Err(CacheError::InvalidArgument);
        }
        self.serial = self
            .serial
            .checked_add(1)
            .ok_or(CacheError::ResourceLimit)?;
        let revision = format!("listing-{}", self.serial);
        work.phase = Phase::Queued;
        let bytes = estimate(&work)
            .saturating_add(revision.capacity() * 2 + std::mem::size_of::<Snapshot>() * 2 + 4096);
        if !self.reserve(bytes, None) {
            return Err(CacheError::ResourceLimit);
        }
        let handle = ListingHandle {
            task_id: work.task_id.clone(),
            revision: revision.clone(),
        };
        self.used += bytes;
        self.snapshots.insert(
            work.task_id.clone(),
            Snapshot {
                work,
                revision,
                category,
                entries: vec![],
                pages: BTreeMap::new(),
                page_limit: None,
                cursors: BTreeMap::new(),
                last_used: now,
                coverage: Coverage::Partial,
                exhausted: false,
                bytes,
            },
        );
        Ok(handle)
    }
    /// The scheduler supplies at most one requested page plus one lookahead page.
    /// False means stop enumeration; previously collected pages remain readable.
    pub fn append(&mut self, task_id: &str, entry: Entry) -> Result<bool, CacheError> {
        let snapshot = self.snapshots.get(task_id).ok_or(CacheError::TaskExpired)?;
        if snapshot.exhausted {
            return Ok(false);
        }
        if entry.parent_id.as_deref() != Some(snapshot.work.target_id.as_str())
            || (entry.kind == crate::domain::model::Kind::Directory)
                != (snapshot.category == Category::Directories)
        {
            return Err(CacheError::InvalidArgument);
        }
        let bytes = estimate(&entry).saturating_add(1024); // reserve future page/cursor records before collection
        if !self.reserve(bytes, Some(task_id)) {
            self.finish(
                task_id,
                Phase::Completed,
                Some(FsIssue {
                    code: "RESOURCE_LIMIT".into(),
                    operation: "list".into(),
                    scope: IssueScope::Subtree,
                    entry_id: None,
                    native_code: None,
                }),
            )?;
            return Ok(false);
        }
        let snapshot = self.snapshots.get_mut(task_id).unwrap();
        snapshot.work.observed_at = entry.observed_at.clone();
        snapshot.entries.push(entry);
        snapshot.work.processed_entries = (snapshot.entries.len() as u64).into();
        if snapshot.category == Category::Directories {
            snapshot.work.processed_directories = (snapshot.entries.len() as u64).into();
        }
        snapshot.work.wait_reason = None;
        snapshot.bytes += bytes;
        self.used += bytes;
        snapshot.work.phase = Phase::Running;
        snapshot.work.sequence += 1;
        Ok(true)
    }
    pub fn finish(
        &mut self,
        task_id: &str,
        phase: Phase,
        issue: Option<FsIssue>,
    ) -> Result<(), CacheError> {
        if !phase.terminal() {
            return Err(CacheError::InvalidArgument);
        }
        if issue.as_ref().is_some_and(|issue| estimate(issue) > 2048) {
            return Err(CacheError::ResourceLimit);
        }
        let snapshot = self
            .snapshots
            .get_mut(task_id)
            .ok_or(CacheError::TaskExpired)?;
        if snapshot.exhausted {
            return Ok(());
        }
        snapshot.exhausted = true;
        snapshot.work.phase = phase;
        snapshot.work.sequence += 1;
        if let Some(issue) = issue {
            snapshot.work.issues.record(issue);
        }
        snapshot.coverage = if phase == Phase::Completed && snapshot.work.issues.counts.is_empty() {
            Coverage::Complete
        } else {
            Coverage::Partial
        };
        Ok(())
    }
    pub fn work(&mut self, task_id: &str, now: u64) -> Result<WorkRecord, CacheError> {
        self.expire(now);
        let snapshot = self
            .snapshots
            .get_mut(task_id)
            .ok_or(CacheError::TaskExpired)?;
        snapshot.last_used = now;
        Ok(snapshot.work.clone())
    }
    pub fn page(
        &mut self,
        task_id: &str,
        cursor: Option<&str>,
        limit: usize,
        now: u64,
    ) -> Result<ListingPage, CacheError> {
        if limit == 0 || limit > 1000 {
            return Err(CacheError::InvalidArgument);
        }
        self.expire(now);
        let snapshot = self.snapshots.get(task_id).ok_or(if cursor.is_some() {
            CacheError::CursorExpired
        } else {
            CacheError::TaskExpired
        })?;
        if snapshot
            .page_limit
            .is_some_and(|original| original != limit)
        {
            return Err(CacheError::InvalidArgument);
        }
        let offset = match cursor {
            None => 0,
            Some(cursor) => match snapshot.cursors.get(cursor) {
                Some((offset, original_limit)) if *original_limit == limit => *offset,
                _ => return Err(CacheError::CursorExpired),
            },
        };
        let published = snapshot.pages.get(&(offset, limit));
        let mut end = published.map_or((offset + limit).min(snapshot.entries.len()), |page| {
            page.end
        });
        let mut next_cursor = published.and_then(|page| page.next_cursor.clone());
        let needs_cursor =
            published.is_none() && (!snapshot.exhausted || end < snapshot.entries.len());
        if needs_cursor {
            next_cursor = snapshot
                .cursors
                .iter()
                .find(|(_, pair)| **pair == (end, limit))
                .map(|(cursor, _)| cursor.clone())
                .or_else(|| {
                    Some(format!(
                        "cursor-{}-{}-{}",
                        self.serial.saturating_add(1),
                        end,
                        limit
                    ))
                });
        }
        let mut page = ListingPage {
            work: snapshot.work.clone(),
            listing_revision: snapshot.revision.clone(),
            directory_id: snapshot.work.target_id.clone(),
            category: snapshot.category,
            entries: snapshot.entries[offset..end].to_vec(),
            cursor: cursor.map(str::to_owned),
            next_cursor,
            coverage: snapshot.coverage,
            issues: snapshot.work.issues.clone(),
        };
        // IPC limit includes every envelope field, not only the entry array.
        while serde_json::to_vec(&page)
            .map_err(|_| CacheError::InvalidArgument)?
            .len()
            .saturating_add(128 + 2048) // room for the bounded terminal issue added after publication
            > self.message_limit
        {
            if published.is_some() || page.entries.is_empty() {
                return Err(CacheError::ResourceLimit);
            }
            page.entries.pop();
            end -= 1;
            page.next_cursor = Some(format!(
                "cursor-{}-{}-{}",
                self.serial.saturating_add(1),
                end,
                limit
            ));
        }
        if end == offset && !snapshot.entries.is_empty() && offset < snapshot.entries.len() {
            return Err(CacheError::ResourceLimit);
        }
        let pending = end == offset && !snapshot.exhausted;
        let needs_record = published.is_none();
        // At most one limit per revision and one cursor per offset. Storage for
        // all future pages is precharged with entries, leaving partial results readable.
        let snapshot = self.snapshots.get_mut(task_id).unwrap();
        snapshot.last_used = now;
        snapshot.page_limit = Some(limit);
        if needs_record {
            self.serial = self
                .serial
                .checked_add(1)
                .ok_or(CacheError::ResourceLimit)?;
            if let Some(cursor) = &page.next_cursor {
                snapshot.cursors.insert(cursor.clone(), (end, limit));
            }
            // Empty running responses are not frozen pages. Reuse the cursor on
            // retry so repeated waiting cannot grow memory without bound.
            if !pending {
                snapshot.pages.insert(
                    (offset, limit),
                    PublishedPage {
                        end,
                        next_cursor: page.next_cursor.clone(),
                    },
                );
            }
        }
        Ok(page)
    }
    pub fn clear(&mut self) {
        self.snapshots.clear();
        self.used = 0;
    }
    pub fn retained_task_ids(&self) -> BTreeSet<&str> {
        self.snapshots.keys().map(String::as_str).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{Field, FollowPolicy, Issues, Kind, Operation};
    fn work(id: &str) -> WorkRecord {
        WorkRecord {
            session_id: "s".into(),
            generation: 1,
            task_id: id.into(),
            operation: Operation::ListDirectories,
            target_id: "A".into(),
            phase: Phase::Queued,
            wait_reason: None,
            sequence: 0,
            processed_entries: 0_u64.into(),
            processed_directories: 0_u64.into(),
            observed_at: "2026-09-13T00:00:00Z".into(),
            issues: Issues::default(),
        }
    }
    fn entry(id: &str) -> Entry {
        Entry {
            entry_id: id.into(),
            parent_id: Some("A".into()),
            display_name: id.into(),
            kind: Kind::Directory,
            hidden: Field::Unknown,
            special_type: Field::Unsupported,
            modified_at: Field::Unknown,
            own_logical_bytes: Field::Unknown,
            own_allocated_bytes: Field::Unknown,
            observed_at: "2026-09-13T00:00:00Z".into(),
            follow_policy: FollowPolicy::Never,
        }
    }
    #[test]
    fn empty_pending_and_complete_empty_are_distinct() {
        let mut cache = ListingCache::new(100_000, 100, 1_048_576);
        cache.start(work("t"), Category::Directories, 0).unwrap();
        let pending = cache.page("t", None, 200, 0).unwrap();
        assert!(pending.entries.is_empty());
        assert!(pending.next_cursor.is_some());
        assert_eq!(pending.coverage, Coverage::Partial);
        cache.finish("t", Phase::Completed, None).unwrap();
        let done = cache
            .page("t", pending.next_cursor.as_deref(), 200, 1)
            .unwrap();
        assert!(done.entries.is_empty());
        assert!(done.next_cursor.is_none());
        assert_eq!(done.coverage, Coverage::Complete);
    }
    #[test]
    fn pages_remain_immutable_and_wrong_cursor_limit_is_rejected() {
        let mut cache = ListingCache::new(100_000, 100, 1_048_576);
        cache.start(work("t"), Category::Directories, 0).unwrap();
        cache.append("t", entry("B")).unwrap();
        let first = cache.page("t", None, 1, 0).unwrap();
        cache.append("t", entry("C")).unwrap();
        cache.finish("t", Phase::Completed, None).unwrap();
        let replay = cache.page("t", None, 1, 1).unwrap();
        assert_eq!(replay.entries, first.entries);
        assert_eq!(replay.next_cursor, first.next_cursor);
        assert_eq!(
            cache.page("t", first.next_cursor.as_deref(), 2, 1),
            Err(CacheError::InvalidArgument)
        );
        let next = cache.page("t", first.next_cursor.as_deref(), 1, 1).unwrap();
        assert_eq!(next.entries[0].entry_id, "C");
        assert!(next.next_cursor.is_none());
    }
    #[test]
    fn snapshot_work_and_cursor_expire_together() {
        let mut cache = ListingCache::new(100_000, 100, 1_048_576);
        let old = cache.start(work("t"), Category::Directories, 0).unwrap();
        let page = cache.page("t", None, 1, 0).unwrap();
        assert_eq!(cache.work("t", 100), Err(CacheError::TaskExpired));
        assert_eq!(
            cache.page("t", page.next_cursor.as_deref(), 1, 100),
            Err(CacheError::CursorExpired)
        );
        let new = cache
            .start(work("new"), Category::Directories, 100)
            .unwrap();
        assert_ne!(old.revision, new.revision);
        assert_ne!(old.task_id, new.task_id);
    }
    #[test]
    fn waiting_cursor_retries_have_constant_storage() {
        let mut cache = ListingCache::new(100_000, 100_000, 1_048_576);
        cache.start(work("t"), Category::Directories, 0).unwrap();
        let first = cache.page("t", None, 200, 0).unwrap();
        let bytes = cache.managed_bytes();
        for now in 1..10_000 {
            let waiting = cache
                .page("t", first.next_cursor.as_deref(), 200, now)
                .unwrap();
            assert_eq!(waiting.next_cursor, first.next_cursor);
        }
        assert_eq!(cache.managed_bytes(), bytes);
        assert_eq!(cache.snapshots["t"].cursors.len(), 1);
        assert!(cache.snapshots["t"].pages.is_empty());
    }
    #[test]
    fn actual_budget_exhaustion_retains_all_collected_rows_and_terminal_record() {
        let mut cache = ListingCache::new(100_000, 100, 1_048_576);
        cache.start(work("t"), Category::Directories, 0).unwrap();
        let bytes = cache.managed_bytes();
        cache.limit = bytes + (estimate(&entry("B")) + 1024) * 2;
        assert!(cache.append("t", entry("B")).unwrap());
        assert!(cache.append("t", entry("C")).unwrap());
        assert!(!cache.append("t", entry("D")).unwrap());
        let first = cache.page("t", None, 1, 0).unwrap();
        let last = cache.page("t", first.next_cursor.as_deref(), 1, 0).unwrap();
        assert_eq!(first.entries[0].entry_id, "B");
        assert_eq!(last.entries[0].entry_id, "C");
        assert_eq!(last.coverage, Coverage::Partial);
        assert!(last.next_cursor.is_none());
        assert_eq!(cache.work("t", 0).unwrap().phase, Phase::Completed);
        assert!(cache.managed_bytes() <= cache.limit);
    }
    #[test]
    fn byte_pressure_evicts_snapshot_and_work_as_one_unit() {
        let mut cache = ListingCache::new(100_000, 100, 1_048_576);
        let first = cache.start(work("t1"), Category::Directories, 0).unwrap();
        cache.limit = cache.managed_bytes() + 10;
        let mut second = work("t2");
        second.target_id = "Z".into();
        cache.start(second, Category::Directories, 1).unwrap();
        assert_eq!(cache.work(&first.task_id, 1), Err(CacheError::TaskExpired));
        assert_eq!(cache.len(), 1);
    }
    #[test]
    fn collected_pages_survive_partial_finish() {
        let mut cache = ListingCache::new(100_000, 100, 1_048_576);
        cache.start(work("t"), Category::Directories, 0).unwrap();
        for name in ["B", "C", "D"] {
            cache.append("t", entry(name)).unwrap();
        }
        cache
            .finish(
                "t",
                Phase::Completed,
                Some(FsIssue {
                    code: "RESOURCE_LIMIT".into(),
                    operation: "list".into(),
                    scope: IssueScope::Subtree,
                    entry_id: None,
                    native_code: None,
                }),
            )
            .unwrap();
        let first = cache.page("t", None, 2, 0).unwrap();
        assert_eq!(first.entries.len(), 2);
        assert!(first.next_cursor.is_some());
        let last = cache.page("t", first.next_cursor.as_deref(), 2, 0).unwrap();
        assert_eq!(last.entries.len(), 1);
        assert!(last.next_cursor.is_none());
        assert_eq!(last.coverage, Coverage::Partial);
    }
}
