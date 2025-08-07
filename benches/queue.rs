use std::collections::VecDeque;

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use cynosure::site_c::queue::Queue;

// Basic operations benchmarks
fn bench_queue_ops(c: &mut Criterion) {
    // Creation benchmarks
    let mut group = c.benchmark_group("Queue Creation");
    group.bench_function("new<i32, 4>", |b| b.iter(Queue::<i32, 4>::new));
    group.bench_function("new<String, 4>", |b| b.iter(Queue::<String, 4>::new));
    group.bench_function("new<Vec<i32>, 4>", |b| {
        b.iter(Queue::<Vec<i32>, 4>::new)
    });
    group.finish();

    // Push operations with different queue capacities
    // For each capacity, create a specific benchmark
    {
        let mut group = c.benchmark_group("Queue<i32, 2> Push");

        // Inline case (fewer items than capacity)
        group.bench_function("push_back_inline", |b| {
            b.iter(|| {
                let mut queue = Queue::<i32, 2>::new();
                for i in 0..1 {
                    queue.push_back(black_box(i));
                }
                black_box(queue)
            })
        });

        // Spill-to-heap case (more items than capacity)
        group.bench_function("push_back_spill", |b| {
            b.iter(|| {
                let mut queue = Queue::<i32, 2>::new();
                for i in 0..6 {
                    queue.push_back(black_box(i));
                }
                black_box(queue)
            })
        });

        group.finish();
    }

    {
        let mut group = c.benchmark_group("Queue<i32, 4> Push");

        // Inline case (fewer items than capacity)
        group.bench_function("push_back_inline", |b| {
            b.iter(|| {
                let mut queue = Queue::<i32, 4>::new();
                for i in 0..3 {
                    queue.push_back(black_box(i));
                }
                black_box(queue)
            })
        });

        // Spill-to-heap case (more items than capacity)
        group.bench_function("push_back_spill", |b| {
            b.iter(|| {
                let mut queue = Queue::<i32, 4>::new();
                for i in 0..8 {
                    queue.push_back(black_box(i));
                }
                black_box(queue)
            })
        });

        group.finish();
    }

    {
        let mut group = c.benchmark_group("Queue<i32, 8> Push");

        // Inline case (fewer items than capacity)
        group.bench_function("push_back_inline", |b| {
            b.iter(|| {
                let mut queue = Queue::<i32, 8>::new();
                for i in 0..7 {
                    queue.push_back(black_box(i));
                }
                black_box(queue)
            })
        });

        // Spill-to-heap case (more items than capacity)
        group.bench_function("push_back_spill", |b| {
            b.iter(|| {
                let mut queue = Queue::<i32, 8>::new();
                for i in 0..12 {
                    queue.push_back(black_box(i));
                }
                black_box(queue)
            })
        });

        group.finish();
    }

    {
        let mut group = c.benchmark_group("Queue<i32, 16> Push");

        // Inline case (fewer items than capacity)
        group.bench_function("push_back_inline", |b| {
            b.iter(|| {
                let mut queue = Queue::<i32, 16>::new();
                for i in 0..15 {
                    queue.push_back(black_box(i));
                }
                black_box(queue)
            })
        });

        // Spill-to-heap case (more items than capacity)
        group.bench_function("push_back_spill", |b| {
            b.iter(|| {
                let mut queue = Queue::<i32, 16>::new();
                for i in 0..20 {
                    queue.push_back(black_box(i));
                }
                black_box(queue)
            })
        });

        group.finish();
    }

    // Pop operations
    {
        let mut group = c.benchmark_group("Queue<i32, 4> Pop");

        // Pop from inline
        group.bench_function("pop_front_inline", |b| {
            b.iter(|| {
                let mut queue = Queue::<i32, 4>::new();
                for i in 0..3 {
                    queue.push_back(i);
                }
                for _ in 0..3 {
                    black_box(queue.pop_front());
                }
            })
        });

        // Pop from heap
        group.bench_function("pop_front_heap", |b| {
            b.iter(|| {
                let mut queue = Queue::<i32, 4>::new();
                for i in 0..8 {
                    queue.push_back(i);
                }
                for _ in 0..8 {
                    black_box(queue.pop_front());
                }
            })
        });

        group.finish();
    }

    {
        let mut group = c.benchmark_group("Queue<i32, 8> Pop");

        // Pop from inline
        group.bench_function("pop_front_inline", |b| {
            b.iter(|| {
                let mut queue = Queue::<i32, 8>::new();
                for i in 0..7 {
                    queue.push_back(i);
                }
                for _ in 0..7 {
                    black_box(queue.pop_front());
                }
            })
        });

        // Pop from heap
        group.bench_function("pop_front_heap", |b| {
            b.iter(|| {
                let mut queue = Queue::<i32, 8>::new();
                for i in 0..12 {
                    queue.push_back(i);
                }
                for _ in 0..12 {
                    black_box(queue.pop_front());
                }
            })
        });

        group.finish();
    }

    // Take all operations
    {
        let mut group = c.benchmark_group("Queue<i32, 4> Take All");

        // Take all from inline
        group.bench_function("take_all_inline", |b| {
            b.iter(|| {
                let mut queue = Queue::<i32, 4>::new();
                for i in 0..3 {
                    queue.push_back(i);
                }
                black_box(queue.into_vec())
            })
        });

        // Take all from heap
        group.bench_function("take_all_heap", |b| {
            b.iter(|| {
                let mut queue = Queue::<i32, 4>::new();
                for i in 0..8 {
                    queue.push_back(i);
                }
                black_box(queue.into_vec())
            })
        });

        group.finish();
    }

    {
        let mut group = c.benchmark_group("Queue<i32, 8> Take All");

        // Take all from inline
        group.bench_function("take_all_inline", |b| {
            b.iter(|| {
                let mut queue = Queue::<i32, 8>::new();
                for i in 0..7 {
                    queue.push_back(i);
                }
                black_box(queue.into_vec())
            })
        });

        // Take all from heap
        group.bench_function("take_all_heap", |b| {
            b.iter(|| {
                let mut queue = Queue::<i32, 8>::new();
                for i in 0..12 {
                    queue.push_back(i);
                }
                black_box(queue.into_vec())
            })
        });

        group.finish();
    }

    // Iteration operations
    {
        let mut group = c.benchmark_group("Queue<i32, 4> Iteration");

        // Iterate inline
        group.bench_function("iter_inline", |b| {
            b.iter_batched(
                || {
                    let mut queue = Queue::<i32, 4>::new();
                    for i in 0..3 {
                        queue.push_back(i);
                    }
                    queue
                },
                |queue| {
                    let mut sum = 0;
                    for &item in &queue {
                        sum += item;
                    }
                    black_box(sum)
                },
                criterion::BatchSize::SmallInput,
            )
        });

        // Iterate heap
        group.bench_function("iter_heap", |b| {
            b.iter_batched(
                || {
                    let mut queue = Queue::<i32, 4>::new();
                    for i in 0..8 {
                        queue.push_back(i);
                    }
                    queue
                },
                |queue| {
                    let mut sum = 0;
                    for &item in &queue {
                        sum += item;
                    }
                    black_box(sum)
                },
                criterion::BatchSize::SmallInput,
            )
        });

        group.finish();
    }

    {
        let mut group = c.benchmark_group("Queue<i32, 8> Iteration");

        // Iterate inline
        group.bench_function("iter_inline", |b| {
            b.iter_batched(
                || {
                    let mut queue = Queue::<i32, 8>::new();
                    for i in 0..7 {
                        queue.push_back(i);
                    }
                    queue
                },
                |queue| {
                    let mut sum = 0;
                    for &item in &queue {
                        sum += item;
                    }
                    black_box(sum)
                },
                criterion::BatchSize::SmallInput,
            )
        });

        // Iterate heap
        group.bench_function("iter_heap", |b| {
            b.iter_batched(
                || {
                    let mut queue = Queue::<i32, 8>::new();
                    for i in 0..12 {
                        queue.push_back(i);
                    }
                    queue
                },
                |queue| {
                    let mut sum = 0;
                    for &item in &queue {
                        sum += item;
                    }
                    black_box(sum)
                },
                criterion::BatchSize::SmallInput,
            )
        });

        group.finish();
    }
}

// Comparison with VecDeque
fn bench_comparison(c: &mut Criterion) {
    let mut group = c.benchmark_group("Queue vs VecDeque");

    // Push single item
    group.bench_function("Queue<i32, 8>::push_back", |b| {
        b.iter_batched(
            Queue::<i32, 8>::new,
            |mut queue| queue.push_back(black_box(42)),
            criterion::BatchSize::SmallInput,
        )
    });

    group.bench_function("VecDeque::push_back", |b| {
        b.iter_batched(
            VecDeque::<i32>::new,
            |mut queue| queue.push_back(black_box(42)),
            criterion::BatchSize::SmallInput,
        )
    });

    // Pop single item
    group.bench_function("Queue<i32, 8>::pop_front", |b| {
        b.iter_batched(
            || {
                let mut queue = Queue::<i32, 8>::new();
                queue.push_back(42);
                queue
            },
            |mut queue| black_box(queue.pop_front()),
            criterion::BatchSize::SmallInput,
        )
    });

    group.bench_function("VecDeque::pop_front", |b| {
        b.iter_batched(
            || {
                let mut queue = VecDeque::<i32>::new();
                queue.push_back(42);
                queue
            },
            |mut queue| black_box(queue.pop_front()),
            criterion::BatchSize::SmallInput,
        )
    });

    // Push many items
    group.bench_function("Queue<i32, 8>::push_many", |b| {
        b.iter(|| {
            let mut queue = Queue::<i32, 8>::new();
            for i in 0..20 {
                queue.push_back(black_box(i));
            }
            black_box(queue)
        })
    });

    group.bench_function("VecDeque::push_many", |b| {
        b.iter(|| {
            let mut queue = VecDeque::<i32>::new();
            for i in 0..20 {
                queue.push_back(black_box(i));
            }
            black_box(queue)
        })
    });

    // Iteration
    group.bench_function("Queue<i32, 8>::iteration", |b| {
        b.iter_batched(
            || {
                let mut queue = Queue::<i32, 8>::new();
                for i in 0..20 {
                    queue.push_back(i);
                }
                queue
            },
            |queue| {
                let mut sum = 0;
                for &item in &queue {
                    sum += item;
                }
                black_box(sum)
            },
            criterion::BatchSize::SmallInput,
        )
    });

    group.bench_function("VecDeque::iteration", |b| {
        b.iter_batched(
            || {
                let mut queue = VecDeque::<i32>::new();
                for i in 0..20 {
                    queue.push_back(i);
                }
                queue
            },
            |queue| {
                let mut sum = 0;
                for &item in &queue {
                    sum += item;
                }
                black_box(sum)
            },
            criterion::BatchSize::SmallInput,
        )
    });

    group.finish();
}

// Realistic use cases
fn bench_realistic_usage(c: &mut Criterion) {
    let mut group = c.benchmark_group("Realistic Usage");

    // FIFO queue pattern
    group.bench_function("Queue<i32, 8> FIFO", |b| {
        b.iter(|| {
            let mut queue = Queue::<i32, 8>::new();

            // Fill queue
            for i in 0..16 {
                queue.push_back(black_box(i));
            }

            // Process half the items
            for _ in 0..8 {
                black_box(queue.pop_front());
            }

            // Add more items
            for i in 16..24 {
                queue.push_back(black_box(i));
            }

            // Process remaining
            while let Some(item) = queue.pop_front() {
                black_box(item);
            }
        })
    });

    group.bench_function("VecDeque<i32> FIFO", |b| {
        b.iter(|| {
            let mut queue = VecDeque::<i32>::new();

            // Fill queue
            for i in 0..16 {
                queue.push_back(black_box(i));
            }

            // Process half the items
            for _ in 0..8 {
                black_box(queue.pop_front());
            }

            // Add more items
            for i in 16..24 {
                queue.push_back(black_box(i));
            }

            // Process remaining
            while let Some(item) = queue.pop_front() {
                black_box(item);
            }
        })
    });

    // Mixed operations pattern
    group.bench_function("Queue<i32, 4> mixed ops", |b| {
        b.iter(|| {
            let mut queue = Queue::<i32, 4>::new();

            for _ in 0..5 {
                // Add a few items
                for i in 0..3 {
                    queue.push_back(black_box(i));
                }

                // Take one
                black_box(queue.pop_front());

                // Add more (causes spill to heap)
                for i in 3..7 {
                    queue.push_back(black_box(i));
                }

                // Take several
                for _ in 0..4 {
                    black_box(queue.pop_front());
                }

                // Final batch
                for i in 7..10 {
                    queue.push_back(black_box(i));
                }
            }

            // Take all remaining
            black_box(queue.into_vec())
        })
    });

    group.bench_function("VecDeque<i32> mixed ops", |b| {
        b.iter(|| {
            let mut queue = VecDeque::<i32>::new();

            for _ in 0..5 {
                // Add a few items
                for i in 0..3 {
                    queue.push_back(black_box(i));
                }

                // Take one
                black_box(queue.pop_front());

                // Add more
                for i in 3..7 {
                    queue.push_back(black_box(i));
                }

                // Take several
                for _ in 0..4 {
                    black_box(queue.pop_front());
                }

                // Final batch
                for i in 7..10 {
                    queue.push_back(black_box(i));
                }
            }

            // Take all remaining
            let all: Vec<_> = queue.drain(..).collect();
            black_box(all)
        })
    });

    group.finish();
}

// Define the benchmark group
criterion_group!(
    benches,
    bench_queue_ops,
    bench_comparison,
    bench_realistic_usage,
);
criterion_main!(benches);
