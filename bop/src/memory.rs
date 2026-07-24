//! Allocation-owned memory accounting for Bop execution.

#[cfg(all(feature = "no_std", not(feature = "std")))]
use alloc::rc::Rc;
#[cfg(any(feature = "std", not(feature = "no_std")))]
use std::rc::Rc;

use core::cell::{Cell, RefCell};

/// One sandbox's persistent memory account. Allocation receipts retain their
/// owner, so values outliving an operation remain charged to the instance that
/// created them and release that charge when their backing allocation drops.
#[doc(hidden)]
#[derive(Debug)]
pub struct MemoryAccount {
    used: Cell<usize>,
    limit: usize,
}

impl MemoryAccount {
    #[doc(hidden)]
    pub fn __new(limit: usize) -> Rc<Self> {
        Rc::new(Self {
            used: Cell::new(0),
            limit,
        })
    }

    fn alloc(&self, bytes: usize) {
        self.used.set(self.used.get().saturating_add(bytes));
    }

    fn dealloc(&self, bytes: usize) {
        self.used.set(self.used.get().saturating_sub(bytes));
    }

    #[doc(hidden)]
    pub fn __exceeded(&self) -> bool {
        self.used.get() > self.limit
    }

    #[doc(hidden)]
    pub fn __would_exceed(&self, bytes: usize) -> bool {
        self.used.get().saturating_add(bytes) > self.limit
    }

    #[doc(hidden)]
    pub fn __used(&self) -> usize {
        self.used.get()
    }

    #[cfg(test)]
    pub(crate) fn used(&self) -> usize {
        self.__used()
    }
}

#[cfg(any(feature = "std", not(feature = "no_std")))]
thread_local! {
    static ACTIVE: RefCell<Option<Rc<MemoryAccount>>> = const { RefCell::new(None) };
}

#[cfg(all(feature = "no_std", not(feature = "std")))]
struct SyncActive(RefCell<Option<Rc<MemoryAccount>>>);
#[cfg(all(feature = "no_std", not(feature = "std")))]
unsafe impl Sync for SyncActive {}
#[cfg(all(feature = "no_std", not(feature = "std")))]
static ACTIVE: SyncActive = SyncActive(RefCell::new(None));

fn replace_active(next: Option<Rc<MemoryAccount>>) -> Option<Rc<MemoryAccount>> {
    #[cfg(any(feature = "std", not(feature = "no_std")))]
    {
        ACTIVE.with(|active| core::mem::replace(&mut *active.borrow_mut(), next))
    }
    #[cfg(all(feature = "no_std", not(feature = "std")))]
    {
        core::mem::replace(&mut *ACTIVE.0.borrow_mut(), next)
    }
}

fn active() -> Option<Rc<MemoryAccount>> {
    #[cfg(any(feature = "std", not(feature = "no_std")))]
    {
        ACTIVE.with(|active| active.borrow().clone())
    }
    #[cfg(all(feature = "no_std", not(feature = "std")))]
    {
        ACTIVE.0.borrow().clone()
    }
}

/// Panic-safe active-account stack entry.
#[doc(hidden)]
pub struct ActiveMemoryGuard(Option<Rc<MemoryAccount>>);

impl ActiveMemoryGuard {
    #[doc(hidden)]
    pub fn __activate(account: &Rc<MemoryAccount>) -> Self {
        Self(replace_active(Some(Rc::clone(account))))
    }

    #[doc(hidden)]
    pub fn __suspend() -> Self {
        Self(replace_active(None))
    }

    #[doc(hidden)]
    pub fn __activate_new_if_none(limit: usize) -> Self {
        let next = active().or_else(|| Some(MemoryAccount::__new(limit)));
        Self(replace_active(next))
    }
}

impl Drop for ActiveMemoryGuard {
    fn drop(&mut self) {
        let previous = self.0.take();
        replace_active(previous);
    }
}

/// Owner receipt embedded in each tracked backing allocation.
#[derive(Debug)]
pub(crate) struct MemoryReceipt {
    owner: Option<Rc<MemoryAccount>>,
    bytes: usize,
}

impl MemoryReceipt {
    pub(crate) fn new(bytes: usize) -> Self {
        let owner = active();
        if let Some(account) = &owner {
            account.alloc(bytes);
        }
        Self { owner, bytes }
    }

    pub(crate) fn resize(&mut self, bytes: usize) {
        if let Some(account) = &self.owner {
            if bytes > self.bytes {
                account.alloc(bytes - self.bytes);
            } else {
                account.dealloc(self.bytes - bytes);
            }
        }
        self.bytes = bytes;
    }

    pub(crate) fn owner_matches_active(&self) -> bool {
        match (&self.owner, active()) {
            (None, None) => true,
            (Some(owner), Some(active)) => Rc::ptr_eq(owner, &active),
            _ => false,
        }
    }
}

impl Drop for MemoryReceipt {
    fn drop(&mut self) {
        if let Some(account) = &self.owner {
            account.dealloc(self.bytes);
        }
    }
}

/// Legacy one-shot compatibility: install a fresh account until another
/// one-shot run replaces it. Allocation receipts make later drops safe.
pub fn bop_memory_init(limit: usize) {
    replace_active(Some(MemoryAccount::__new(limit)));
}

/// Legacy mutation hook. New owned backings use internal allocation receipts
/// directly.
pub fn bop_alloc(bytes: usize) {
    if let Some(account) = active() {
        account.alloc(bytes);
    }
}

/// Legacy mutation hook paired with [`bop_alloc`].
pub fn bop_dealloc(bytes: usize) {
    if let Some(account) = active() {
        account.dealloc(bytes);
    }
}

pub fn bop_memory_exceeded() -> bool {
    active().is_some_and(|account| account.__exceeded())
}

pub fn bop_would_exceed(bytes: usize) -> bool {
    active().is_some_and(|account| account.__would_exceed(bytes))
}

#[cfg(test)]
pub(crate) fn bop_memory_used() -> usize {
    active().map_or(0, |account| account.used())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Value;
    #[cfg(all(feature = "no_std", not(feature = "std")))]
    use alloc::vec::Vec;
    #[cfg(any(feature = "std", not(feature = "no_std")))]
    use std::vec::Vec;

    #[test]
    fn value_layout_stays_compact_with_allocation_receipts() {
        assert_eq!(core::mem::size_of::<crate::value::BopStr>(), core::mem::size_of::<usize>());
        assert!(core::mem::size_of::<Value>() <= 32);
    }

    #[test]
    fn mutation_detaches_to_the_active_allocation_owner() {
        let external = {
            let _suspended = ActiveMemoryGuard::__suspend();
            Value::new_array(Vec::new())
        };
        let account = MemoryAccount::__new(1024 * 1024);
        let mut external = external;
        {
            let _active = ActiveMemoryGuard::__activate(&account);
            match &mut external {
                Value::Array(array) => array.try_push(Value::Int(1), 0).unwrap(),
                _ => unreachable!(),
            }
        }
        assert!(account.used() > 0);
        drop(external);
        assert_eq!(account.used(), 0);

        let first = MemoryAccount::__new(1024 * 1024);
        let mut value = {
            let _active = ActiveMemoryGuard::__activate(&first);
            Value::new_array(Vec::new())
        };
        let second = MemoryAccount::__new(1024 * 1024);
        {
            let _active = ActiveMemoryGuard::__activate(&second);
            match &mut value {
                Value::Array(array) => array.try_push(Value::Int(2), 0).unwrap(),
                _ => unreachable!(),
            }
        }
        assert_eq!(first.used(), 0);
        assert!(second.used() > 0);
    }

    #[cfg(all(feature = "std", feature = "no_std"))]
    #[test]
    fn unified_std_and_no_std_features_keep_memory_accounts_thread_local() {
        use std::sync::{Arc, Barrier};

        let ready = Arc::new(Barrier::new(2));
        let finish = Arc::new(Barrier::new(2));
        let handles = [17, 29].map(|bytes| {
            let ready = Arc::clone(&ready);
            let finish = Arc::clone(&finish);
            std::thread::spawn(move || {
                bop_memory_init(1024);
                ready.wait();
                bop_alloc(bytes);
                finish.wait();
                assert_eq!(bop_memory_used(), bytes);
                bop_dealloc(bytes);
                assert_eq!(bop_memory_used(), 0);
            })
        });

        for handle in handles {
            handle.join().expect("memory-account worker panicked");
        }
    }
}
