# Build and install: `make && make install` (PG_CONFIG selects the PostgreSQL installation).
PG_CONFIG ?= pg_config
CARGO ?= cargo
PG_MAJOR := $(shell $(PG_CONFIG) --version | sed -E 's/^PostgreSQL ([0-9]+).*/\1/')
FEATURES := --no-default-features --features pg$(PG_MAJOR)
# Majors for the test matrix; they must be registered with `cargo pgrx init`.
PG_VERSIONS ?= 15 16 17 18
FUZZ_SECONDS ?= 60
# Toolchain for Miri and fuzzing (CI pins a dated nightly).
NIGHTLY ?= nightly
# Extra flags for `cargo pgrx install`, e.g. --sudo to install into root-owned system directories.
PGRX_INSTALL_FLAGS ?=
# Documentation site generator, run through uvx.
ZENSICAL ?= uvx zensical==0.1.2
# Versioned docs: Zensical's fork of mike (not on PyPI), pinned to a commit.
MIKE ?= uvx --with zensical==0.1.2 --from git+https://github.com/squidfunk/mike.git@2d4ad799442f4592db8ad53b179bfb33db8c69ac mike

.PHONY: all install test test-core test-matrix miri fuzz clippy crosscheck cborx-check docs docs-serve docs-deploy

# pgrx needs the PostgreSQL installation registered once. Building and installing use a project-local pgrx home,
# so a developer's ~/.pgrx (used by the test targets) is never modified.
BUILD_PGRX_HOME := $(CURDIR)/target/pgrx-home
all install: export PGRX_HOME := $(BUILD_PGRX_HOME)
PGRX_REGISTER = grep -qs '^pg$(PG_MAJOR) *= *"$(shell $(PG_CONFIG) --bindir)/pg_config"' $(BUILD_PGRX_HOME)/config.toml \
	|| (mkdir -p $(BUILD_PGRX_HOME) && $(CARGO) pgrx init --pg$(PG_MAJOR) $(PG_CONFIG))

all:
	$(PGRX_REGISTER)
	cd postcbor && $(CARGO) pgrx package --pg-config $(PG_CONFIG) $(FEATURES)

install:
	$(PGRX_REGISTER)
	cd postcbor && $(CARGO) pgrx install --release --pg-config $(PG_CONFIG) $(FEATURES) $(PGRX_INSTALL_FLAGS)

test-core:
	$(CARGO) test -p postcbor-core

# SQL tests against pgrx's PostgreSQL of the same major as PG_CONFIG.
test: test-core
	cd postcbor && $(CARGO) pgrx regress pg$(PG_MAJOR) --resetdb $(FEATURES)

test-matrix: test-core
	set -e; for v in $(PG_VERSIONS); do \
		(cd postcbor && $(CARGO) pgrx regress pg$$v --resetdb --no-default-features --features pg$$v); \
	done

miri:
	$(CARGO) +$(NIGHTLY) miri test -p postcbor-core

fuzz:
	set -e; cd fuzz && for t in $$($(CARGO) +$(NIGHTLY) fuzz list); do \
		$(CARGO) +$(NIGHTLY) fuzz run $$t -- -max_total_time=$(FUZZ_SECONDS); \
	done

crosscheck:
	uv run -q --with cbor2 tests/crosscheck.py 10000

# Against real cbor-x: CBORX_NODE_MODULES must contain a cbor-x install (nothing is added to this repository).
cborx-check:
	NODE_PATH=$(CBORX_NODE_MODULES) node tests/cborx/check.mjs

clippy:
	$(CARGO) clippy --all-targets -- -D warnings

# Strict: a broken link fails the build. Builds over docs/site in place, so a running docs-serve keeps working.
docs:
	$(ZENSICAL) build --strict

docs-serve:
	$(ZENSICAL) serve

# Commits the site as DOCS_VERSION (e.g. 0.1, or dev) to the gh-pages branch, with DOCS_ALIAS (e.g. latest) if set.
# CI runs it; MIKE_FLAGS=--push publishes.
docs-deploy:
	test -n "$(DOCS_VERSION)"
	$(MIKE) deploy $(MIKE_FLAGS) --update-aliases $(DOCS_VERSION) $(DOCS_ALIAS)
	$(if $(DOCS_ALIAS),$(MIKE) set-default $(MIKE_FLAGS) $(DOCS_ALIAS))
