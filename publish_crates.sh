#!/usr/bin/env bash

set -euo pipefail

usage() {
    cat <<'EOF'
Publish the complete kcptun-rs Cargo workspace to crates.io.

Usage:
  ./publish_crates.sh              Validate and run a crates.io dry-run
  ./publish_crates.sh --execute    Validate, confirm, then publish for real
  ./publish_crates.sh --execute --yes
                                   Publish without the interactive confirmation

The script intentionally refuses to publish unless:
  - the workspace contains the public installer package `kcptun-rs`;
  - the Git worktree is clean;
  - formatting, tests, and Clippy pass;
  - Cargo can package every workspace member successfully.

Before the first real publish, authenticate once with `cargo login`.
EOF
}

release_mode="dry-run"
release_confirm="prompt"

while (($# > 0)); do
    case "$1" in
        --execute)
            release_mode="execute"
            ;;
        --yes)
            release_confirm="yes"
            ;;
        -h | --help)
            usage
            exit 0
            ;;
        *)
            echo "error: unknown argument: $1" >&2
            usage >&2
            exit 2
            ;;
    esac
    shift
done

release_root="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$release_root"

for release_command in cargo git make rg; do
    if ! command -v "$release_command" >/dev/null 2>&1; then
        echo "error: required command not found: $release_command" >&2
        exit 1
    fi
done

if ! cargo publish --help | rg -q -- '--workspace'; then
    echo "error: this Cargo version does not support 'cargo publish --workspace'" >&2
    echo "       update Rust/Cargo before publishing" >&2
    exit 1
fi

release_metadata="$(cargo metadata --no-deps --format-version 1)"
if ! rg -q '"name":"kcptun-rs"' <<<"$release_metadata"; then
    echo "error: the workspace does not contain the public package 'kcptun-rs'" >&2
    echo "       refusing to publish only the internal support crates" >&2
    exit 1
fi

release_dirty="$(git status --porcelain)"
if [[ -n "$release_dirty" ]]; then
    echo "error: the Git worktree is not clean:" >&2
    printf '%s\n' "$release_dirty" >&2
    echo "       commit or stash these changes before publishing" >&2
    exit 1
fi

echo "==> Running repository release gates"
make gate

echo "==> Verifying every workspace package for crates.io"
cargo publish --workspace --registry crates-io --locked --dry-run

if [[ "$release_mode" == "dry-run" ]]; then
    echo
    echo "Dry-run passed. No package was uploaded."
    echo "Run './publish_crates.sh --execute' to publish the workspace."
    exit 0
fi

if [[ "$release_confirm" != "yes" ]]; then
    if [[ ! -t 0 ]]; then
        echo "error: interactive confirmation unavailable; pass --yes in CI" >&2
        exit 1
    fi

    echo
    echo "This will permanently upload new crate versions to crates.io."
    read -r -p "Type 'publish kcptun-rs' to continue: " release_answer
    if [[ "$release_answer" != "publish kcptun-rs" ]]; then
        echo "Publish cancelled."
        exit 1
    fi
fi

echo "==> Publishing the complete workspace to crates.io"
cargo publish --workspace --registry crates-io --locked

echo
echo "Published the kcptun-rs workspace successfully."
