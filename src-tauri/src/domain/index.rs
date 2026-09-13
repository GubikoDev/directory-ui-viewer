//! Generation-owned directory index. Raw component names never cross the IPC boundary.
use super::controller::Scope;
use super::model::{Capacity, CapacityState, DirectoryUsage, Entry, Kind};
use crate::runtime::memory::{ByteBudget, Reservation};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Background,
    Foreground,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexError {
    ResourceLimit,
    EntryExpired,
    InvalidArgument,
    EntryChanged,
}
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct DirectoryKey {
    pub parent_id: Option<String>,
    pub raw_name: Vec<u8>,
}
struct Record {
    key: DirectoryKey,
    entry: Option<Entry>,
    usage: Option<DirectoryUsage>,
}
/// Summary allocation includes fixed room for two measurements, all partial
/// reasons, the bounded IDs/timestamps, and map nodes. Exposed entry metadata is
/// charged separately to the foreground reservation, including UTF-8 storage.
const SUMMARY_BYTES: usize = 2048;
fn entry_cost(entry: &Entry) -> usize {
    serde_json::to_vec(entry).map_or(usize::MAX, |v| {
        v.len()
            .saturating_mul(2)
            .saturating_add(std::mem::size_of::<Entry>() * 2 + 256)
    })
}
pub struct DirectoryIndex {
    records: BTreeMap<String, Record>,
    by_key: BTreeMap<DirectoryKey, String>,
    prefix: String,
    next: u64,
    background: Reservation,
    foreground: Reservation,
    capacity: Capacity,
    scope: Option<Scope>,
}
impl DirectoryIndex {
    pub fn new(prefix: String, background_limit: usize, foreground_limit: usize) -> Self {
        Self::with_budgets(
            prefix,
            ByteBudget::new(background_limit),
            ByteBudget::new(foreground_limit),
        )
    }
    pub fn with_budgets(prefix: String, background: ByteBudget, foreground: ByteBudget) -> Self {
        Self {
            records: BTreeMap::new(),
            by_key: BTreeMap::new(),
            prefix,
            next: 0,
            background: background.reserve(0).unwrap(),
            foreground: foreground.reserve(0).unwrap(),
            scope: None,
            capacity: Capacity {
                background: CapacityState::Available,
                foreground: CapacityState::Available,
            },
        }
    }
    pub fn bind_scope(&mut self, scope: &Scope) -> Result<(), IndexError> {
        if scope.session_id.is_empty()
            || scope.session_id.len() > 128
            || scope.generation == 0
            || scope.generation > 9_007_199_254_740_991
        {
            return Err(IndexError::InvalidArgument);
        }
        if let Some(current) = &self.scope {
            return if current == scope {
                Ok(())
            } else {
                Err(IndexError::InvalidArgument)
            };
        }
        self.charge(Origin::Foreground, 256 + scope.session_id.len() * 2)?;
        self.scope = Some(scope.clone());
        Ok(())
    }
    pub fn capacity(&self) -> Capacity {
        self.capacity.clone()
    }
    pub fn managed_bytes(&self) -> (usize, usize) {
        (self.background.bytes(), self.foreground.bytes())
    }
    pub fn len(&self) -> usize {
        self.records.len()
    }
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
    fn charge(&mut self, origin: Origin, amount: usize) -> Result<(), IndexError> {
        let (reservation, state) = match origin {
            Origin::Background => (&mut self.background, &mut self.capacity.background),
            Origin::Foreground => (&mut self.foreground, &mut self.capacity.foreground),
        };
        let target = reservation
            .bytes()
            .checked_add(amount)
            .ok_or(IndexError::ResourceLimit)?;
        if reservation.resize(target).is_err() {
            *state = CapacityState::Limited;
            return Err(IndexError::ResourceLimit);
        }
        Ok(())
    }
    pub fn register(&mut self, key: DirectoryKey, origin: Origin) -> Result<String, IndexError> {
        if let Some(id) = self.by_key.get(&key) {
            return Ok(id.clone());
        }
        if key.raw_name.len() > 255
            || key.raw_name.contains(&0)
            || key.raw_name.contains(&b'/')
            || key.raw_name == b"."
            || key.raw_name == b".."
            || self.prefix.len() > 128
        {
            return Err(IndexError::InvalidArgument);
        }
        if let Some(parent_id) = &key.parent_id {
            if key.raw_name.is_empty() || !self.records.contains_key(parent_id) {
                return Err(IndexError::EntryExpired);
            }
        } else if !self.records.is_empty() {
            return Err(IndexError::InvalidArgument);
        }
        let next = self.next.checked_add(1).ok_or(IndexError::ResourceLimit)?;
        let id = format!("{}-{}", self.prefix, next);
        let cost = SUMMARY_BYTES
            + key.raw_name.capacity() * 2
            + key.parent_id.as_ref().map_or(0, |s| s.capacity() * 2)
            + id.capacity() * 4;
        self.charge(origin, cost)?;
        self.next = next;
        self.by_key.insert(key.clone(), id.clone());
        self.records.insert(
            id.clone(),
            Record {
                key,
                entry: None,
                usage: None,
            },
        );
        Ok(id)
    }
    /// Exposes a previously scanned directory using the same ID. No summary copy
    /// or second summary charge. New listing revisions may replace metadata;
    /// published listing pages own their prior immutable observations.
    pub fn expose(&mut self, id: &str, entry: Entry) -> Result<(), IndexError> {
        let record = self.records.get(id).ok_or(IndexError::EntryExpired)?;
        if entry.entry_id != id
            || entry.kind != Kind::Directory
            || entry.parent_id != record.key.parent_id
        {
            return Err(IndexError::InvalidArgument);
        }
        if record.entry.as_ref() == Some(&entry) {
            return Ok(());
        }
        let previous = record.entry.as_ref().map_or(0, entry_cost);
        let next = entry_cost(&entry);
        if next > previous {
            self.charge(Origin::Foreground, next - previous)?;
        } else {
            self.foreground
                .resize(self.foreground.bytes() - (previous - next))
                .expect("shrinking a reservation");
        }
        self.records.get_mut(id).unwrap().entry = Some(entry);
        Ok(())
    }
    pub fn key(&self, id: &str) -> Result<&DirectoryKey, IndexError> {
        self.records
            .get(id)
            .map(|r| &r.key)
            .ok_or(IndexError::EntryExpired)
    }
    pub fn entry(&self, id: &str) -> Option<&Entry> {
        self.records.get(id).and_then(|r| r.entry.as_ref())
    }
    pub fn set_usage(&mut self, usage: DirectoryUsage) -> Result<bool, IndexError> {
        // All summary fields are bounded by protocol enums plus a decimal u128.
        // Reject unexpected oversized data instead of hiding it outside accounting.
        if serde_json::to_vec(&usage).map_or(true, |v| v.len() > SUMMARY_BYTES / 2) {
            return Err(IndexError::ResourceLimit);
        }
        let record = self
            .records
            .get_mut(&usage.directory_id)
            .ok_or(IndexError::EntryExpired)?;
        if record
            .usage
            .as_ref()
            .is_some_and(|old| old.usage_revision >= usage.usage_revision)
        {
            return Ok(false);
        }
        record.usage = Some(usage);
        Ok(true)
    }
    pub fn usage(&self, id: &str) -> Result<Option<&DirectoryUsage>, IndexError> {
        self.records
            .get(id)
            .map(|r| r.usage.as_ref())
            .ok_or(IndexError::EntryExpired)
    }
}
/// Shared generation index. Every method releases its lock before returning;
/// callers cannot accidentally hold this lock during filesystem I/O.
#[derive(Clone)]
pub struct SharedDirectoryIndex(Arc<Mutex<DirectoryIndex>>);
impl From<DirectoryIndex> for SharedDirectoryIndex {
    fn from(index: DirectoryIndex) -> Self {
        Self(Arc::new(Mutex::new(index)))
    }
}
impl SharedDirectoryIndex {
    pub fn bind_scope(&self, scope: &Scope) -> Result<(), IndexError> {
        self.0.lock().unwrap().bind_scope(scope)
    }
    pub fn register(&self, key: DirectoryKey, origin: Origin) -> Result<String, IndexError> {
        self.0.lock().unwrap().register(key, origin)
    }
    pub fn expose(&self, id: &str, entry: Entry) -> Result<(), IndexError> {
        self.0.lock().unwrap().expose(id, entry)
    }
    pub fn key(&self, id: &str) -> Result<DirectoryKey, IndexError> {
        self.0.lock().unwrap().key(id).cloned()
    }
    pub fn entry(&self, id: &str) -> Option<Entry> {
        self.0.lock().unwrap().entry(id).cloned()
    }
    pub fn usage(&self, id: &str) -> Result<Option<DirectoryUsage>, IndexError> {
        self.0.lock().unwrap().usage(id).map(|u| u.cloned())
    }
    pub fn set_usage(&self, usage: DirectoryUsage) -> Result<bool, IndexError> {
        self.0.lock().unwrap().set_usage(usage)
    }
    pub fn capacity(&self) -> Capacity {
        self.0.lock().unwrap().capacity()
    }
    pub fn managed_bytes(&self) -> (usize, usize) {
        self.0.lock().unwrap().managed_bytes()
    }
    pub fn len(&self) -> usize {
        self.0.lock().unwrap().len()
    }
    pub fn is_empty(&self) -> bool {
        self.0.lock().unwrap().is_empty()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{Field, FollowPolicy, Measure, ScanState};
    fn key(parent: Option<&str>, name: &[u8]) -> DirectoryKey {
        DirectoryKey {
            parent_id: parent.map(str::to_owned),
            raw_name: name.to_vec(),
        }
    }
    fn entry(id: &str, parent: Option<&str>) -> Entry {
        Entry {
            entry_id: id.into(),
            parent_id: parent.map(str::to_owned),
            display_name: "display only".into(),
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
    fn background_saturation_leaves_foreground_reservation_available() {
        let mut index = DirectoryIndex::new("g1".into(), 2400, 7000);
        let root = index
            .register(key(None, b"root"), Origin::Background)
            .unwrap();
        assert_eq!(
            index.register(key(Some(&root), b"background"), Origin::Background),
            Err(IndexError::ResourceLimit)
        );
        let child = index
            .register(key(Some(&root), b"foreground"), Origin::Foreground)
            .unwrap();
        index.expose(&child, entry(&child, Some(&root))).unwrap();
        assert_eq!(index.capacity().background, CapacityState::Limited);
        assert_eq!(index.capacity().foreground, CapacityState::Available);
        assert!(index.entry(&child).is_some());
    }
    #[test]
    fn foreground_saturation_keeps_existing_ids_and_cached_observations() {
        let mut index = DirectoryIndex::new("g1".into(), 5000, 4000);
        let root = index
            .register(key(None, b"root"), Origin::Background)
            .unwrap();
        index.expose(&root, entry(&root, None)).unwrap();
        loop {
            let name = format!("child-{}", index.len());
            if index
                .register(key(Some(&root), name.as_bytes()), Origin::Foreground)
                .is_err()
            {
                break;
            }
        }
        assert_eq!(index.capacity().foreground, CapacityState::Limited);
        assert!(index.entry(&root).is_some());
        assert_eq!(
            index
                .register(key(None, b"root"), Origin::Foreground)
                .unwrap(),
            root
        );
        assert!(index.managed_bytes().1 <= 4000);
    }
    #[test]
    fn raw_names_do_not_collapse_and_background_usage_survives_exposure() {
        let mut index = DirectoryIndex::new("g1".into(), 100_000, 100_000);
        let root = index
            .register(key(None, b"root"), Origin::Background)
            .unwrap();
        let a = index
            .register(key(Some(&root), &[0xff]), Origin::Background)
            .unwrap();
        let b = index
            .register(key(Some(&root), &[0xfe]), Origin::Background)
            .unwrap();
        assert_ne!(a, b);
        let usage = DirectoryUsage {
            directory_id: a.clone(),
            usage_revision: 1,
            scan_state: ScanState::Settled,
            logical: Measure::Complete {
                bytes: 9_u64.into(),
            },
            allocated: Measure::Unknown,
            observed_at: "2026-09-13T00:00:00Z".into(),
        };
        index.set_usage(usage.clone()).unwrap();
        let before = index.managed_bytes();
        assert_eq!(
            index
                .register(key(Some(&root), &[0xff]), Origin::Foreground)
                .unwrap(),
            a
        );
        index.expose(&a, entry(&a, Some(&root))).unwrap();
        assert_eq!(index.managed_bytes().0, before.0);
        let once = index.managed_bytes();
        index.expose(&a, entry(&a, Some(&root))).unwrap();
        assert_eq!(index.managed_bytes(), once);
        assert_eq!(index.usage(&a).unwrap(), Some(&usage));
    }
    #[test]
    fn invalid_components_and_other_generation_ids_are_rejected() {
        let mut index = DirectoryIndex::new("g2".into(), 10_000, 10_000);
        let root = index
            .register(key(None, b"root"), Origin::Background)
            .unwrap();
        for name in [b"..".as_slice(), b"a/b", b"a\0b"] {
            assert_eq!(
                index.register(key(Some(&root), name), Origin::Background),
                Err(IndexError::InvalidArgument)
            );
        }
        assert_eq!(index.key("g1-1"), Err(IndexError::EntryExpired));
    }
    #[test]
    fn an_old_generation_retains_its_shared_charge_until_its_last_owner_drops() {
        let background = ByteBudget::new(3000);
        let foreground = ByteBudget::new(7000);
        let mut old =
            DirectoryIndex::with_budgets("old".into(), background.clone(), foreground.clone());
        old.register(key(None, b"root"), Origin::Background)
            .unwrap();
        let old: SharedDirectoryIndex = old.into();
        let draining = old.clone();
        drop(old);
        let mut current =
            DirectoryIndex::with_budgets("new".into(), background.clone(), foreground.clone());
        assert_eq!(
            current.register(key(None, b"root"), Origin::Background),
            Err(IndexError::ResourceLimit)
        );
        assert!(background.used() > 0);
        drop(draining);
        assert_eq!(background.used(), 0);
        let root = current
            .register(key(None, b"root"), Origin::Background)
            .unwrap();
        current
            .register(key(Some(&root), b"child"), Origin::Foreground)
            .unwrap();
        assert_eq!(
            (background.used(), foreground.used()),
            current.managed_bytes()
        );
        drop(current);
        assert_eq!(background.used() + foreground.used(), 0);
    }
}
