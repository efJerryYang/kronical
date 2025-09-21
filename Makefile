.PHONY: release build check install clean test doc coverage help all


.DEFAULT_GOAL := release

# Controls (override from the command line, e.g. `make build PROFILE=release VERBOSE=1 FEATURES="--features hotpath"`)
PROFILE ?= debug
VERBOSE ?= 0
TARGET_FLAGS ?= --all-targets
FEATURE_FLAGS ?=

CARGO_PROFILE_FLAG_debug =
CARGO_PROFILE_FLAG_release = --release
CARGO_PROFILE_FLAG := $(CARGO_PROFILE_FLAG_$(PROFILE))

ifeq ($(VERBOSE),1)
  Q =
  CARGO_LOG_FLAG = --verbose
  ANNOUNCE = @true
else
  Q = @
  CARGO_LOG_FLAG = --quiet
  ANNOUNCE = @echo
endif

release:
	$(ANNOUNCE) "cargo build $(TARGET_FLAGS) $(FEATURE_FLAGS) --release $(CARGO_LOG_FLAG)"
	$(Q)cargo build $(TARGET_FLAGS) $(FEATURE_FLAGS) --release $(CARGO_LOG_FLAG)

build:
	$(ANNOUNCE) "cargo build $(TARGET_FLAGS) $(FEATURE_FLAGS) $(CARGO_PROFILE_FLAG) $(CARGO_LOG_FLAG)"
	$(Q)cargo build $(TARGET_FLAGS) $(FEATURE_FLAGS) $(CARGO_PROFILE_FLAG) $(CARGO_LOG_FLAG)

check:
	$(ANNOUNCE) "cargo check $(TARGET_FLAGS) $(FEATURE_FLAGS) $(CARGO_LOG_FLAG)"
	$(Q)cargo check $(TARGET_FLAGS) $(FEATURE_FLAGS) $(CARGO_LOG_FLAG)
	$(ANNOUNCE) "cargo clippy $(TARGET_FLAGS) $(FEATURE_FLAGS) $(CARGO_LOG_FLAG) -- -D warnings"
	$(Q)cargo clippy $(TARGET_FLAGS) $(FEATURE_FLAGS) $(CARGO_LOG_FLAG) -- -D warnings
	$(ANNOUNCE) "cargo fmt --check"
	$(Q)cargo fmt --check

install:
	$(ANNOUNCE) "cargo install --path . --force $(CARGO_LOG_FLAG) $(FEATURE_FLAGS)"
	$(Q)cargo install --path . --force $(CARGO_LOG_FLAG) $(FEATURE_FLAGS)

clean:
	$(ANNOUNCE) "cargo clean"
	$(Q)cargo clean

test:
	$(ANNOUNCE) "cargo test $(TARGET_FLAGS) $(FEATURE_FLAGS) $(CARGO_PROFILE_FLAG) $(CARGO_LOG_FLAG)"
	$(Q)cargo test $(TARGET_FLAGS) $(FEATURE_FLAGS) $(CARGO_PROFILE_FLAG) $(CARGO_LOG_FLAG)

doc:
	$(ANNOUNCE) "cargo doc $(TARGET_FLAGS) $(FEATURE_FLAGS) --no-deps $(CARGO_LOG_FLAG)"
	$(Q)cargo doc $(TARGET_FLAGS) $(FEATURE_FLAGS) --no-deps $(CARGO_LOG_FLAG)

coverage:
	$(ANNOUNCE) "rm -rf coverage"
	$(Q)rm -rf coverage
	$(ANNOUNCE) "mkdir -p coverage"
	$(Q)mkdir -p coverage
	$(ANNOUNCE) "cargo llvm-cov --workspace $(FEATURE_FLAGS) --remap-path-prefix --fail-under-lines 0 --lcov --output-path coverage/lcov.info"
	$(Q)cargo llvm-cov --workspace $(FEATURE_FLAGS) --remap-path-prefix --fail-under-lines 0 --lcov --output-path coverage/lcov.info
	$(ANNOUNCE) "cargo llvm-cov report $(FEATURE_FLAGS) --remap-path-prefix --summary-only --show-missing-lines --color always"
	$(Q)cargo llvm-cov report $(FEATURE_FLAGS) --remap-path-prefix --summary-only --show-missing-lines --color always | tee coverage/summary.ansi
	$(ANNOUNCE) "strip ANSI sequences into coverage/summary.txt"
	$(Q)perl -pe 's/\e\[[0-9;]*[A-Za-z]//g' coverage/summary.ansi > coverage/summary.txt
	$(Q)rm coverage/summary.ansi
	$(ANNOUNCE) "cargo llvm-cov report $(FEATURE_FLAGS) --remap-path-prefix --html --output-dir coverage"
	$(Q)cargo llvm-cov report $(FEATURE_FLAGS) --remap-path-prefix --html --output-dir coverage

all: check build

help:
	@echo "Targets: release (default), build, check, test, coverage, doc, install, clean"
	@echo "Use PROFILE=debug or PROFILE=release to select cargo profile"
	@echo "Set VERBOSE=1 to show underlying cargo commands"
	@echo "Pass FEATURE_FLAGS=\"--features hotpath\" (etc.) to forward feature toggles"
