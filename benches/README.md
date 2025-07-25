# LocalCell Benchmarks

This directory contains benchmarks for the LocalCell crate components using [Criterion.rs](https://github.com/bheisler/criterion.rs). These benchmarks help measure the performance of various operations and compare them with alternative implementations.

## Benchmark Suites

The benchmarks are organized into three main suites:

1. **LocalCell Benchmarks** - Tests the core `LocalCell<T>` type
   - Basic operations (creation, access, mutation)
   - Comparison with `Rc<RefCell<T>>`
   - Realistic usage patterns

2. **SmolQueue Benchmarks** - Tests the stack-optimized queue implementation
   - Operations with different capacities (inline vs heap storage)
   - Comparison with `VecDeque`
   - FIFO patterns and mixed operations

3. **LocalMutex Benchmarks** - Tests the async mutex implementation
   - Lock/unlock operations and try_lock variants
   - Comparison with Tokio's async Mutex
   - Holding locks across await points
   - Sequential operations and cloning overhead
   - Memory overhead and realistic usage patterns

## Running the Benchmarks

You can run all benchmarks or target specific ones:

```bash
# Run all benchmarks (may take some time)
cargo bench

# Run only LocalCell benchmarks
cargo bench --bench localcell

# Run only SmolQueue benchmarks
cargo bench --bench smolqueue

# Run only LocalMutex benchmarks
cargo bench --bench mutex

# Run a specific benchmark group
cargo bench --bench localcell -- "LocalCell Creation"

# Filter benchmarks by name
cargo bench --bench smolqueue -- "push_back"
```

## Interpreting Results

Criterion generates HTML reports in `target/criterion/report/index.html`. Open this file in a browser to see detailed results, including:

- Mean execution time
- Statistical significance
- Comparison with previous runs
- Performance regression detection
- Violin plots showing performance distribution

The most important metrics to look at:

- **Mean time**: Average execution time for the operation
- **Throughput**: Operations per second (higher is better)
- **Slope**: Change in performance over time (for trend analysis)

## Benchmarking Tips

For valid and reliable benchmarks:

1. **Run in release mode**: Always use `cargo bench` which automatically uses release mode
2. **Minimize system noise**: Close other applications and avoid running resource-intensive processes
3. **Run multiple times**: Criterion runs benchmarks multiple times to get statistically significant results
4. **Watch for outliers**: Sudden spikes may indicate external interference
5. **Compare relative performance**: Focus on relative differences rather than absolute numbers
6. **Use black_box()**: Prevent the compiler from optimizing away code that would be used in real scenarios

## Comparing with Alternatives

These benchmarks include comparisons with standard library and ecosystem alternatives:

- `LocalCell<T>` vs `Rc<RefCell<T>>`
- `SmolQueue<T, N>` vs `VecDeque<T>`
- `LocalMutex<T>` vs `tokio::sync::Mutex<T>` (wrapped in `Arc`)

The comparison focuses on:
- Memory overhead and allocation patterns
- Runtime performance for basic operations
- Lock contention and async behavior
- Scalability with data size
- Common usage patterns (shared counters, read-modify-write, etc.)

## Custom Benchmark Configurations

You can customize the benchmark parameters by modifying the source files. Some examples:

- Change data sizes in vector tests
- Adjust the number of waiters in mutex tests
- Modify the stack capacity of SmolQueue

After changing parameters, run the benchmarks again to see the impact on performance.