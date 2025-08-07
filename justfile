fmt:
    cargo +nightly fmt --all

miri:
    cargo +nightly miri test --lib -- \
        --skip async

precommit: fmt clippy

clippy:
    cargo clippy --all-features -- -D warnings
