.PHONY: test clippy fmt build docker

test:
	cargo test --all

clippy:
	cargo clippy --all-targets --all-features -- -D warnings

fmt:
	cargo fmt --all

build:
	cargo build --release

docker:
	docker build -t txwatch .