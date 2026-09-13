//! Shared managed-data reservations. These are not measurements of allocator RSS.
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourceLimit;
#[derive(Clone)]
pub struct ByteBudget(Arc<Counter>);
struct Counter {
    used: AtomicUsize,
    peak: AtomicUsize,
    limit: usize,
}
pub struct Reservation {
    budget: ByteBudget,
    bytes: usize,
}
impl ByteBudget {
    pub fn new(limit: usize) -> Self {
        Self(Arc::new(Counter {
            used: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            limit,
        }))
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
                        .filter(|next| *next <= self.budget.0.limit)
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
    pub work: ByteBudget,
}
impl Default for ManagedBudgets {
    fn default() -> Self {
        const MIB: usize = 1024 * 1024;
        Self {
            background_index: ByteBudget::new(48 * MIB),
            foreground_index: ByteBudget::new(16 * MIB),
            listings: ByteBudget::new(48 * MIB),
            hardlinks: ByteBudget::new(12 * MIB),
            work: ByteBudget::new(4 * MIB),
        }
    }
}
impl ManagedBudgets {
    pub fn used(&self) -> usize {
        self.background_index.used()
            + self.foreground_index.used()
            + self.listings.used()
            + self.hardlinks.used()
            + self.work.used()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::Barrier, thread};
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
                + defaults.work.limit(),
            128 * 1024 * 1024
        );
    }
}
