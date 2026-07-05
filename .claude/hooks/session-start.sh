#!/bin/bash
# SessionStart hook: make `cargo fmt`, `cargo clippy`, and `cargo test` work in
# a fresh Claude Code on the web container, so a session never ships a diff that
# CI (.github/workflows/ci.yml) would reject on formatting/lints.
#
# Runs synchronously (the toolchain must be ready before the agent lints or
# tests). Idempotent and non-interactive — safe to re-run on resume/clear.
set -euo pipefail

# CI links fontconfig via dlopen at runtime; mirror those env vars for the whole
# session so rendering/tests behave exactly like CI.
if [ -n "${CLAUDE_ENV_FILE:-}" ]; then
  {
    echo 'export RUST_FONTCONFIG_DLOPEN=on'
    echo 'export FONTCONFIG_NO_PKG_CONFIG=1'
  } >> "$CLAUDE_ENV_FILE"
fi

# Nothing to install outside the managed web container.
if [ "${CLAUDE_CODE_REMOTE:-}" != "true" ]; then
  exit 0
fi

cd "${CLAUDE_PROJECT_DIR:-.}"

# The lint job needs these components; a bare stable toolchain may lack them.
if command -v rustup >/dev/null 2>&1; then
  rustup component add rustfmt clippy >/dev/null 2>&1 || true
fi

# Pre-fetch crate dependencies so the first build/test isn't a cold download.
cargo fetch --locked >/dev/null 2>&1 || cargo fetch >/dev/null 2>&1 || true
