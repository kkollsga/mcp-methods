.PHONY: dev test lint fmt clean

dev:
	maturin develop --release

test:
	pytest tests/ -v

lint:
	cargo fmt -- --check
	cargo clippy -- -D warnings
	ruff check .

fmt:
	cargo fmt
	ruff format .
	ruff check --fix .

clean:
	cargo clean
	rm -rf wheels/ dist/ *.egg-info build/
