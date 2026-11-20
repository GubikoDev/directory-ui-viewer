//! Shared managed-data reservations. These are not measurements of allocator RSS.
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourceLimit;
#[derive(Clone)]
pub struct ByteBudget(Arc<Counter>, bool);
struct Counter {
    used: AtomicUsize,
    peak: AtomicUsize,
    limit: usize,
    ordinary_limit: usize,
}
pub struct Reservation {
    budget: ByteBudget,
    bytes: usize,
}
/// A read result keeps its precharged allocation alive through serialization.
/// No mutable dereference or consuming extraction can detach the value from it.
pub struct Charged<T> {
    value: T,
    _memory: Reservation,
}
impl<T> Charged<T> {
    pub fn new(value: T, memory: Reservation) -> Self {
        Self {
            value,
            _memory: memory,
        }
    }
}
impl<T> std::ops::Deref for Charged<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.value
    }
}
impl<T: std::fmt::Debug> std::fmt::Debug for Charged<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.value.fmt(f)
    }
}
impl<T: serde::Serialize> serde::Serialize for Charged<T> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.value.serialize(serializer)
    }
}
impl ByteBudget {
    pub fn same_pool(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
    pub fn new(limit: usize) -> Self {
        Self::with_headroom(limit, 0)
    }
    /// Both admission classes share one counter and one absolute limit.
    pub fn with_headroom(limit: usize, headroom: usize) -> Self {
        assert!(headroom <= limit);
        Self(
            Arc::new(Counter {
                used: AtomicUsize::new(0),
                peak: AtomicUsize::new(0),
                limit,
                ordinary_limit: limit - headroom,
            }),
            false,
        )
    }
    pub(crate) fn control(&self) -> Self {
        Self(self.0.clone(), true)
    }
    fn ceiling(&self) -> usize {
        if self.1 {
            self.0.limit
        } else {
            self.0.ordinary_limit
        }
    }
    pub fn available(&self) -> usize {
        self.ceiling().saturating_sub(self.used())
    }
    pub fn limit(&self) -> usize {
        self.0.limit
    }
    pub fn used(&self) -> usize {
        self.0.used.load(Ordering::Acquire)
    }
    pub fn peak(&self) -> usize {
        self.0.peak.load(Ordering::Acquire)
    }
    pub fn reserve(&self, bytes: usize) -> Result<Reservation, ResourceLimit> {
        let mut reservation = Reservation {
            budget: self.clone(),
            bytes: 0,
        };
        reservation.resize(bytes)?;
        Ok(reservation)
    }
}
impl Reservation {
    pub fn bytes(&self) -> usize {
        self.bytes
    }
    pub fn resize(&mut self, bytes: usize) -> Result<(), ResourceLimit> {
        if bytes > self.bytes {
            let additional = bytes - self.bytes;
            let before = self
                .budget
                .0
                .used
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                    used.checked_add(additional)
                        .filter(|next| *next <= self.budget.ceiling())
                })
                .map_err(|_| ResourceLimit)?;
            self.budget
                .0
                .peak
                .fetch_max(before + additional, Ordering::Relaxed);
        } else {
            self.budget
                .0
                .used
                .fetch_sub(self.bytes - bytes, Ordering::AcqRel);
        }
        self.bytes = bytes;
        Ok(())
    }
}
impl Drop for Reservation {
    fn drop(&mut self) {
        self.budget.0.used.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}
#[derive(Clone)]
pub struct ManagedBudgets {
    pub background_index: ByteBudget,
    pub foreground_index: ByteBudget,
    pub listings: ByteBudget,
    pub hardlinks: ByteBudget,
    pub scan_workspace: ByteBudget,
    pub replay: ByteBudget,
    pub work: ByteBudget,
}
impl Default for ManagedBudgets {
    fn default() -> Self {
        const MIB: usize = 1024 * 1024;
        Self {
            background_index: ByteBudget::new(48 * MIB),
            foreground_index: ByteBudget::new(16 * MIB),
            listings: ByteBudget::new(38 * MIB),
            hardlinks: ByteBudget::new(12 * MIB),
            // Keep scanner history and replayable replies out of the budget used
            // to open a directory and deliver its next page.
            scan_workspace: ByteBudget::new(4 * MIB),
            replay: ByteBudget::with_headroom(4 * MIB, MIB),
            work: ByteBudget::with_headroom(6 * MIB, MIB),
        }
    }
}
impl ManagedBudgets {
    pub fn used(&self) -> usize {
        self.background_index.used()
            + self.foreground_index.used()
            + self.listings.used()
            + self.hardlinks.used()
            + self.scan_workspace.used()
            + self.replay.used()
            + self.work.used()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::Barrier, thread};
    #[test]
    fn control_headroom_shares_ownership_and_never_increases_total_limit() {
        let budget = ByteBudget::with_headroom(100, 20);
        let priority = budget.control();
        assert!(budget.same_pool(&priority));
        let mut data = budget.reserve(80).unwrap();
        assert!(budget.reserve(1).is_err());
        let mut control = priority.reserve(10).unwrap();
        control.resize(20).unwrap();
        assert_eq!(budget.used(), 100);
        assert_eq!(budget.peak(), 100);
        assert_eq!(budget.available(), 0);
        assert!(control.resize(21).is_err());
        data.resize(70).unwrap();
        assert!(data.resize(71).is_err());
        drop(control);
        assert_eq!(budget.available(), 10);
        drop(data);
        assert_eq!(budget.used(), 0);
    }
    #[test]
    fn charged_result_keeps_its_cost_through_serialization_and_value_destruction() {
        struct Value(ByteBudget);
        impl serde::Serialize for Value {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                assert_eq!(self.0.used(), 1024);
                serializer.serialize_str("live")
            }
        }
        impl Drop for Value {
            fn drop(&mut self) {
                assert_eq!(self.0.used(), 1024);
            }
        }
        let budget = ByteBudget::new(1024);
        let memory = budget.reserve(1024).unwrap();
        let reply = Charged::new(Value(budget.clone()), memory);
        assert_eq!(serde_json::to_string(&reply).unwrap(), "\"live\"");
        assert!(budget.reserve(1).is_err());
        drop(reply);
        assert_eq!(budget.used(), 0);
    }
    #[test]
    fn reservations_keep_their_old_size_on_failure_and_release_exactly_once() {
        let budget = ByteBudget::new(10);
        let mut first = budget.reserve(6).unwrap();
        let second = budget.reserve(4).unwrap();
        assert_eq!(first.resize(7), Err(ResourceLimit));
        assert_eq!(first.bytes(), 6);
        assert_eq!(budget.used(), 10);
        first.resize(3).unwrap();
        assert_eq!(budget.used(), 7);
        drop(second);
        assert_eq!(budget.used(), 3);
        first.resize(10).unwrap();
        assert_eq!(budget.used(), 10);
        assert_eq!(budget.peak(), 10);
        drop(first);
        assert_eq!(budget.used(), 0);
        assert!(budget.reserve(usize::MAX).is_err());
        assert_eq!(budget.used(), 0);
    }
    #[test]
    fn concurrent_generations_cannot_overbook_one_shared_pool() {
        let budget = ByteBudget::new(7);
        let acquired = Arc::new(Barrier::new(17));
        let release = Arc::new(Barrier::new(17));
        let workers: Vec<_> = (0..16)
            .map(|_| {
                let budget = budget.clone();
                let acquired = acquired.clone();
                let release = release.clone();
                thread::spawn(move || {
                    let reservation = budget.reserve(1);
                    acquired.wait();
                    release.wait();
                    drop(reservation);
                })
            })
            .collect();
        acquired.wait();
        assert_eq!(budget.used(), 7);
        assert_eq!(budget.peak(), 7);
        release.wait();
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(budget.used(), 0);
        let defaults = ManagedBudgets::default();
        assert_eq!(
            defaults.background_index.limit()
                + defaults.foreground_index.limit()
                + defaults.listings.limit()
                + defaults.hardlinks.limit()
                + defaults.scan_workspace.limit()
                + defaults.replay.limit()
                + defaults.work.limit(),
            128 * 1024 * 1024
        );
    }
}
