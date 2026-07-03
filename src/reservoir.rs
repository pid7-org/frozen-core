//! Reservoir is a low latency, lock-free resource pool that provides exclusive, lease-based
//! access to managed resources
//!
//! ## Design
//!
//! By design, the `Reservoir` optimizes for the fast-path using a lock-free LIFO stack
//! (a.k.a. Treiber stack) managed via the atomic operations.
//!
//! While threads only interact with the OS-level blocking primitives (`Mutex`/`Condvar`) when the
//! pool is entirely exhausted.
//!
//! ## Benchmarks
//!
//! Observed measurements for acquire latency (both single and multi-threaded),
//!
//! | Metric | 1 Thread (ns) | 4 Threads (ns) |
//! |:-------|:--------------|:---------------|
//! | P50    |       18.0000 |       100.0000 |
//! | P90    |       18.0000 |       300.0000 |
//! | P99    |       20.0000 |       500.0000 |
//! | Mean   |       18.2808 |       202.3235 |
//!
//! _NOTE:_ Benchmarks run with a pool capacity of 1024. Multi-threaded test represents a high CAS
//! contention path.
//!
//! ## Example
//!
//! ```
//! use frozen_core::reservoir::Reservoir;
//! use std::sync::Arc;
//! use std::thread;
//!
//! let pool = Arc::new(Reservoir::new(vec![0x0A, 0x1A, 0x2A]));
//!
//! let mut permit = pool.acquire();
//! assert_eq!(*permit, 0x0A);
//!
//! *permit = 0x0F;
//! drop(permit);
//!
//! let pool_clone = Arc::clone(&pool);
//! let worker = thread::spawn(move || {
//!     let permit = pool_clone.acquire();
//!     assert_eq!(*permit, 0x0F);
//! });
//!
//! worker.join().unwrap();
//! ```

use std::{cell, mem, sync, sync::atomic};

const MAX_NODE: u32 = u32::MAX;

/// Reservoir is a low latency, lock-free resource pool that provides exclusive, lease-based
/// access to managed resources
///
/// ## Type `T`
///
/// The [`Reservoir`] expects the inner type `T` to be `Send`, `Sync` and `Sized` as it
/// mathematically guarantees exclusive mutable access to the yielded resource across thread
/// boundries.
///
/// ## Example
///
/// ```
/// use frozen_core::reservoir::Reservoir;
///
/// let pool = Reservoir::new(vec!["Conn1".to_string(), "Conn2".to_string()]);
///
/// let mut lease = pool.acquire();
/// assert_eq!(*lease, "Conn1");
///
/// lease.push_str("_used");
/// assert_eq!(*lease, "Conn1_used");
///
/// drop(lease);
/// ```
#[derive(Debug)]
pub struct Reservoir<T: Send + Sync + Sized> {
    cv: sync::Condvar,
    waiters: atomic::AtomicU32,
    nexts: Box<[atomic::AtomicU32]>,
    resources: Box<[cell::UnsafeCell<Option<T>>]>,
    lock: sync::Mutex<()>,
    head: atomic::AtomicU64,
    empty_head: atomic::AtomicU64,
}

// NOTE:
//
// The `UnsafeCell` implementation is `!Sync` by default, but our atomic state machines mathematically
// guarantee us the exclusive access to the underlying indices

unsafe impl<T: Send + Sync + Sized> Send for Reservoir<T> {}
unsafe impl<T: Send + Sync + Sized> Sync for Reservoir<T> {}

impl<T> Reservoir<T>
where
    T: Send + Sync + Sized,
{
    /// Creates a new `Reservoir` from a pre-allocated collection of resources of type [`T`]
    ///
    /// **WARNING:** The maximum supported capacity is `u32::MAX - 1`. Attempting to init a
    /// [`Reservoir`] with a vec that equals or exceeds this limit will result in panic.
    ///
    /// ## Uniqueness
    ///
    /// The caller is responsible for ensuring that each resource [`T`] appears at most once in the
    /// provided collection. Supplying the same logical resource multiple times (for example,
    /// multiple handles referring to the same underlying object) violates the reservoir's
    /// exclusivity guarantees and results in undefined application behavior.
    ///
    /// ## Example
    ///
    /// ```
    /// use frozen_core::reservoir::Reservoir;
    ///
    /// let pool = Reservoir::new(vec!["Conn1".to_string(), "Conn2".to_string()]);
    ///
    /// let mut lease = pool.acquire();
    /// assert_eq!(*lease, "Conn1");
    ///
    /// lease.push_str("_used");
    /// assert_eq!(*lease, "Conn1_used");
    ///
    /// drop(lease);
    /// ```
    pub fn new(resources: Vec<T>) -> Self {
        let len = resources.len();
        Self::with_capacity(resources, len)
    }

    /// Creates a new `Reservoir` from a pre-defined capacity and a pre-allocated collection of
    /// resources of type [`T`]
    ///
    /// **WARNING:** The maximum supported capacity is `u32::MAX - 1`. Attempting to init a
    /// [`Reservoir`] with a vec that equals or exceeds this limit will result in panic.
    ///
    /// ## Uniqueness
    ///
    /// The caller is responsible for ensuring that each resource [`T`] appears at most once in the
    /// provided collection. Supplying the same logical resource multiple times (for example,
    /// multiple handles referring to the same underlying object) violates the reservoir's
    /// exclusivity guarantees and results in undefined application behavior.
    ///
    /// ## Example
    ///
    /// ```
    /// use frozen_core::reservoir::Reservoir;
    ///
    /// let pool = Reservoir::with_capacity(vec!["Conn1".to_string(), "Conn2".to_string()], 0x0A);
    ///
    /// let mut lease = pool.acquire();
    /// assert_eq!(*lease, "Conn1");
    ///
    /// lease.push_str("_used");
    /// assert_eq!(*lease, "Conn1_used");
    ///
    /// drop(lease);
    /// ```
    pub fn with_capacity(resources: Vec<T>, capacity: usize) -> Self {
        // sanity checks
        assert!(capacity > 0, "Reservoir capacity must be greater than 0");
        assert!(capacity >= resources.len(), "Capacity must be >= initial resources length");
        assert!(capacity < MAX_NODE as usize, "Resources must not exceed `u32::MAX` length");

        let initial_len = resources.len();

        let mut nexts = Vec::with_capacity(capacity);
        let mut res_cells = Vec::with_capacity(capacity);

        for (idx, res) in resources.into_iter().enumerate() {
            res_cells.push(std::cell::UnsafeCell::new(Some(res)));
            nexts.push(atomic::AtomicU32::new(if idx + 1 == initial_len {
                MAX_NODE
            } else {
                (idx + 1) as u32
            }));
        }

        let empty_head = if initial_len < capacity {
            for idx in initial_len..capacity {
                res_cells.push(std::cell::UnsafeCell::new(None));
                nexts.push(atomic::AtomicU32::new(if idx + 1 == capacity {
                    MAX_NODE
                } else {
                    (idx + 1) as u32
                }));
            }
            pack(initial_len as u32, 0)
        } else {
            pack(MAX_NODE, 0)
        };

        let head = if initial_len > 0 { pack(0, 0) } else { pack(MAX_NODE, 0) };

        Self {
            cv: sync::Condvar::new(),
            head: atomic::AtomicU64::new(head),
            empty_head: atomic::AtomicU64::new(empty_head),
            lock: sync::Mutex::new(()),
            nexts: nexts.into_boxed_slice(),
            resources: res_cells.into_boxed_slice(),
            waiters: atomic::AtomicU32::new(0),
        }
    }

    /// Acquires a resource of type [`T`] from the reservoir, with internal blocking when the pool
    /// is exhausted
    ///
    /// ## Working
    ///
    /// This method first attempts a lock-free fast-path acquisition. If no resources are available,
    /// the calling thread is registered as a waiter and safely parked using a [`std::sync::Condvar`]
    /// until another caller drops its permit.
    ///
    /// ## Example
    ///
    /// ```
    /// use frozen_core::reservoir::Reservoir;
    ///
    /// let pool = Reservoir::new(vec!["hello".to_string()]);
    ///
    /// {
    ///     let mut permit = pool.acquire();
    ///     permit.push_str(" world");
    /// }
    ///
    /// let permit = pool.acquire();
    /// assert_eq!(*permit, "hello world");
    /// ```
    #[inline(always)]
    pub fn acquire(&self) -> ReservoirPermit<'_, T> {
        if let Some(index) = self.try_pop_stack(&self.head) {
            return ReservoirPermit { reservoir: self, index: index as usize };
        }

        self.waiters.fetch_add(1, atomic::Ordering::SeqCst);

        let mut guard = self.lock.lock().unwrap_or_else(|err| err.into_inner());
        loop {
            if let Some(index) = self.try_pop_stack(&self.head) {
                self.waiters.fetch_sub(1, atomic::Ordering::SeqCst);
                return ReservoirPermit { reservoir: self, index: index as usize };
            }

            guard = self.cv.wait(guard).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Permanently retire a resource [`T`] using its [`ReservoirPermit`]
    ///
    /// ## Example
    ///
    /// ```
    /// use frozen_core::reservoir::Reservoir;
    ///
    /// let pool = Reservoir::new(vec!["hello".to_string(), "world".to_string()]);
    ///
    /// let permit = pool.acquire();
    /// assert_eq!(pool.retire(permit), "hello");
    ///
    /// let permit = pool.acquire();
    /// assert_eq!(*permit, "world");
    /// ```
    #[inline]
    pub fn retire(&self, permit: ReservoirPermit<'_, T>) -> T {
        let index = permit.index;

        // NOTE: To avoid the automatic drop by the compiler, we must manually forget the `permit`
        mem::forget(permit);

        // SAFETY: As we have exclusive access to `T`, the use of unsafe is perfectly sound
        let resource = unsafe {
            let opt_ref = &mut *self.resources[index].get();
            opt_ref.take().expect("Permit held an empty slot (this is a bug in Reservoir)")
        };

        // NOTE: we reclaim the space back to be used by another `T`
        self.push_stack(&self.empty_head, index as u32);

        resource
    }

    #[inline]
    fn push(&self, index: u32) {
        loop {
            let current_head = self.head.load(atomic::Ordering::Acquire);
            let (head, version) = unpack(current_head);
            self.nexts[index as usize].store(head, atomic::Ordering::Relaxed);
            if self
                .head
                .compare_exchange_weak(
                    current_head,
                    pack(index, version.wrapping_add(1)),
                    atomic::Ordering::AcqRel,
                    atomic::Ordering::Acquire,
                )
                .is_ok()
            {
                return;
            }
        }
    }

    #[inline]
    fn push_stack(&self, stack: &atomic::AtomicU64, index: u32) {
        loop {
            let current = stack.load(atomic::Ordering::Acquire);
            let (head_idx, version) = unpack(current);
            self.nexts[index as usize].store(head_idx, atomic::Ordering::Relaxed);
            if stack
                .compare_exchange_weak(
                    current,
                    pack(index, version.wrapping_add(1)),
                    atomic::Ordering::AcqRel,
                    atomic::Ordering::Acquire,
                )
                .is_ok()
            {
                return;
            }
        }
    }

    #[inline]
    fn release(&self, index: u32) {
        self.push(index);
        if self.waiters.load(atomic::Ordering::SeqCst) > 0 {
            let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
            self.cv.notify_one();
        }
    }

    #[inline]
    fn try_pop_stack(&self, stack: &atomic::AtomicU64) -> Option<u32> {
        loop {
            let current = stack.load(atomic::Ordering::Acquire);
            let (head_idx, version) = unpack(current);

            if head_idx == MAX_NODE {
                return None;
            }

            let next = self.nexts[head_idx as usize].load(atomic::Ordering::Acquire);
            if stack
                .compare_exchange_weak(
                    current,
                    pack(next, version.wrapping_add(1)),
                    atomic::Ordering::AcqRel,
                    atomic::Ordering::Acquire,
                )
                .is_ok()
            {
                return Some(head_idx);
            }
        }
    }
}

/// An RAII guard representing exclusive access to a resource from the [`Reservoir`]
///
/// ## Note
///
/// The [`ReservoirPermit`] implements `Deref` and `DerefMut`. The caller can use the permit
/// exactly as if it were a `&T` or `&mut T`. When the permit is dropped, the resource index is
/// automatically returned to the pool for reuse.
///
/// ## Example
///
/// ```
/// use frozen_core::reservoir::Reservoir;
///
/// let pool = Reservoir::new(vec![String::from("initial")]);
///
/// {
///     let mut permit = pool.acquire();
///     assert_eq!(permit.len(), 7);
///     
///     permit.push_str(" state");
///     assert_eq!(*permit, "initial state");
/// }
///
/// let permit2 = pool.acquire();
/// assert_eq!(*permit2, "initial state");
/// ```
#[derive(Debug)]
pub struct ReservoirPermit<'a, T: Send + Sync + Sized> {
    reservoir: &'a Reservoir<T>,
    index: usize,
}

impl<'a, T> Drop for ReservoirPermit<'a, T>
where
    T: Send + Sync + Sized,
{
    fn drop(&mut self) {
        self.reservoir.release(self.index as u32);
    }
}

impl<'a, T> std::ops::Deref for ReservoirPermit<'a, T>
where
    T: Send + Sync + Sized,
{
    type Target = T;

    fn deref(&self) -> &Self::Target {
        // SAFETY: As we have exclusive access to `T` via the `ReservoirPermit`, the use of unsafe
        // is perfectly sound
        unsafe {
            let opt = &*self.reservoir.resources[self.index].get();
            opt.as_ref().unwrap()
        }
    }
}

impl<'a, T> std::ops::DerefMut for ReservoirPermit<'a, T>
where
    T: Send + Sync + Sized,
{
    fn deref_mut(&mut self) -> &mut Self::Target {
        // SAFETY: As we have exclusive access to `T` via the `ReservoirPermit`, the use of unsafe
        // is perfectly sound
        unsafe {
            let opt = &mut *self.reservoir.resources[self.index].get();
            opt.as_mut().unwrap()
        }
    }
}

#[inline]
fn pack(index: u32, version: u32) -> u64 {
    (version as u64) << 0x20 | index as u64
}

#[inline]
fn unpack(value: u64) -> (u32, u32) {
    (value as u32, (value >> 0x20) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn ok_create_and_basic_acquire() {
        let pool = Reservoir::new(vec![0x0A, 0x1A, 0x2A]);

        let permit = pool.acquire();
        assert_eq!(*permit, 0x0A);
    }

    mod deref {
        use super::*;

        #[test]
        fn ok_deref_and_deref_mut_coercion() {
            let pool = Reservoir::new(vec![String::from("hello")]);

            {
                let mut permit = pool.acquire();
                assert_eq!(permit.len(), 5);

                permit.push_str(" world");
            }

            let permit = pool.acquire();
            assert_eq!(*permit, "hello world");
        }
    }

    #[test]
    fn ok_exhaustion_and_sequential_reuse() {
        let pool = Reservoir::new(vec![1, 2]);

        let p1 = pool.acquire();
        let p2 = pool.acquire();
        drop(p1);

        let p3 = pool.acquire();
        assert_eq!(*p3, 1);
        assert_eq!(*p2, 2);
    }

    #[test]
    fn ok_acquire_blocks_until_notified() {
        let pool = Arc::new(Reservoir::new(vec![0x3C]));
        let permit = pool.acquire();

        let pool_clone = Arc::clone(&pool);
        let worker = thread::spawn(move || {
            let p = pool_clone.acquire();
            assert_eq!(*p, 0x3C);
        });

        thread::sleep(Duration::from_millis(0x32));
        drop(permit);

        worker.join().expect("Worker thread panicked");
    }

    #[test]
    fn ok_concurrent_stress_test() {
        const CAPACITY: usize = 0x0A;
        const THREADS: usize = 0x32;
        const ITERATIONS: usize = 0x64;

        let pool = Arc::new(Reservoir::new(vec![0; CAPACITY]));
        let mut handles = Vec::new();

        for _ in 0..THREADS {
            let pool_clone = Arc::clone(&pool);
            handles.push(thread::spawn(move || {
                for _ in 0..ITERATIONS {
                    let mut permit = pool_clone.acquire();
                    *permit += 1;

                    thread::yield_now();
                }
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }

        let mut total_sum = 0;
        let mut _held_permits = Vec::with_capacity(CAPACITY);

        for _ in 0..CAPACITY {
            let permit = pool.acquire();
            total_sum += *permit;
            _held_permits.push(permit);
        }

        assert_eq!(total_sum, THREADS * ITERATIONS);
    }

    #[test]
    #[should_panic]
    fn err_zero_capacity_panics_on_acquire() {
        let pool: Reservoir<u32> = Reservoir::new(vec![]);
        let _permit = pool.acquire();
    }

    #[test]
    #[should_panic]
    fn err_capacity_exceeds_max_node() {
        let massive_vec: Vec<()> = vec![(); MAX_NODE as usize];
        let _pool = Reservoir::new(massive_vec);
    }

    #[test]
    fn ok_multiple_waiters_wake_sequentially() {
        let pool = Arc::new(Reservoir::new(vec![1, 2]));

        let p1 = pool.acquire();
        let p2 = pool.acquire();

        let mut handles = Vec::new();
        for _ in 0..3 {
            let pool_clone = Arc::clone(&pool);
            handles.push(thread::spawn(move || {
                let _permit = pool_clone.acquire();
                thread::sleep(Duration::from_millis(0x0A));
            }));
        }

        thread::sleep(Duration::from_millis(0x32));

        drop(p1);
        drop(p2);

        for handle in handles {
            handle.join().expect("Thread panicked or deadlocked");
        }

        assert_eq!(pool.resources.len(), 2);
    }

    #[test]
    fn ok_permit_dropped_on_thread_panic() {
        let pool = Arc::new(Reservoir::new(vec![0x63]));

        let pool_clone = Arc::clone(&pool);
        let result = thread::spawn(move || {
            let _permit = pool_clone.acquire();
            panic!("Intentional crash!");
        })
        .join();

        assert!(result.is_err());

        let (tx, rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let permit = pool.acquire();
            tx.send(*permit).unwrap();
        });

        let recovered_value = rx
            .recv_timeout(Duration::from_millis(0x64))
            .expect("Permit leaked during panic! Resource was not returned.");

        assert_eq!(recovered_value, 0x63);
    }
}
