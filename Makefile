.PHONY: build-parser sync-typeshed

build-parser:
	RUSTFLAGS="-C target-cpu=native" cargo build --release --manifest-path parser/Cargo.toml

sync-typeshed:
	cargo run --release --manifest-path parser/Cargo.toml -- sync-typeshed
