use localcell::mutex::LocalMutex;
use std::time::Duration;

#[monoio::test]
async fn test_basic_lock_unlock() {
    let mutex = LocalMutex::new(42);

    {
        let mut guard = mutex.lock().await;
        assert_eq!(*guard, 42);
        *guard = 100;
    } // Lock released here

    let guard = mutex.lock().await;
    assert_eq!(*guard, 100);
}

#[monoio::test]
async fn test_try_lock() {
    let mutex = LocalMutex::new(vec![1, 2, 3]);

    let guard = mutex.try_lock();
    assert!(guard.is_some());

    let guard2 = mutex.try_lock();
    assert!(guard2.is_none()); // Already locked

    drop(guard);

    let guard3 = mutex.try_lock();
    assert!(guard3.is_some()); // Now available
}

#[monoio::test(timer_enabled = true)]
async fn test_multiple_waiters() {
    let mutex = LocalMutex::new(0);
    let mutex_clone1 = mutex.clone();
    let mutex_clone2 = mutex.clone();

    // First task acquires lock
    let guard = mutex.lock().await;

    // Spawn waiting tasks
    let handle1 = monoio::spawn(async move {
        let mut guard = mutex_clone1.lock().await;
        *guard += 1;
    });

    let handle2 = monoio::spawn(async move {
        let mut guard = mutex_clone2.lock().await;
        *guard += 10;
    });

    // Let tasks register as waiters
    monoio::time::sleep(Duration::from_millis(10)).await;

    // Release lock
    drop(guard);

    // Wait for both tasks to complete
    handle1.await;
    handle2.await;

    let final_value = mutex.lock().await;
    assert_eq!(*final_value, 11);
}

#[monoio::test(timer_enabled = true)]
async fn test_hold_across_await() {
    let mutex = LocalMutex::new(String::from("test"));

    let mut guard = mutex.lock().await;

    // Can hold lock across await points
    monoio::time::sleep(Duration::from_millis(10)).await;
    guard.push_str("_modified");
    monoio::time::sleep(Duration::from_millis(10)).await;

    assert_eq!(&*guard, "test_modified");
}

#[monoio::test(timer_enabled = true)]
async fn test_mutex_contention() {
    let counter = LocalMutex::new(0);
    let mut handles = vec![];

    for _ in 0..10 {
        let counter = counter.clone();
        let handle = monoio::spawn(async move {
            for _ in 0..10 {
                let mut guard = counter.lock().await;

                let old_value = *guard;

                // Simulate some async work while holding lock
                monoio::time::sleep(Duration::from_millis(1)).await;

                *guard = old_value + 1;
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.await;
    }

    let final_count = counter.lock().await;
    assert_eq!(*final_count, 100);
}
