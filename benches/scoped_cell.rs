use std::{cell::RefCell, rc::Rc};

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use cynosure::site_c::cell::ScopedCell;

// Benchmark basic operations
fn bench_scopedcell_ops(c: &mut Criterion) {
    // Creation benchmarks
    let mut group = c.benchmark_group("ScopedCell Creation");
    group.bench_function("new_i32", |b| b.iter(|| ScopedCell::new(black_box(42))));
    group.bench_function("new_vec", |b| {
        b.iter(|| ScopedCell::new(black_box(vec![1, 2, 3, 4, 5])))
    });
    group.bench_function("new_string", |b| {
        b.iter(|| ScopedCell::new(black_box(String::from("hello world"))))
    });
    group.finish();

    // Access benchmarks
    let cell_i32 = ScopedCell::new(42);
    let cell_vec = ScopedCell::new(vec![1, 2, 3, 4, 5]);
    let cell_string = ScopedCell::new(String::from("hello world"));

    let mut group = c.benchmark_group("ScopedCell Access");
    group.bench_function("with_i32", |b| {
        b.iter(|| {
            cell_i32.with(|x| {
                black_box(*x);
            })
        })
    });
    group.bench_function("with_vec", |b| {
        b.iter(|| {
            cell_vec.with(|v| {
                black_box(v.len());
            })
        })
    });
    group.bench_function("with_string", |b| {
        b.iter(|| {
            cell_string.with(|s| {
                black_box(s.len());
            })
        })
    });
    group.finish();

    // Mutation benchmarks
    let cell_i32 = ScopedCell::new(42);
    let cell_vec = ScopedCell::new(vec![1, 2, 3, 4, 5]);
    let cell_string = ScopedCell::new(String::from("hello world"));

    let mut group = c.benchmark_group("ScopedCell Mutation");
    group.bench_function("with_mut_i32", |b| {
        b.iter(|| {
            cell_i32.with_mut(|x| {
                *x = black_box(*x + 1);
            })
        })
    });
    group.bench_function("with_mut_vec_read", |b| {
        b.iter(|| {
            cell_vec.with_mut(|v| {
                black_box(v.len());
            })
        })
    });
    group.bench_function("with_mut_vec_push", |b| {
        b.iter(|| {
            cell_vec.with_mut(|v| {
                v.push(black_box(6));
                v.pop();
            })
        })
    });
    group.bench_function("with_mut_string_read", |b| {
        b.iter(|| {
            cell_string.with_mut(|s| {
                black_box(s.len());
            })
        })
    });
    group.bench_function("with_mut_string_append", |b| {
        b.iter(|| {
            cell_string.with_mut(|s| {
                s.push('!');
                s.pop();
            })
        })
    });
    group.finish();

    // Clone benchmarks
    let cell_i32 = ScopedCell::rc(42);
    let cell_vec = ScopedCell::rc(vec![1, 2, 3, 4, 5]);
    let cell_string = ScopedCell::rc(String::from("hello world"));

    let mut group = c.benchmark_group("ScopedCell Clone");
    group.bench_function("clone_i32", |b| b.iter(|| black_box(cell_i32.clone())));
    group.bench_function("clone_vec", |b| b.iter(|| black_box(cell_vec.clone())));
    group.bench_function("clone_string", |b| {
        b.iter(|| black_box(cell_string.clone()))
    });
    group.finish();
}

// Comparison with RefCell<T>
fn bench_comparison(c: &mut Criterion) {
    let mut group = c.benchmark_group("ScopedCell vs RefCell");

    // Creation
    group.bench_function("ScopedCell::new(i32)", |b| {
        b.iter(|| ScopedCell::new(black_box(42)))
    });
    group.bench_function("RefCell::new(i32)", |b| {
        b.iter(|| RefCell::new(black_box(42)))
    });

    // Immutable access
    let scopedcell = ScopedCell::new(42);
    let refcell = RefCell::new(42);

    group.bench_function("ScopedCell::with", |b| {
        b.iter(|| {
            scopedcell.with(|x| {
                black_box(*x);
            })
        })
    });
    group.bench_function("RefCell::borrow", |b| {
        b.iter(|| {
            black_box(*refcell.borrow());
        })
    });

    // Mutable access
    let scopedcell = ScopedCell::new(42);
    let refcell = RefCell::new(42);

    group.bench_function("ScopedCell::with_mut", |b| {
        b.iter(|| {
            scopedcell.with_mut(|x| {
                *x = black_box(*x + 1);
            })
        })
    });
    group.bench_function("RefCell::borrow_mut", |b| {
        b.iter(|| {
            let old_value = *refcell.borrow();
            *refcell.borrow_mut() = black_box(old_value + 1);
        })
    });

    group.finish();
}

// Benchmark for realistic use cases
fn bench_realistic_usage(c: &mut Criterion) {
    // Create a scenario that simulates a typical usage pattern
    let mut group = c.benchmark_group("Realistic Usage");

    // Simple counter incrementing
    group.bench_function("ScopedCell counter", |b| {
        b.iter(|| {
            let counter = ScopedCell::rc(0);
            let counter2 = counter.clone();

            for _ in 0..100 {
                counter.with_mut(|c| *c += 1);
            }

            for _ in 0..100 {
                counter2.with_mut(|c| *c += 1);
            }

            counter.with(|c| black_box(*c))
        })
    });

    group.bench_function("Rc<RefCell> counter", |b| {
        b.iter(|| {
            let counter = Rc::new(RefCell::new(0));
            let counter2 = counter.clone();

            for _ in 0..100 {
                *counter.borrow_mut() += 1;
            }

            for _ in 0..100 {
                *counter2.borrow_mut() += 1;
            }

            black_box(*counter.borrow())
        })
    });

    // Simulating multiple readers and occasional writer
    group.bench_function("ScopedCell readers+writer", |b| {
        b.iter(|| {
            let data = ScopedCell::rc(vec![1, 2, 3, 4, 5]);
            let data2 = data.clone();
            let data3 = data.clone();

            // Many reads
            for _ in 0..50 {
                data.with(|v| black_box(v.len()));
                data2.with(|v| black_box(v.iter().sum::<i32>()));
                data3.with(|v| black_box(v.last().copied()));
            }

            // Occasional write
            for _ in 0..5 {
                data.with_mut(|v| v.push(black_box(6)));
            }

            data.with(|v| black_box(v.len()))
        })
    });

    group.bench_function("Rc<RefCell> readers+writer", |b| {
        b.iter(|| {
            let data = Rc::new(RefCell::new(vec![1, 2, 3, 4, 5]));
            let data2 = data.clone();
            let data3 = data.clone();

            // Many reads
            for _ in 0..50 {
                black_box(data.borrow().len());
                black_box(data2.borrow().iter().sum::<i32>());
                black_box(data3.borrow().last());
            }

            // Occasional write
            for _ in 0..5 {
                data.borrow_mut().push(black_box(6));
            }

            black_box(data.borrow().len())
        })
    });

    group.finish();
}

// Define the benchmark group
criterion_group!(
    benches,
    bench_scopedcell_ops,
    bench_comparison,
    bench_realistic_usage,
);
criterion_main!(benches);
