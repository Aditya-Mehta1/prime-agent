# Merge gate (AGENTS.md): fmt + clippy + test + release build must pass before every merge.
check:
	cargo fmt --all --check
	cargo clippy --workspace --all-targets -- -D warnings
	cargo test --workspace
	cargo build --release --workspace

# Supply-chain gates (docs/installer-ci-design.md §7/§9) — local mirrors of the
# ci.yml workflow jobs. They fail loudly when the tool is missing instead of
# silently skipping the gate.

deny:
	@command -v cargo-deny >/dev/null 2>&1 || { echo "cargo-deny not installed (cargo install cargo-deny --locked)"; exit 1; }
	cargo deny --all-features --workspace check advisories licenses

# Lints the staged workflow files (see ci/workflows/README.md for why they are
# not under .github/ yet).
actionlint:
	@command -v actionlint >/dev/null 2>&1 || { echo "actionlint not installed (see rhysd/actionlint releases)"; exit 1; }
	actionlint ci/workflows/ci.yml ci/workflows/release.yml

# Local mirror of the release build-job gates (docs/installer-ci-design.md §9):
# release build against the committed lockfile, deterministic tarball assembly,
# then end-to-end verification of the host-target artifact. Until the
# kernel-packaging lane merges (which vendors prime-agent-runtime/ at the repo
# root), pass RUNTIME_DIR=<worktree>/prime-agent-runtime.
VERSION := $(shell sed -n 's/^version *= *"\([^"]*\)".*/\1/p' Cargo.toml | head -1)
TARGET := $(shell rustc -vV | sed -n 's/^host: //p')
RUNTIME_DIR ?=
RUNTIME_FLAG = $(if $(RUNTIME_DIR),--runtime-dir $(RUNTIME_DIR),)

release-dry-run:
	cargo build --release --locked --workspace
	python3 scripts/release/assemble_artifacts.py \
		--repo-root . --version "$(VERSION)" --target "$(TARGET)" $(RUNTIME_FLAG) \
		--out-dir target/release/dist
	python3 scripts/release/verify_release.py \
		--dist-dir target/release/dist --version "$(VERSION)" --target "$(TARGET)"

# Optional hardening: embed the dependency list in the binary for incident
# response (docs/installer-ci-design.md §7).
audit-build:
	@command -v cargo-auditable >/dev/null 2>&1 || { echo "cargo-auditable not installed (cargo install cargo-auditable --locked)"; exit 1; }
	cargo auditable build --release --locked --workspace

.PHONY: check deny actionlint release-dry-run audit-build
