.PHONY: fmt lint test check
fmt:
	cargo fmt --all
lint:
	cargo clippy --workspace --all-targets --all-features -- -D warnings
test:
	cargo test --workspace --all-features
check: fmt lint test

