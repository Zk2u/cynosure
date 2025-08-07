# Miri Setup and Usage Guide for LocalCell

This guide explains how to use [Miri](https://github.com/rust-lang/miri) to detect undefined behavior and memory safety issues in the LocalCell project.

## What is Miri?

Miri is an interpreter for Rust's mid-level intermediate representation (MIR) that can detect various classes of undefined behavior:

- **Use after free**: Accessing memory that has been deallocated
- **Double free**: Deallocating the same memory twice
- **Buffer overflows**: Reading/writing beyond allocated memory bounds
- **Uninitialized memory access**: Using values before they're initialized
- **Invalid pointer arithmetic**: Creating invalid pointers
- **Data races**: Concurrent access violations (in unsafe code)
- **Stacked Borrows violations**: Aliasing rule violations in unsafe code

## Installation

### 1. Install Nightly Toolchain

Miri requires the nightly toolchain:

```bash
rustup toolchain install nightly
```

### 2. Install Miri Component

```bash
rustup +nightly component add miri
```

### 3. Install rust-src (if prompted)

```bash
rustup +nightly component add rust-src
```

## Running Miri Tests

```
just miri
```

## Miri Flags

Useful Miri flags for debugging:

| Flag | Purpose |
|------|---------|
| `-Zmiri-backtrace=full` | Show full backtraces on errors |
| `-Zmiri-strict-provenance` | Enable strict pointer provenance checking |
| `-Zmiri-tag-tracking` | Better stacked borrows violation messages |
| `-Zmiri-disable-alignment-check` | Disable alignment checking (not recommended) |
| `-Zmiri-disable-stacked-borrows` | Disable stacked borrows checking (not recommended) |
| `-Zmiri-print-leaked` | Show information about memory leaks |

## Common Issues and Solutions

### 1. Async Tests Failing

**Problem**: Tests using `monoio` fail with "unsupported syscall" errors.

**Solution**: Miri doesn't support io_uring syscalls. Exclude async tests:

```bash
cargo +nightly miri test --lib -- \
  --skip async
```

### 2. Stacked Borrows Violations

**Problem**: "not granting access to tag X because that would remove [Unique for Y]"

**Cause**: Aliasing violations in unsafe code - creating overlapping mutable and immutable references.

**Example Fix**: In LocalCell, we fixed this by changing:
```rust
// WRONG - creates reference that can conflict
unsafe { self.ptr.as_ref().count.get() }

// CORRECT - uses raw pointer to avoid aliasing
unsafe { (*self.ptr.as_ptr()).count.get() }
```

### 3. Use After Free

**Problem**: "use after free" errors.

**Cause**: Usually double-drop issues or accessing moved memory.

**Example**: The Queue double-drop bug we fixed where elements were read with `assume_init_read()` but the Drop implementation still tried to drop them.

### 4. Uninitialized Memory

**Problem**: "uninitialized memory" errors.

**Cause**: Using `MaybeUninit` incorrectly.

**Example Fix**: We changed:
```rust
// WRONG - undefined behavior
buf: unsafe { MaybeUninit::uninit().assume_init() }

// CORRECT - properly initialized array
buf: [const { MaybeUninit::uninit() }; N]
```

## Understanding Miri Output

### Stacked Borrows Violation Example

```
error: Undefined Behavior: not granting access to tag <1234> because that would remove [Unique for <5678>] which is strongly protected
```

This means:
- `tag <1234>`: The memory access being attempted
- `[Unique for <5678>]`: A mutable reference that would be invalidated
- The access violates aliasing rules

### Memory Location Information

```
help: <1234> was created by a SharedReadWrite retag at offsets [0x0..0x10]
   --> src/lib.rs:82:18
```

This shows where the conflicting memory access originated.

## Project-Specific Notes

### LocalCell Aliasing Restrictions

LocalCell has fundamental aliasing restrictions that Miri helped us identify:

**❌ UNSOUND** - Don't do this:
```rust
let cell = LocalCell::new(vec![1, 2, 3]);
let cell2 = cell.clone();

cell.with_mut(|v| {
    let len = cell2.with(|v2| v2.len()); // Aliasing violation!
    v.push(len as i32);
});
```

**✅ SOUND** - Do this instead:
```rust
let cell = LocalCell::new(vec![1, 2, 3]);

let len = cell.with(|v| v.len()); // Get length first
cell.with_mut(|v| v.push(len as i32)); // Then mutate
```

### Queue Memory Safety

The Queue implementation required several fixes for Miri compliance:

1. **Proper array initialization**: Using `[const { MaybeUninit::uninit() }; N]`
2. **Double-drop prevention**: Setting `len = 0` before heap transitions
3. **Correct Drop implementation**: Only dropping initialized elements

## Troubleshooting

### Miri Won't Install

```bash
# Try updating rustup first
rustup update nightly
rustup +nightly component add miri
```

### Tests Run Forever

Some tests might be very slow under Miri. Use `--timeout` or exclude expensive tests.

### False Positives

Miri is conservative and may report issues in valid code. However, investigate thoroughly before assuming it's a false positive - Miri is usually correct.

## Additional Resources

- [Miri Repository](https://github.com/rust-lang/miri)
- [Stacked Borrows Explanation](https://github.com/rust-lang/unsafe-code-guidelines/blob/master/wip/stacked-borrows.md)
- [Rust Unsafe Code Guidelines](https://rust-lang.github.io/unsafe-code-guidelines/)
- [The Rustonomicon](https://doc.rust-lang.org/nomicon/) - The Dark Arts of Unsafe Rust

## Integration with Development Workflow

### Pre-commit Hook

Add to `.git/hooks/pre-commit`:

```bash
#!/bin/bash
echo "Running Miri tests..."
./miri-test.sh
if [ $? -ne 0 ]; then
    echo "Miri tests failed. Fix undefined behavior before committing."
    exit 1
fi
```

### VS Code Integration

Add to `.vscode/tasks.json`:

```json
{
    "version": "2.0.0",
    "tasks": [
        {
            "label": "Miri Test",
            "type": "shell",
            "command": "./miri-test.sh",
            "group": "test",
            "presentation": {
                "echo": true,
                "reveal": "always",
                "focus": false,
                "panel": "shared"
            }
        }
    ]
}
```

Remember: Miri is a powerful tool for finding subtle bugs that could otherwise manifest as crashes or security vulnerabilities in production. The time invested in fixing Miri violations pays dividends in code reliability and safety.
