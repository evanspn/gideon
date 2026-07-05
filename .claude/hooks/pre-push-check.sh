#!/bin/bash
# PreToolUse(Bash) hook: a gate that runs ONLY when a session is about to
# `git push`. It mirrors CI's fast, deterministic checks (.github/workflows/
# ci.yml — rustfmt + clippy) and blocks the push if they fail, so no session
# ever ships a diff that CI would reject on formatting or lints.
#
# Why a push gate rather than a Stop hook: this fires at the one moment code
# leaves for CI, not on every turn — no per-message latency during rapid
# iteration. Exit 2 tells Claude Code to block the tool call and feeds the
# message back so it can fix and retry.
set -uo pipefail

# The tool call is delivered as JSON on stdin: {tool_name, tool_input:{command}}.
input="$(cat)"
command="$(printf '%s' "$input" | python3 -c \
  'import json,sys; print(json.load(sys.stdin).get("tool_input",{}).get("command",""))' \
  2>/dev/null || true)"

# Only gate git pushes; every other command passes straight through.
case "$command" in
  *"git push"*) ;;
  *) exit 0 ;;
esac

cd "${CLAUDE_PROJECT_DIR:-.}" || exit 0

# fontconfig is dlopen'd at runtime, same as CI.
export RUST_FONTCONFIG_DLOPEN=on
export FONTCONFIG_NO_PKG_CONFIG=1

if ! fmt_out="$(cargo fmt --all -- --check 2>&1)"; then
  echo "Blocked git push: formatting is not clean. Run 'cargo fmt --all', then push again." >&2
  echo "$fmt_out" | head -30 >&2
  exit 2
fi

if ! clippy_out="$(cargo clippy --workspace --all-targets -- -D warnings 2>&1)"; then
  echo "Blocked git push: clippy found warnings/errors (CI runs with -D warnings). Fix them, then push again." >&2
  echo "$clippy_out" | tail -40 >&2
  exit 2
fi

exit 0
