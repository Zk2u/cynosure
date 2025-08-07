fmt:
    cargo +nightly fmt --all

miri:
    cargo +nightly miri test --lib -- \
        --skip async
