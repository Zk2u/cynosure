use std::{
    future::Future,
    ops::{Deref, DerefMut},
    pin::Pin,
    rc::Rc,
    task::{Context, Poll, Waker},
};

use crate::hints::likely;

use super::{cell::ScopedCell, queue::Queue};

/// A reader-writer lock for single-threaded async executors.
///
/// This lock allows multiple readers or a single writer to access the
/// protected data. Like `LocalMutex`, it can be held across await points.
#[derive(Clone)]
pub struct LocalRwLock<T> {
    state: Rc<ScopedCell<RwLockState<T>>>,
}

struct RwLockState<T, const QUEUE_STACK_SIZE: usize = 8> {
    readers: usize,
    writer: bool,
    read_waiters: Queue<Waker, QUEUE_STACK_SIZE>,
    write_waiters: Queue<Waker, QUEUE_STACK_SIZE>,
    value: T,
}

impl<T> LocalRwLock<T> {
    /// Creates a new reader-writer lock in an unlocked state.
    pub fn new(value: T) -> Self {
        Self {
            state: ScopedCell::new(RwLockState {
                readers: 0,
                writer: false,
                read_waiters: Queue::new(),
                write_waiters: Queue::new(),
                value,
            })
            .into(),
        }
    }

    /// Acquires a read lock, returning a guard that releases it on drop.
    ///
    /// Multiple readers can hold the lock simultaneously. If a writer is
    /// active, the calling task will yield and be woken when the lock
    /// becomes available for reading.
    pub fn read(&self) -> LocalRwLockReadFuture<'_, T> {
        LocalRwLockReadFuture { rwlock: self }
    }

    /// Acquires a write lock, returning a guard that releases it on drop.
    ///
    /// Only one writer can hold the lock at a time, and it excludes all
    /// readers. If the lock is held by any readers or another writer,
    /// the calling task will yield and be woken when the lock becomes
    /// available for writing.
    pub fn write(&self) -> LocalRwLockWriteFuture<'_, T> {
        LocalRwLockWriteFuture { rwlock: self }
    }

    /// Attempts to acquire a read lock without waiting.
    ///
    /// Returns `Some(guard)` if successful, `None` if a writer holds the lock.
    pub fn try_read(&self) -> Option<LocalRwLockReadGuard<'_, T>> {
        self.state.with_mut(|state| {
            if likely(!state.writer) {
                state.readers += 1;
                Some(LocalRwLockReadGuard { rwlock: self })
            } else {
                None
            }
        })
    }

    /// Attempts to acquire a write lock without waiting.
    ///
    /// Returns `Some(guard)` if successful, `None` if the lock is held by
    /// any readers or another writer.
    pub fn try_write(&self) -> Option<LocalRwLockWriteGuard<'_, T>> {
        self.state.with_mut(|state| {
            if likely(state.readers == 0 && !state.writer) {
                state.writer = true;
                Some(LocalRwLockWriteGuard { rwlock: self })
            } else {
                None
            }
        })
    }

    /// Returns true if the lock is currently held by a writer.
    pub fn is_write_locked(&self) -> bool {
        self.state.with(|state| state.writer)
    }

    /// Returns the number of current readers.
    pub fn reader_count(&self) -> usize {
        self.state.with(|state| state.readers)
    }

    fn release_read(&self) {
        self.state.with_mut(|state| {
            state.readers -= 1;

            // If no more readers, wake a writer if any
            if state.readers == 0 && !state.write_waiters.is_empty() {
                if let Some(waker) = state.write_waiters.pop_front() {
                    waker.wake();
                }
            }
        });
    }

    fn release_write(&self) {
        self.state.with_mut(|state| {
            state.writer = false;

            // Prefer waking all readers over a single writer
            if !state.read_waiters.is_empty() {
                let waiters = std::mem::take(&mut state.read_waiters);
                for waker in waiters {
                    waker.wake();
                }
            } else if let Some(waker) = state.write_waiters.pop_front() {
                waker.wake();
            }
        });
    }
}

/// Future returned by `LocalRwLock::read()`.
pub struct LocalRwLockReadFuture<'a, T> {
    rwlock: &'a LocalRwLock<T>,
}

impl<'a, T> Future for LocalRwLockReadFuture<'a, T> {
    type Output = LocalRwLockReadGuard<'a, T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.rwlock.state.with_mut(|state| {
            if likely(!state.writer) {
                state.readers += 1;
                Poll::Ready(LocalRwLockReadGuard {
                    rwlock: self.rwlock,
                })
            } else {
                state.read_waiters.push_back(cx.waker().clone());
                Poll::Pending
            }
        })
    }
}

/// Future returned by `LocalRwLock::write()`.
pub struct LocalRwLockWriteFuture<'a, T> {
    rwlock: &'a LocalRwLock<T>,
}

impl<'a, T> Future for LocalRwLockWriteFuture<'a, T> {
    type Output = LocalRwLockWriteGuard<'a, T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.rwlock.state.with_mut(|state| {
            if likely(state.readers == 0 && !state.writer) {
                state.writer = true;
                Poll::Ready(LocalRwLockWriteGuard {
                    rwlock: self.rwlock,
                })
            } else {
                state.write_waiters.push_back(cx.waker().clone());
                Poll::Pending
            }
        })
    }
}

/// RAII guard for read access that releases the read lock on drop.
pub struct LocalRwLockReadGuard<'a, T> {
    rwlock: &'a LocalRwLock<T>,
}

impl<'a, T> Deref for LocalRwLockReadGuard<'a, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe {
            // SAFETY: We have shared read access via the lock
            self.rwlock.state.with(|state| &*(&state.value as *const T))
        }
    }
}

impl<'a, T> Drop for LocalRwLockReadGuard<'a, T> {
    fn drop(&mut self) {
        self.rwlock.release_read();
    }
}

/// RAII guard for write access that releases the write lock on drop.
pub struct LocalRwLockWriteGuard<'a, T> {
    rwlock: &'a LocalRwLock<T>,
}

impl<'a, T> Deref for LocalRwLockWriteGuard<'a, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe {
            // SAFETY: We have exclusive write access via the lock
            self.rwlock.state.with(|state| &*(&state.value as *const T))
        }
    }
}

impl<'a, T> DerefMut for LocalRwLockWriteGuard<'a, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe {
            // SAFETY: We have exclusive write access via the lock
            self.rwlock
                .state
                .with_mut(|state| &mut *(&mut state.value as *mut T))
        }
    }
}

impl<'a, T> Drop for LocalRwLockWriteGuard<'a, T> {
    fn drop(&mut self) {
        self.rwlock.release_write();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

    fn dummy_waker() -> Waker {
        unsafe fn clone(_: *const ()) -> RawWaker {
            RawWaker::new(std::ptr::null(), &VTABLE)
        }
        unsafe fn wake(_: *const ()) {}
        unsafe fn wake_by_ref(_: *const ()) {}
        unsafe fn drop(_: *const ()) {}

        static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop);
        let raw_waker = RawWaker::new(std::ptr::null(), &VTABLE);
        unsafe { Waker::from_raw(raw_waker) }
    }

    #[test]
    fn test_sync_try_read_write() {
        let rwlock = LocalRwLock::new(42);

        // Try read should succeed
        let guard = rwlock.try_read();
        assert!(guard.is_some());
        assert_eq!(*guard.unwrap(), 42);

        // Another try read should succeed
        let guard2 = rwlock.try_read();
        assert!(guard2.is_some());

        // Try write should fail with readers
        let write_guard = rwlock.try_write();
        assert!(write_guard.is_none());
    }

    #[test]
    fn test_sync_write_exclusion() {
        let rwlock = LocalRwLock::new(String::from("hello"));

        // Try write should succeed
        let mut guard = rwlock.try_write();
        assert!(guard.is_some());

        let guard = guard.as_mut().unwrap();
        assert_eq!(&**guard, "hello");
        guard.push_str(" world");

        // Try read should fail with writer
        let read_guard = rwlock.try_read();
        assert!(read_guard.is_none());

        // Another try write should fail
        let write_guard2 = rwlock.try_write();
        assert!(write_guard2.is_none());
    }

    #[test]
    fn test_sync_reader_count() {
        let rwlock = LocalRwLock::new(vec![1, 2, 3]);

        assert_eq!(rwlock.reader_count(), 0);
        assert!(!rwlock.is_write_locked());

        let _r1 = rwlock.try_read().unwrap();
        assert_eq!(rwlock.reader_count(), 1);

        let _r2 = rwlock.try_read().unwrap();
        assert_eq!(rwlock.reader_count(), 2);

        let _r3 = rwlock.try_read().unwrap();
        assert_eq!(rwlock.reader_count(), 3);

        drop(_r1);
        assert_eq!(rwlock.reader_count(), 2);

        drop(_r2);
        drop(_r3);
        assert_eq!(rwlock.reader_count(), 0);
    }

    #[test]
    fn test_sync_future_polling() {
        let rwlock = LocalRwLock::new(100);
        let waker = dummy_waker();
        let mut cx = Context::from_waker(&waker);

        // Poll read future - should succeed immediately
        let mut read_fut = Box::pin(rwlock.read());
        match read_fut.as_mut().poll(&mut cx) {
            Poll::Ready(guard) => assert_eq!(*guard, 100),
            Poll::Pending => panic!("Read should succeed immediately"),
        }

        // Try to poll write future while read is held - should be pending
        let _read_guard = rwlock.try_read().unwrap();
        let mut write_fut = Box::pin(rwlock.write());
        match write_fut.as_mut().poll(&mut cx) {
            Poll::Ready(_) => panic!("Write should be pending with active reader"),
            Poll::Pending => {} // Expected
        }
    }

    #[test]
    fn test_sync_drop_behavior() {
        let rwlock = LocalRwLock::new(0);

        // Test read guard drop
        {
            let _r1 = rwlock.try_read().unwrap();
            let _r2 = rwlock.try_read().unwrap();
            assert_eq!(rwlock.reader_count(), 2);
        }
        assert_eq!(rwlock.reader_count(), 0);

        // Test write guard drop
        {
            let _w = rwlock.try_write().unwrap();
            assert!(rwlock.is_write_locked());
        }
        assert!(!rwlock.is_write_locked());

        // Verify can acquire write after drops
        let guard = rwlock.try_write();
        assert!(guard.is_some());
    }
}
