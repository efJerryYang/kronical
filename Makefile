.PHONY: all check build install clean help

CARGO_FLAGS = --all-features --all-targets
RELEASE_FLAGS = --release $(CARGO_FLAGS)

all: check build

check:
	@echo "Checking code..."
	cargo check $(CARGO_FLAGS)
	@echo "Running clippy..."
	cargo clippy $(CARGO_FLAGS) -- -D warnings
	@echo "Checking formatting..."
	cargo fmt --check

build:
	@echo "Building debug..."
	cargo build $(CARGO_FLAGS)
	@echo "Building release..."
	cargo build $(RELEASE_FLAGS) -q

install:
	@echo "Installing binaries..."
	cargo install --path . --all-features --force

clean:
	@echo "Cleaning build artifacts..."
	cargo clean

test:
	@echo "Running tests..."
	cargo test $(CARGO_FLAGS)

doc:
	@echo "Building documentation..."
	cargo doc $(CARGO_FLAGS) --no-deps

help:
	@echo "Available targets:"
	@echo "  all     - Run check and build (default)"
	@echo "  check   - Run cargo check, clippy, and format check"
	@echo "  build   - Build debug and release versions"
	@echo "  install - Install release binaries to ~/.cargo/bin"
	@echo "  test    - Run tests"
	@echo "  doc     - Build documentation"
	@echo "  clean   - Clean build artifacts"
	@echo "  help    - Show this help message"