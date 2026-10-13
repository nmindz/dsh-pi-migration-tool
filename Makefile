# pi-transfuse — session migration between DSH, Pi, and Claude Code

BINARY := pi-transfuse
CARGO ?= cargo
INSTALL_DIR ?= $(HOME)/.local/bin
TARGET ?= $(shell rustc -vV | awk '/^host:/ { print $$2 }')
DIST_DIR := dist

# Resolved from cargo metadata so CARGO_TARGET_DIR / target-dir overrides are honored. Lazy + memoized.
TARGET_DIR = $(or $(_TARGET_DIR),$(eval _TARGET_DIR := $(shell $(CARGO) metadata --format-version 1 --no-deps | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p'))$(_TARGET_DIR))

.DEFAULT_GOAL := build

# ── Build ───────────────────────────────────────────────────────────────────

.PHONY: build
build: ## Build debug binary
	$(CARGO) build
	@$(MAKE) --no-print-directory prune-incremental

.PHONY: release
release: ## Build optimized release binary
	$(CARGO) build --release
	@$(MAKE) --no-print-directory prune-incremental

.PHONY: install
install: release ## Build release and install to ~/.local/bin
	@mkdir -p $(INSTALL_DIR)
	install -m 755 $(TARGET_DIR)/release/$(BINARY) $(INSTALL_DIR)/$(BINARY)
	@echo "✓ Installed $(BINARY) to $(INSTALL_DIR)/$(BINARY)"

.PHONY: uninstall
uninstall: ## Remove installed binary
	rm -f $(INSTALL_DIR)/$(BINARY)
	@echo "✓ Removed $(BINARY) from $(INSTALL_DIR)"

# ── Quality ─────────────────────────────────────────────────────────────────

.PHONY: check
check: ## Run cargo check (fast type checking)
	$(CARGO) check

.PHONY: clippy
clippy: ## Run clippy with warnings as errors
	$(CARGO) clippy --all-targets -- -D warnings

.PHONY: fmt
fmt: ## Format code with rustfmt
	$(CARGO) fmt

.PHONY: fmt-check
fmt-check: ## Check formatting without modifying files
	$(CARGO) fmt -- --check

.PHONY: lint
lint: fmt-check clippy ## Run all linting (format check + clippy)

# ── Test ────────────────────────────────────────────────────────────────────

.PHONY: test
test: ## Run all tests
	$(CARGO) test

.PHONY: test-verbose
test-verbose: ## Run all tests with verbose output
	$(CARGO) test -- --nocapture

.PHONY: test-unit
test-unit: ## Run unit tests only (bin module tests)
	$(CARGO) test --bins

.PHONY: test-all
test-all: lint test ## Run linting + all tests

.PHONY: ci
ci: fmt-check clippy test release ## Full CI pipeline (fmt + clippy + test + release build)

# ── Clean ───────────────────────────────────────────────────────────────────

.PHONY: clean
clean: ## Remove build artifacts
	$(CARGO) clean

.PHONY: dist-clean
dist-clean: clean ## Remove build artifacts, dist dir, and lock file
	rm -rf $(DIST_DIR)
	rm -f Cargo.lock

# rustc never deletes old incremental session dirs. Keep the newest per profile;
# scoped to $(BINARY)-* because the target dir may be shared across projects.
.PHONY: prune-incremental
prune-incremental: ## Prune stale incremental-compilation caches
	@for dir in $(TARGET_DIR)/debug/incremental $(TARGET_DIR)/release/incremental; do \
	  [ -d "$$dir" ] || continue; \
	  latest=$$(ls -dt "$$dir"/$(BINARY)-* 2>/dev/null | head -1); \
	  for d in "$$dir"/$(BINARY)-*; do \
	    if [ -e "$$d" ] && [ "$$d" != "$$latest" ]; then rm -rf "$$d"; fi; \
	  done; \
	done

# ── Run ─────────────────────────────────────────────────────────────────────

.PHONY: run
run: build ## Run in debug mode (pass ARGS="list" or ARGS="convert <id>" for subcommands)
	$(CARGO) run -- $(ARGS)

# ── Distribution ────────────────────────────────────────────────────────────

.PHONY: dist
dist: ## Build release binary for TARGET and package as tar.gz
	$(CARGO) build --release --target $(TARGET)
	@mkdir -p $(DIST_DIR)
	tar -czf $(DIST_DIR)/$(BINARY)-$(TARGET).tar.gz -C $(TARGET_DIR)/$(TARGET)/release $(BINARY)
	@echo "✓ $(DIST_DIR)/$(BINARY)-$(TARGET).tar.gz"

.PHONY: dist-list
dist-list: ## List packaged release artifacts
	@ls -lh $(DIST_DIR)/ 2>/dev/null || echo "No artifacts — run make dist"

.PHONY: help
help: ## Show this help
	@grep -E '^[a-zA-Z0-9_-]+:.*##' $(MAKEFILE_LIST) | sort | awk 'BEGIN {FS = ":.*##"}; {printf "\033[36m%-22s\033[0m %s\n", $$1, $$2}'
