use std::{
    pin::Pin,
    sync::Arc,
    task::{Context, Poll, Wake, Waker},
    thread,
};

struct ThreadWaker(thread::Thread);

impl Wake for ThreadWaker {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}

thread_local! {
    /// One waker per thread, built lazily and reused across every `block_on`
    /// call on that thread. The captured `Thread` handle is this thread's, so
    /// the waker always unparks the thread that is parking — correct for both
    /// the same-thread fast path and cross-thread wakeups (the parking thread
    /// registers *its* waker, the peer calls `wake()` to unpark it).
    ///
    /// Building the `Arc<ThreadWaker>` once per thread (instead of once per
    /// call) avoids a heap allocation on every `push_blocking`/`pop_blocking`.
    static THREAD_WAKER: Waker = Arc::new(ThreadWaker(thread::current())).into();
}

pub fn block_on<F: std::future::Future>(mut future: F) -> F::Output {
    let mut future = unsafe { Pin::new_unchecked(&mut future) };

    THREAD_WAKER.with(|waker| {
        let mut cx = Context::from_waker(waker);
        loop {
            match future.as_mut().poll(&mut cx) {
                Poll::Ready(output) => return output,
                Poll::Pending => thread::park(),
            }
        }
    })
}
