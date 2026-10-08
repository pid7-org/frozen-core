//! Acknowledge primitives for durability tracking
//!
//! ## Example
//!
//! ```
//! use frozen_core::ack::{AckTicket, Completion};
//! use std::sync::Arc;
//!
//! let completion = Arc::new(Completion::default());
//! let epoch = completion.increment_current_epoch();
//!
//! let ticket = AckTicket::new(epoch, completion.clone());
//! assert!(!ticket.is_durable());
//!
//! completion.mark_epoch_as_durable(epoch);
//! completion.notify_all_listeners();
//!
//! assert!(ticket.is_durable());
//!
//! let durable_epoch = ticket.wait().unwrap();
//! assert_eq!(durable_epoch, epoch);
//! ```

use crate::error::{ErrCode, FrozenError, FrozenResult};
use std::sync::{self, atomic};

/// A monotonically increasing epoch used as identifier for tracking durability of write operations
pub type TEpoch = u64;

/// Custom interface for requesting or executing synchronization trigger
pub trait SyncTrigger: Send + Sync {
    /// Trigger synchronization up to the current epoch
    fn trigger_sync(&self) -> FrozenResult<()>;
}

/// A shared durability acknowledgement state used for issuing [`AckTicket`]
///
/// The completion state tracks following things,
///
/// - The latest assigned epoch
/// - The latest durable epoch
/// - Waiters blocked on durability advancement
/// - Durability errors (if any) blocking durability progress
///
/// ## Example
///
/// ```
/// let completion = frozen_core::ack::Completion::default();
///
/// assert_eq!(completion.read_current_epoch(), 0);
/// assert_eq!(completion.read_durable_epoch(), 0);
/// ```
#[derive(Debug)]
pub struct Completion {
    current_epoch: atomic::AtomicU64,
    durable_epoch: atomic::AtomicU64,
    error: sync::Mutex<Option<FrozenError>>,
    wait_mutex: sync::Mutex<()>,
    durability_condvar: sync::Condvar,
    sync_trigger: sync::RwLock<Option<sync::Weak<dyn SyncTrigger>>>,
}

impl Default for Completion {
    fn default() -> Self {
        Self {
            current_epoch: atomic::AtomicU64::new(0),
            durable_epoch: atomic::AtomicU64::new(0),
            error: sync::Mutex::new(None),
            wait_mutex: sync::Mutex::new(()),
            durability_condvar: sync::Condvar::new(),
            sync_trigger: sync::RwLock::new(None),
        }
    }
}

impl Completion {
    /// Advance current and return next durability epoch
    ///
    /// ## Epoch
    ///
    /// Epoch value is monotonically increasing and used to identify unique write operations
    ///
    /// ## Example
    ///
    /// ```
    /// let completion = frozen_core::ack::Completion::default();
    ///
    /// assert_eq!(completion.increment_current_epoch(), 1);
    /// assert_eq!(completion.increment_current_epoch(), 2);
    /// assert_eq!(completion.increment_current_epoch(), 3);
    /// ```
    #[inline]
    pub fn increment_current_epoch(&self) -> TEpoch {
        self.current_epoch.fetch_add(1, atomic::Ordering::AcqRel).wrapping_add(1)
    }

    /// Mark given [`TEpoch`] as durable
    ///
    /// ## Monotonic Epoch
    ///
    /// Once an epoch is marked durable, all earlier epochs are implicitly understood to be durable
    ///
    /// ## Example
    ///
    /// ```
    /// use frozen_core::ack::Completion;
    ///
    /// let completion = Completion::default();
    /// completion.mark_epoch_as_durable(0x0A);
    ///
    /// assert_eq!(completion.read_durable_epoch(), 0x0A);
    /// ```
    #[inline]
    pub fn mark_epoch_as_durable(&self, epoch: TEpoch) {
        self.durable_epoch.fetch_max(epoch, atomic::Ordering::Release);
    }

    /// Fetch the acknowledgement error (if any)
    ///
    /// ## Example
    ///
    /// ```
    /// use frozen_core::ack::Completion;
    ///
    /// let completion = Completion::default();
    /// assert_eq!(completion.get_err(), None);
    /// ```
    #[inline]
    pub fn get_err(&self) -> Option<FrozenError> {
        let lock = self.error.lock().unwrap_or_else(|e| e.into_inner());
        lock.clone()
    }

    /// Update (i.e. replace) current acknowledgement error w/ a new [`FrozenError`]
    ///
    /// ## Example
    ///
    /// ```
    /// use frozen_core::{ack::Completion, error::{FrozenError, ErrCode}};
    ///
    /// let completion = Completion::default();
    /// let new_error = FrozenError::new(0x10, 0x20, ErrCode::new(0x30, "io"), "failed to read file");
    ///
    /// completion.set_err(new_error.clone());
    /// assert_eq!(completion.get_err(), Some(new_error));
    /// ```
    #[inline]
    pub fn set_err(&self, new_error: FrozenError) {
        let mut lock = self.error.lock().unwrap_or_else(|e| e.into_inner());
        *lock = Some(new_error);
    }

    /// Clear acknowledgement error by replacing the underlying error w/ `None`
    ///
    /// ## Example
    ///
    /// ```
    /// use frozen_core::{ack::Completion, error::{FrozenError, ErrCode}};
    ///
    /// let completion = Completion::default();
    /// let new_error = FrozenError::new(0x10, 0x20, ErrCode::new(0x30, "io"), "failed to read file");
    ///
    /// completion.set_err(new_error.clone());
    /// assert!(completion.get_err().is_some());
    ///
    /// completion.del_err();
    /// assert!(completion.get_err().is_none());
    /// ```
    #[inline]
    pub fn del_err(&self) {
        let mut lock = self.error.lock().unwrap_or_else(|e| e.into_inner());
        *lock = None;
    }

    /// Read the latest assigned epoch
    ///
    /// ## Example
    ///
    /// ```
    /// use frozen_core::ack::Completion;
    ///
    /// let completion = Completion::default();
    /// completion.increment_current_epoch();
    ///
    /// assert_eq!(completion.read_current_epoch(), 1);
    /// ```
    #[inline]
    pub fn read_current_epoch(&self) -> TEpoch {
        self.current_epoch.load(atomic::Ordering::Acquire)
    }

    /// Read the latest durable epoch
    ///
    /// ## Example
    ///
    /// ```
    /// use frozen_core::ack::Completion;
    ///
    /// let completion = Completion::default();
    /// completion.mark_epoch_as_durable(0x3A);
    ///
    /// assert_eq!(completion.read_durable_epoch(), 0x3A);
    /// ```
    #[inline]
    pub fn read_durable_epoch(&self) -> TEpoch {
        self.durable_epoch.load(atomic::Ordering::Acquire)
    }

    /// Wake all listeners currently waiting for durability progress
    ///
    /// Waking listeners does not modify any durable state and is typically called after advancing the durable
    /// epoch or setting an error
    ///
    /// ## Example
    ///
    /// ```
    /// use frozen_core::ack::Completion;
    ///
    /// let completion = Completion::default();
    /// completion.notify_all_listeners();
    /// ```
    #[inline]
    pub fn notify_all_listeners(&self) {
        self.durability_condvar.notify_all();
    }

    /// Register a [`SyncTrigger`] implementation for triggering syncs
    pub fn set_sync_trigger(&self, trigger: sync::Weak<dyn SyncTrigger>) {
        let mut lock = self.sync_trigger.write().unwrap_or_else(|e| e.into_inner());
        *lock = Some(trigger);
    }

    /// Trigger synchronization using the registered [`SyncTrigger`]
    pub fn trigger_sync(&self) -> FrozenResult<()> {
        let trigger = {
            let lock = self.sync_trigger.read().unwrap_or_else(|e| e.into_inner());
            lock.as_ref().and_then(|w| w.upgrade())
        };

        if let Some(trigger) = trigger {
            trigger.trigger_sync()
        } else {
            Err(FrozenError::new(
                0,
                0x08,
                ErrCode::new(0x0A, "failed to sync"),
                "no sync trigger registered or file handle closed",
            ))
        }
    }

    /// Blocks the current thread until the specified `epoch` becomes durable or an error is reported
    pub fn wait_for_epoch(&self, epoch: TEpoch) -> FrozenResult<TEpoch> {
        if self.read_durable_epoch() >= epoch {
            return Ok(epoch);
        }

        if let Some(err) = self.get_err() {
            return Err(err);
        }

        let mut guard = self.wait_mutex.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if self.read_durable_epoch() >= epoch {
                return Ok(epoch);
            }

            if let Some(err) = self.get_err() {
                return Err(err);
            }

            guard = self.durability_condvar.wait(guard).unwrap_or_else(|e| e.into_inner());
        }
    }
}

/// Durability handle associated with a write operation
///
/// ## Epoch
///
/// Every ticket is assigned a monotonically increasing epoch to monitor durability
///
/// ## Durability Guarantee
///
/// The ticket can be queried via [`is_durable`](Self::is_durable), waited on via [`wait`](Self::wait), or forced
/// via [`force`](Self::force)
///
/// Once an epoch is confirmed durable, all writes assigned to earlier epochs are also guaranteed to be durable
///
/// Callers that only require fire-and-forget semantics may simply discard the returned ticket
#[derive(Debug, Clone)]
pub struct AckTicket {
    epoch: TEpoch,
    completion: sync::Arc<Completion>,
}

impl AckTicket {
    /// Construct a new [`AckTicket`] for a write operation
    ///
    /// ## Example
    ///
    /// ```
    /// use frozen_core::ack::{AckTicket, Completion};
    /// use std::sync::Arc;
    ///
    /// let completion = Arc::new(Completion::default());
    /// let ticket = AckTicket::new(1, completion);
    ///
    /// assert_eq!(ticket.epoch(), 1);
    /// ```
    #[inline]
    pub const fn new(epoch: TEpoch, completion: sync::Arc<Completion>) -> Self {
        Self { epoch, completion }
    }

    /// Read assigned durability epoch for the [`AckTicket`]
    ///
    /// ## Example
    ///
    /// ```
    /// use frozen_core::ack::{AckTicket, Completion};
    /// use std::sync::Arc;
    ///
    /// let completion = Arc::new(Completion::default());
    /// let ticket = AckTicket::new(0x4C, completion);
    ///
    /// assert_eq!(ticket.epoch(), 0x4C);
    /// ```
    #[inline(always)]
    pub const fn epoch(&self) -> TEpoch {
        self.epoch
    }

    /// Check if the [`AckTicket`] is already durable w/o blocking
    ///
    /// ## Example
    ///
    /// ```
    /// use frozen_core::ack::{AckTicket, Completion};
    /// use std::sync::Arc;
    ///
    /// let completion = Arc::new(Completion::default());
    /// let ticket = AckTicket::new(10, completion.clone());
    ///
    /// assert!(!ticket.is_durable());
    /// completion.mark_epoch_as_durable(10);
    /// assert!(ticket.is_durable());
    /// ```
    #[inline]
    pub fn is_durable(&self) -> bool {
        self.completion.read_durable_epoch() >= self.epoch
    }

    /// Blocks the current thread until the [`AckTicket`] becomes durable
    ///
    /// ## Error Propagation
    ///
    /// If a durability error is reported before the epoch becomes durable, the corresponding [`FrozenError`] is
    /// returned instead
    ///
    /// ## Example
    ///
    /// ```
    /// use frozen_core::ack::{AckTicket, Completion};
    /// use std::{sync::Arc, thread, time};
    ///
    /// let completion = Arc::new(Completion::default());
    /// let epoch = completion.increment_current_epoch();
    /// let ticket = AckTicket::new(epoch, completion.clone());
    ///
    /// thread::spawn({
    ///     let completion = completion.clone();
    ///     move || {
    ///         thread::sleep(time::Duration::from_millis(10));
    ///         completion.mark_epoch_as_durable(epoch);
    ///         completion.notify_all_listeners();
    ///     }
    /// });
    ///
    /// assert_eq!(ticket.wait().unwrap(), epoch);
    /// ```
    #[inline]
    pub fn wait(&self) -> FrozenResult<TEpoch> {
        self.completion.wait_for_epoch(self.epoch)
    }

    /// Forces synchronization for this ticket and blocks until it becomes durable
    ///
    /// If a background sync thread is running, signals the worker to sync immediately; otherwise executes the
    /// sync synchronously under the lock
    ///
    /// ## Example
    ///
    /// ```
    /// use frozen_core::ack::{AckTicket, Completion};
    /// use std::sync::Arc;
    ///
    /// let completion = Arc::new(Completion::default());
    /// completion.mark_epoch_as_durable(5);
    /// let ticket = AckTicket::new(5, completion);
    ///
    /// assert_eq!(ticket.force().unwrap(), 5);
    /// ```
    #[inline]
    pub fn force(&self) -> FrozenResult<TEpoch> {
        if self.is_durable() {
            return Ok(self.epoch);
        }

        if let Some(err) = self.completion.get_err() {
            return Err(err);
        }

        self.completion.trigger_sync()?;
        self.completion.wait_for_epoch(self.epoch)
    }

    /// Reference to the underlying shared [`Completion`]
    #[inline]
    pub fn completion(&self) -> &sync::Arc<Completion> {
        &self.completion
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::Arc, thread, time::Duration};

    #[test]
    fn ok_ticket_epoch_and_is_durable() {
        let completion = Arc::new(Completion::default());
        let ticket = AckTicket::new(42, completion.clone());

        assert_eq!(ticket.epoch(), 42);
        assert!(!ticket.is_durable());

        completion.mark_epoch_as_durable(41);
        assert!(!ticket.is_durable());

        completion.mark_epoch_as_durable(42);
        assert!(ticket.is_durable());

        completion.mark_epoch_as_durable(100);
        assert!(ticket.is_durable());
    }

    #[test]
    fn ok_ticket_wait_blocks_until_durable() {
        let completion = Arc::new(Completion::default());
        let epoch = completion.increment_current_epoch();
        let ticket = AckTicket::new(epoch, completion.clone());

        let t = thread::spawn({
            let completion = completion.clone();
            move || {
                thread::sleep(Duration::from_millis(20));
                completion.mark_epoch_as_durable(epoch);
                completion.notify_all_listeners();
            }
        });

        assert_eq!(ticket.wait().unwrap(), epoch);
        t.join().unwrap();
    }

    #[test]
    fn err_ticket_wait_returns_error() {
        let completion = Arc::new(Completion::default());
        let epoch = completion.increment_current_epoch();
        let ticket = AckTicket::new(epoch, completion.clone());

        let err = FrozenError::new(1, 2, ErrCode::new(3, "test"), "io failure");

        let t = thread::spawn({
            let completion = completion.clone();
            let err = err.clone();
            move || {
                thread::sleep(Duration::from_millis(20));
                completion.set_err(err);
                completion.notify_all_listeners();
            }
        });

        let res = ticket.wait();
        assert!(res.is_err());
        assert_eq!(res.unwrap_err().reason, 3);
        t.join().unwrap();
    }

    struct MockTrigger {
        completion: Arc<Completion>,
        synced: atomic::AtomicBool,
    }

    impl SyncTrigger for MockTrigger {
        fn trigger_sync(&self) -> FrozenResult<()> {
            self.synced.store(true, atomic::Ordering::Release);
            let curr = self.completion.read_current_epoch();
            self.completion.mark_epoch_as_durable(curr);
            self.completion.notify_all_listeners();
            Ok(())
        }
    }

    #[test]
    fn ok_ticket_force_with_trigger() {
        let completion = Arc::new(Completion::default());
        let trigger = Arc::new(MockTrigger {
            completion: completion.clone(),
            synced: atomic::AtomicBool::new(false),
        });

        let weak = Arc::downgrade(&trigger) as sync::Weak<dyn SyncTrigger>;
        completion.set_sync_trigger(weak);

        let epoch = completion.increment_current_epoch();
        let ticket = AckTicket::new(epoch, completion.clone());

        assert!(!ticket.is_durable());
        assert_eq!(ticket.force().unwrap(), epoch);
        assert!(trigger.synced.load(atomic::Ordering::Acquire));
        assert!(ticket.is_durable());
    }

    #[test]
    fn err_ticket_force_without_trigger() {
        let completion = Arc::new(Completion::default());
        let epoch = completion.increment_current_epoch();
        let ticket = AckTicket::new(epoch, completion.clone());

        let res = ticket.force();
        assert!(res.is_err());
    }
}
