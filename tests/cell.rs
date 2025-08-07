use std::{rc::Rc, time::Duration};

use cynosure::site_c::cell::*;

#[monoio::test(timer_enabled = true)]
async fn test_rate_limiter_pattern() {
    // Rate limiter with shared state
    struct RateLimiter {
        count: ScopedCell<u32>,
        last_reset: ScopedCell<std::time::Instant>,
    }

    let limiter = RateLimiter {
        count: ScopedCell::new(0),
        last_reset: ScopedCell::new(std::time::Instant::now()),
    };

    // Simulate requests
    for _ in 0..5 {
        let allowed = limiter.count.with_mut(|count| {
            if *count < 3 {
                *count += 1;
                true
            } else {
                false
            }
        });

        assert!(allowed || limiter.count.with(|c| *c >= 3));
        monoio::time::sleep(Duration::from_millis(10)).await;
    }

    // Reset after time window
    limiter
        .last_reset
        .with_mut(|t| *t = std::time::Instant::now());
    limiter.count.with_mut(|c| *c = 0);

    // Should allow new requests
    let allowed = limiter.count.with_mut(|count| {
        *count += 1;
        true
    });
    assert!(allowed);
}

#[monoio::test]
async fn test_connection_pool_pattern() {
    // Shared pool of "connections" (simplified)
    struct Connection {
        id: usize,
        in_use: bool,
    }

    let pool = ScopedCell::new(vec![
        Connection {
            id: 0,
            in_use: false,
        },
        Connection {
            id: 1,
            in_use: false,
        },
        Connection {
            id: 2,
            in_use: false,
        },
    ]);

    // Acquire connection
    let conn_id = pool.with_mut(|conns| {
        conns.iter_mut().find(|c| !c.in_use).map(|c| {
            c.in_use = true;
            c.id
        })
    });

    assert_eq!(conn_id, Some(0));

    // Use connection (async work)
    monoio::spawn(async move {
        // Simulate work
        std::thread::yield_now();
    })
    .await;

    // Release connection
    pool.with_mut(|conns| {
        if let Some(conn) = conns.iter_mut().find(|c| c.id == 0) {
            conn.in_use = false;
        }
    });

    // Verify it's available again
    let reacquired = pool.with(|conns| conns.iter().any(|c| c.id == 0 && !c.in_use));
    assert!(reacquired);
}

#[monoio::test]
async fn test_accumulator_across_tasks() {
    // Shared accumulator for results from multiple tasks
    struct Stats {
        total: i64,
        count: usize,
        min: Option<i64>,
        max: Option<i64>,
    }

    let stats = ScopedCell::rc(Stats {
        total: 0,
        count: 0,
        min: None,
        max: None,
    });

    let mut handles = vec![];

    for i in 0..10 {
        let stats = stats.clone();
        let handle = monoio::spawn(async move {
            // Simulate some computation
            let value = (i * i) as i64;

            // Update shared stats
            stats.with_mut(|s| {
                s.total += value;
                s.count += 1;
                s.min = Some(s.min.map_or(value, |m| m.min(value)));
                s.max = Some(s.max.map_or(value, |m| m.max(value)));
            });
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.await;
    }

    stats.with(|s| {
        assert_eq!(s.count, 10);
        assert_eq!(s.total, (0..10).map(|i| (i * i) as i64).sum::<i64>());
        assert_eq!(s.min, Some(0));
        assert_eq!(s.max, Some(81));
    });
}

#[monoio::test(timer_enabled = true)]
async fn test_event_bus_pattern() {
    // Simple event bus with subscribers
    type Handler = Box<dyn Fn(&str)>;

    struct EventBus {
        handlers: ScopedCell<Vec<Handler>>,
        events: Rc<ScopedCell<Vec<String>>>,
    }

    let bus = EventBus {
        handlers: ScopedCell::new(Vec::new()),
        events: ScopedCell::rc(Vec::new()),
    };

    // Register handlers
    bus.handlers.with_mut(|h| {
        h.push(Box::new(|event| {
            println!("Handler 1: {event}");
        }));
        h.push(Box::new(|event| {
            println!("Handler 2: {event}");
        }));
    });

    // Emit events from different tasks
    let bus_clone = bus.events.clone();
    monoio::spawn(async move {
        monoio::time::sleep(Duration::from_millis(10)).await;
        bus_clone.with_mut(|events| {
            events.push("Event A".to_string());
        });
    })
    .await;

    // Process events
    bus.events.with_mut(|events| {
        while let Some(event) = events.pop() {
            bus.handlers.with(|handlers| {
                for handler in handlers {
                    handler(&event);
                }
            });
        }
    });
}

#[monoio::test]
async fn test_recursive_data_structure() {
    // Tree-like structure with shared nodes
    struct Node {
        value: i32,
        children: Vec<ScopedCell<Node>>,
    }

    let root = ScopedCell::new(Node {
        value: 1,
        children: vec![
            ScopedCell::new(Node {
                value: 2,
                children: vec![],
            }),
            ScopedCell::new(Node {
                value: 3,
                children: vec![],
            }),
        ],
    });

    // Traverse and sum values
    fn sum_tree(node: &ScopedCell<Node>) -> i32 {
        node.with(|n| n.value + n.children.iter().map(sum_tree).sum::<i32>())
    }

    assert_eq!(sum_tree(&root), 6);

    // Modify leaf node
    root.with(|r| {
        if let Some(child) = r.children.first() {
            child.with_mut(|c| c.value = 10);
        }
    });

    assert_eq!(sum_tree(&root), 14);
}

#[monoio::test]
async fn test_cached_computation() {
    struct Cache<T> {
        value: Option<T>,
        computation_in_progress: bool,
    }

    let cache = ScopedCell::rc(Cache::<String> {
        value: None,
        computation_in_progress: false,
    });

    // Multiple tasks trying to get cached value
    let mut handles = vec![];

    for i in 0..3 {
        let cache = cache.clone();
        let handle = monoio::spawn(async move {
            // Check if we need to compute
            let should_compute = cache.with_mut(|c| {
                if c.value.is_some() {
                    return false;
                }
                if !c.computation_in_progress {
                    c.computation_in_progress = true;
                    return true;
                }
                false
            });

            if should_compute {
                // Simulate expensive computation
                let result = format!("Computed by task {i}");

                cache.with_mut(|c| {
                    c.value = Some(result);
                    c.computation_in_progress = false;
                });
            }

            // Wait for value to be available
            loop {
                if let Some(val) = cache.with(|c| c.value.clone()) {
                    return val;
                }
                monoio::spawn(async {}).await; // Yield
            }
        });
        handles.push(handle);
    }

    let results: Vec<String> = futures::future::join_all(handles).await;

    // All tasks should get the same cached value
    assert!(results.iter().all(|r| r == &results[0]));
}

#[monoio::test]
async fn test_no_await_enforcement() {
    let data = ScopedCell::new(vec![1, 2, 3]);

    // This compiles and works
    data.with_mut(|v| {
        v.push(4);
        // Any synchronous operation is fine
        let sum: i32 = v.iter().sum();
        v.push(sum);
    });

    // This would NOT compile (commented out to keep tests passing):
    // data.with_mut(|v| async {
    //     v.push(5);
    //     monoio::time::sleep(Duration::from_millis(1)).await;
    // });

    data.with(|v| {
        assert_eq!(v, &[1, 2, 3, 4, 10]);
    });
}
