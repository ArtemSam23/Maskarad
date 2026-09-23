.PHONY: build test lint fmt run dist clean bench eval

build:
	cargo build --release

test:
	cargo test --workspace

lint:
	cargo fmt --all -- --check
	cargo clippy --workspace --all-targets -- -D warnings

fmt:
	cargo fmt --all

run:
	cargo run --release -- serve --config config/maskarad.yaml

eval:
	cargo run --release -- eval --config config/maskarad.yaml --dataset tests/golden

bench:
	cargo bench -p maskarad-core

# Source-only archive for submission: no target/, no .git, no dependencies.
dist:
	mkdir -p dist
	rm -f dist/maskarad-src.zip
	git ls-files | zip -q dist/maskarad-src.zip -@
	@ls -la dist/maskarad-src.zip

clean:
	cargo clean
	rm -rf dist
