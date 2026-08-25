# Contributing

SessionTap is a clean-room MIT implementation. Read `docs/clean-room.md` before
contributing provider behavior or fixtures.

Before submitting a change, run:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

Provider fixtures must contain synthetic or independently captured, sanitized
data and cite the public contract or capture procedure that supports them.

