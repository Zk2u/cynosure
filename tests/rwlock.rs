use std::time::Duration;

use cynosure::site_c::rwlock::LocalRwLock;

#[monoio::test]
async fn test_basic_read() {
    let rwlock = LocalRwLock::new(42);

    {
        let guard = rwlock.read().await;
        assert_eq!(*guard, 42);
    } // Read lock released here

    let guard = rwlock.read().await;
    assert_eq!(*guard, 42);
}

#[monoio::test]
async fn test_basic_write() {
    let rwlock = LocalRwLock::new(42);

    {
        let mut guard = rwlock.write().await;
        assert_eq!(*guard, 42);
        *guard = 100;
    } // Write lock released here

    let guard = rwlock.read().await;
    assert_eq!(*guard, 100);
}

#[monoio::test]
async fn test_multiple_readers() {
    let rwlock = LocalRwLock::new(vec![1, 2, 3]);

    // Multiple readers can acquire lock simultaneously
    let guard1 = rwlock.read().await;
    let guard2 = rwlock.read().await;
    let guard3 = rwlock.read().await;

    assert_eq!(*guard1, vec![1, 2, 3]);
    assert_eq!(*guard2, vec![1, 2, 3]);
    assert_eq!(*guard3, vec![1, 2, 3]);
    assert_eq!(rwlock.reader_count(), 3);
}

#[monoio::test]
async fn test_try_read_write() {
    let rwlock = LocalRwLock::new(String::from("test"));

    // Can acquire read lock
    let read_guard = rwlock.try_read();
    assert!(read_guard.is_some());

    // Can acquire another read lock
    let read_guard2 = rwlock.try_read();
    assert!(read_guard2.is_some());

    // Cannot acquire write lock while readers exist
    let write_guard = rwlock.try_write();
    assert!(write_guard.is_none());

    drop(read_guard);
    drop(read_guard2);

    // Now can acquire write lock
    let write_guard = rwlock.try_write();
    assert!(write_guard.is_some());

    // Cannot acquire read lock while writer exists
    let read_guard3 = rwlock.try_read();
    assert!(read_guard3.is_none());
}

#[monoio::test(timer_enabled = true)]
async fn test_reader_writer_exclusion() {
    let rwlock = LocalRwLock::new(0);
    let rwlock_clone = rwlock.clone();

    // Writer acquires lock first
    let mut write_guard = rwlock.write().await;

    // Spawn reader that will wait
    let reader_handle = monoio::spawn(async move {
        let guard = rwlock_clone.read().await;
        *guard // Return the value
    });

    // Let reader register as waiter
    monoio::time::sleep(Duration::from_millis(10)).await;

    // Modify value
    *write_guard = 42;

    // Release write lock
    drop(write_guard);

    // Reader should now get the updated value
    let value = reader_handle.await;
    assert_eq!(value, 42);
}

#[monoio::test(timer_enabled = true)]
async fn test_writer_waits_for_readers() {
    let rwlock = LocalRwLock::new(0);
    let rwlock_clone = rwlock.clone();

    // Multiple readers acquire lock
    let _read1 = rwlock.read().await;
    let _read2 = rwlock.read().await;

    // Spawn writer that will wait
    let writer_handle = monoio::spawn(async move {
        let mut guard = rwlock_clone.write().await;
        *guard = 100;
    });

    // Let writer register as waiter
    monoio::time::sleep(Duration::from_millis(10)).await;

    // Release read locks
    drop(_read1);
    drop(_read2);

    // Writer should complete
    writer_handle.await;

    let value = rwlock.read().await;
    assert_eq!(*value, 100);
}

#[monoio::test(timer_enabled = true)]
async fn test_hold_across_await() {
    let rwlock = LocalRwLock::new(String::from("test"));

    // Hold read lock across await
    {
        let guard = rwlock.read().await;
        monoio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(&*guard, "test");
        monoio::time::sleep(Duration::from_millis(10)).await;
    }

    // Hold write lock across await
    {
        let mut guard = rwlock.write().await;
        monoio::time::sleep(Duration::from_millis(10)).await;
        guard.push_str("_modified");
        monoio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(&*guard, "test_modified");
    }
}

#[monoio::test(timer_enabled = true)]
async fn test_reader_preference() {
    let rwlock = LocalRwLock::new(0);
    let rwlock_clone1 = rwlock.clone();
    let rwlock_clone2 = rwlock.clone();

    // Writer holds lock
    let _write_guard = rwlock.write().await;

    // Queue up readers and a writer
    let reader1 = monoio::spawn(async move {
        let _guard = rwlock_clone1.read().await;
        1
    });

    let writer = monoio::spawn(async move {
        let mut guard = rwlock_clone2.write().await;
        *guard = 42;
        2
    });

    // Let them register as waiters
    monoio::time::sleep(Duration::from_millis(10)).await;

    // Release write lock - readers should be woken first
    drop(_write_guard);

    let result1 = reader1.await;
    assert_eq!(result1, 1); // Reader completes first

    let result2 = writer.await;
    assert_eq!(result2, 2); // Writer completes second

    let final_value = rwlock.read().await;
    assert_eq!(*final_value, 42);
}

#[monoio::test(timer_enabled = true)]
async fn test_contention() {
    let counter = LocalRwLock::new(0);
    let mut handles = vec![];

    // Spawn readers
    for i in 0..5 {
        let counter = counter.clone();
        let handle = monoio::spawn(async move {
            for _ in 0..10 {
                let guard = counter.read().await;
                let value = *guard;
                drop(guard);

                // Simulate some async work
                monoio::time::sleep(Duration::from_millis(1)).await;

                // Verify value hasn't decreased (writers only increment)
                let guard = counter.read().await;
                assert!(*guard >= value);
            }
            format!("reader_{i}")
        });
        handles.push(handle);
    }

    // Spawn writers
    for i in 0..5 {
        let counter = counter.clone();
        let handle = monoio::spawn(async move {
            for _ in 0..10 {
                let mut guard = counter.write().await;
                let old_value = *guard;

                // Simulate some async work while holding write lock
                monoio::time::sleep(Duration::from_millis(1)).await;

                *guard = old_value + 1;
            }
            format!("writer_{i}")
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.await;
    }

    let final_count = counter.read().await;
    assert_eq!(*final_count, 50); // 5 writers * 10 increments each
}

#[monoio::test]
async fn test_is_write_locked() {
    let rwlock = LocalRwLock::new(42);

    assert!(!rwlock.is_write_locked());

    let _read = rwlock.read().await;
    assert!(!rwlock.is_write_locked());
    drop(_read);

    let _write = rwlock.write().await;
    assert!(rwlock.is_write_locked());
    drop(_write);

    assert!(!rwlock.is_write_locked());
}

#[monoio::test]
async fn test_reader_count() {
    let rwlock = LocalRwLock::new(42);

    assert_eq!(rwlock.reader_count(), 0);

    let _r1 = rwlock.read().await;
    assert_eq!(rwlock.reader_count(), 1);

    let _r2 = rwlock.read().await;
    assert_eq!(rwlock.reader_count(), 2);

    drop(_r1);
    assert_eq!(rwlock.reader_count(), 1);

    drop(_r2);
    assert_eq!(rwlock.reader_count(), 0);
}
