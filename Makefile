.PHONY: all fmt fmt-check clippy clippy-windows test test-lib doc dupes dupes-cleanup publish-check version-check changelog-check check build install e2e check-all help

# cargo-dupes reads dupes.toml ONLY from the analysed path (no parent lookup)
# and silently passes when it finds none, so the gate refuses to run without it.
DUPES_CFG := dupes.toml
DUPES_VERSION := 0.2.1
BIN := browser-control-mcp

# Default target: run all mandatory quality gates
all: check

# Auto-fix code formatting
fmt:
	cargo fmt --all

# Verify code formatting
fmt-check:
	cargo fmt --all -- --check

# Lint every target (lib, bin, tests) with warnings as errors
clippy:
	cargo clippy --all-targets --all-features -- -D warnings

# Windows-only compile errors (e.g. dead code behind cfg(unix)) caught from
# Linux. Needs `rustup target add x86_64-pc-windows-gnu`; not part of `check`
# because CI runs the real Windows build.
clippy-windows:
	cargo clippy --all-targets --all-features --target x86_64-pc-windows-gnu -- -D warnings

# Every test target `cargo test` runs (unit tests, tests/*.rs, doctests);
# real-browser tests are #[ignore]d and do not run here.
test:
	cargo test --all-features

# Fast inner loop: library unit tests only (not part of `make check`)
test-lib:
	cargo test --lib --all-features

# Rustdoc must build without warnings (broken intra-doc links, missing docs).
# --document-private-items also checks the links in private code.
doc:
	RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features --document-private-items

# Check for code duplication (requires cargo-dupes)
dupes:
	@test -x "$$(command -v cargo-dupes)" || (echo "cargo-dupes not found. Installing..." && cargo install cargo-dupes --version $(DUPES_VERSION) --locked)
	@test -f "$(DUPES_CFG)" || (echo "ERROR: $(DUPES_CFG) missing; cargo-dupes would silently pass without thresholds" && exit 1)
	cargo dupes --path . check

# Show stale duplication suppressions
dupes-cleanup:
	@test -x "$$(command -v cargo-dupes)" || (echo "cargo-dupes not found. Installing..." && cargo install cargo-dupes --version $(DUPES_VERSION) --locked)
	cargo dupes --path . cleanup --dry-run

# The crate must package cleanly (no path dependencies, nothing missing).
# --allow-dirty: the gate runs before the commit it guards.
publish-check:
	cargo publish --dry-run --allow-dirty --locked

# The release notes come from CHANGELOG.md: GitHub masks a URL with
# credentials (scheme://user:pass@) as a secret and drops the release plan,
# so no binary, npm or Homebrew release is built.
changelog-check:
	@if grep -nE '[a-z][a-z0-9+.-]*://[^[:space:]/@`]+:[^[:space:]/@`]*@' CHANGELOG.md; then \
		echo "CHANGELOG.md has a URL with credentials (GitHub masks it and skips the release builds)"; \
		exit 1; \
	fi

# package.json (npm installer / glama metadata) must carry Cargo.toml's version
version-check:
	@cargo_v=$$(awk -F'"' '/^version/ {print $$2; exit}' Cargo.toml); \
	npm_v=$$(awk -F'"' '/"version"/ {print $$4; exit}' package.json); \
	if [ -n "$$cargo_v" ] && [ "$$cargo_v" = "$$npm_v" ]; then \
		echo "Version in sync: $$cargo_v"; \
	else \
		printf 'Version mismatch: Cargo.toml=%s package.json=%s\n' "$$cargo_v" "$$npm_v"; \
		exit 1; \
	fi

# All local quality gates, sequentially; stops at the first red one
check: fmt-check clippy test doc dupes publish-check version-check changelog-check

# Release binary
build:
	cargo build --release --all-features

ifeq ($(OS),Windows_NT)
    EXT := .exe
else
    EXT :=
endif
CARGO_HOME ?= $(HOME)/.cargo
DESTDIR ?= $(CARGO_HOME)/bin

# Install the release binary into CARGO_HOME/bin (defaults to ~/.cargo/bin)
install: build
	@mkdir -p "$(DESTDIR)"
	@cp -f target/release/$(BIN)$(EXT) "$(DESTDIR)/$(BIN)$(EXT)"
	@echo "Installed $(DESTDIR)/$(BIN)$(EXT)"

# Smoke against real Chrome and Firefox (Linux/macOS; headless, E2E_VISIBLE=1 shows them).
# Not part of `make check`. See README > Development.
E2E_SCRIPT ?= tests/run_all_e2e.sh
e2e:
	@if [ -x "$(E2E_SCRIPT)" ]; then ./$(E2E_SCRIPT); else echo "e2e: $(E2E_SCRIPT) not present yet - skipped"; fi

# All local quality gates AND the E2E smoke
check-all: check e2e

help:
	@echo "Available targets:"
	@echo "  make (or make all)  - Alias for 'make check'"
	@echo "  make check          - All gates (fmt-check, clippy, test, doc, dupes, publish-check, version-check, changelog-check)"
	@echo "  make test-lib       - Library unit tests only (fast inner loop, not a gate)"
	@echo "  make check-all      - Quality gates AND the E2E smoke against real browsers"
	@echo "  make fmt            - Auto-fix formatting"
	@echo "  make fmt-check      - Verify formatting"
	@echo "  make clippy         - Lints with -D warnings on every target"
	@echo "  make clippy-windows - Same lints compiled for Windows (cross-check from Linux)"
	@echo "  make test           - Unit tests, integration tests and doctests"
	@echo "  make doc            - Rustdoc with -D warnings"
	@echo "  make dupes          - Duplication check (auto-installs cargo-dupes $(DUPES_VERSION))"
	@echo "  make dupes-cleanup  - Show stale duplication suppressions"
	@echo "  make publish-check  - cargo publish --dry-run"
	@echo "  make version-check  - package.json version equals Cargo.toml's"
	@echo "  make build          - Release binary"
	@echo "  make install        - Release binary into ~/.cargo/bin"
	@echo "  make e2e            - Smoke against real Chrome and Firefox (tests/run_all_e2e.sh)"
