.PHONY: fmt fmt-check lint test check
fmt:
	cargo fmt --all
fmt-check:
	cargo fmt --all --check
lint:
	cargo clippy --workspace --all-targets --all-features -- -D warnings
test:
	cargo test --workspace --all-features
check: fmt-check lint test
