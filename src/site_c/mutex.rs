use std::{
    future::Future,
    ops::{Deref, DerefMut},
    pin::Pin,
    rc::Rc,
    task::{Context, Poll, Waker},
};

use super::{cell::ScopedCell, queue::Queue};
use crate::hints::likely;

/// A mutual exclusion primitive for single-threaded async executors.
///
/// This mutex allows holding a lock across await points, with waiting tasks
/// being woken when the lock becomes available.
#[derive(Clone)]
pub struct LocalMutex<T> {
    state: Rc<ScopedCell<MutexState<T>>>,
}

struct MutexState<T, const QUEUE_STACK_SIZE: usize = 8> {
    locked: bool,
    waiters: Queue<Waker, QUEUE_STACK_SIZE>,
    value: T,
}

impl<T> LocalMutex<T> {
    /// Creates a new mutex in an unlocked state.
    pub fn new(value: T) -> Self {
        Self {
            state: ScopedCell::new(MutexState {
                locked: false,
                waiters: Queue::new(),
                value,
            })
            .into(),
        }
    }

    /// Acquires the mutex, returning a guard that releases it on drop.
    ///
    /// If the mutex is already locked, the calling task will yield and
    /// be woken when the mutex becomes available.
    pub fn lock(&self) -> LocalMutexLockFuture<'_, T> {
        LocalMutexLockFuture { mutex: self }
    }

    /// Attempts to acquire the mutex without waiting.
    ///
    /// Returns `Some(guard)` if successful, `None` if already locked.
    pub fn try_lock(&self) -> Option<LocalMutexGuard<'_, T>> {
        self.state.with_mut(|state| {
            if likely(!state.locked) {
                state.locked = true;
                Some(LocalMutexGuard { mutex: self })
            } else {
                None
            }
        })
    }

    /// Returns true if the mutex is currently locked.
    pub fn is_locked(&self) -> bool {
        self.state.with(|state| state.locked)
    }

    fn unlock(&self) {
        self.state.with_mut(|state| {
            state.locked = false;

            // Wake ALL waiters - better throughput
            let waiters = std::mem::take(&mut state.waiters);
            for waker in waiters {
                waker.wake();
            }
        });
    }
}

/// Future returned by `LocalMutex::lock()`.
pub struct LocalMutexLockFuture<'a, T> {
    mutex: &'a LocalMutex<T>,
}

impl<'a, T> Future for LocalMutexLockFuture<'a, T> {
    type Output = LocalMutexGuard<'a, T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.mutex.state.with_mut(|state| {
            if likely(!state.locked) {
                // Acquire the lock
                state.locked = true;
                Poll::Ready(LocalMutexGuard { mutex: self.mutex })
            } else {
                state.waiters.push_back(cx.waker().clone());
                Poll::Pending
            }
        })
    }
}

/// RAII guard that releases the mutex on drop.
pub struct LocalMutexGuard<'a, T> {
    mutex: &'a LocalMutex<T>,
}

impl<'a, T> Deref for LocalMutexGuard<'a, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe {
            // SAFETY: We have exclusive access via the lock
            self.mutex.state.with(|state| &*(&state.value as *const T))
        }
    }
}

impl<'a, T> DerefMut for LocalMutexGuard<'a, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe {
            // SAFETY: We have exclusive access via the lock
            self.mutex
                .state
                .with_mut(|state| &mut *(&mut state.value as *mut T))
        }
    }
}

impl<'a, T> Drop for LocalMutexGuard<'a, T> {
    fn drop(&mut self) {
        self.mutex.unlock();
    }
}
