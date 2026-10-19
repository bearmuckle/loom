#!/bin/sh
# Makes the Rust toolchain reachable for every command on a worker node.
#
# Agents run commands without a login shell, so a toolchain installed below
# `$HOME/.cargo/bin` is invisible when that directory is not on `PATH`:
# `cargo` then fails to start and the run reports the toolchain as missing.
# This script finds the toolchain (installing it with `--install` when the node
# has none), links its proxies into a directory that is already on `PATH`, and
# reports what it resolved.
#
# It is idempotent. Run it when a node is provisioned, and whenever a command
# reports that `cargo`, `rustfmt`, or `clippy` is missing.
#
#   ./scripts/agent-toolchain.sh            # link what the node already has
#   ./scripts/agent-toolchain.sh --install  # install Rust first when missing
#
# The links are rustup proxies, so they resolve whatever toolchain rustup has
# as its default; install or pin components with rustup itself.

set -eu

install=no
for argument in "$@"; do
    case "$argument" in
        --install)
            install=yes
            ;;
        -h | --help)
            sed -n '2,18p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            echo "unknown argument: $argument" >&2
            exit 2
            ;;
    esac
done

# A build of this workspace needs the first line; the second is what the
# documented optional checks and the faster-builds advice use.
required="cargo rustc rustup rustfmt rustdoc cargo-fmt cargo-clippy clippy-driver"
optional="cargo-llvm-cov sccache"

on_path() {
    case ":${PATH}:" in
        *":$1:"*) return 0 ;;
        *) return 1 ;;
    esac
}

# The directory holding `cargo`, preferring an existing `PATH` entry so a node
# that is already correct is left alone.
find_toolchain_bin() {
    if command -v cargo >/dev/null 2>&1; then
        dirname "$(command -v cargo)"
        return 0
    fi
    for home in "${CARGO_HOME:-}" "$HOME/.cargo" /usr/local/cargo /opt/cargo /opt/rust /root/.cargo; do
        [ -n "$home" ] || continue
        if [ -x "$home/bin/cargo" ]; then
            printf '%s\n' "$home/bin"
            return 0
        fi
    done
    return 1
}

if ! toolchain_bin="$(find_toolchain_bin)"; then
    if [ "$install" != yes ]; then
        cat >&2 <<'END'
No Rust toolchain found on this node.

Run this script with --install to install one with rustup, or install Rust 1.99
or newer yourself and run it again. Without a toolchain, none of the Rust
checks in AGENTS.md can run.
END
        exit 1
    fi
    if ! command -v curl >/dev/null 2>&1; then
        echo "installing Rust needs curl, which this node does not have" >&2
        exit 1
    fi
    echo "installing Rust with rustup into ${CARGO_HOME:-$HOME/.cargo}"
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs |
        sh -s -- -y --profile default --default-toolchain 1.99
    if ! toolchain_bin="$(find_toolchain_bin)"; then
        echo "rustup finished but no cargo appeared; check ${CARGO_HOME:-$HOME/.cargo}" >&2
        exit 1
    fi
fi

# A link directory has to be writable and already on PATH, because a directory
# that still has to be added to PATH is invisible to the next command an agent
# runs.
link_dir=""
for candidate in /usr/local/bin "$HOME/.local/bin" /usr/bin; do
    if [ -d "$candidate" ] && [ -w "$candidate" ] && on_path "$candidate"; then
        link_dir="$candidate"
        break
    fi
done

if [ "$link_dir" = "$toolchain_bin" ]; then
    printf 'the toolchain in %s is already on PATH
' "$toolchain_bin"
elif [ -n "$link_dir" ]; then
    for name in $required $optional; do
        source_path="$toolchain_bin/$name"
        [ -x "$source_path" ] || continue
        if [ "$(readlink -f "$link_dir/$name" 2>/dev/null || true)" = "$(readlink -f "$source_path")" ]; then
            continue
        fi
        ln -sfn "$source_path" "$link_dir/$name"
    done
    printf 'linked the toolchain in %s into %s\n' "$toolchain_bin" "$link_dir"
else
    printf 'warning: no writable directory on PATH to link into\n' >&2
    printf 'warning: call cargo as %s/cargo until the node provisioner adds\n' "$toolchain_bin" >&2
    printf 'warning: %s to PATH for non-login shells\n' "$toolchain_bin" >&2
fi

missing=0
echo
echo "resolved toolchain:"
for name in $required; do
    if resolved="$(command -v "$name" 2>/dev/null)"; then
        printf '  %-14s %s\n' "$name" "$resolved"
    else
        printf '  %-14s MISSING from PATH\n' "$name" >&2
        missing=1
    fi
done

echo
"$toolchain_bin/cargo" --version
"$toolchain_bin/rustup" show active-toolchain 2>/dev/null || true

# `rustup show active-toolchain` prints nothing when no default toolchain is
# set, which leaves every proxy failing, so name that case instead of
# reporting success.
if ! "$toolchain_bin/cargo" --version >/dev/null 2>&1; then
    echo "cargo is linked but cannot run; set a default with 'rustup default stable'" >&2
    missing=1
fi

host="$("$toolchain_bin/rustc" -vV 2>/dev/null | sed -n 's/^host: //p')"
for component in rustfmt clippy; do
    if ! "$toolchain_bin/rustup" component list --installed 2>/dev/null | grep -qx "$component-$host"; then
        printf 'missing component: %s (rustup component add %s)\n' "$component" "$component" >&2
        missing=1
    fi
done

# The wasm client is part of the workspace, so a node that builds it needs the
# target; it is reported rather than required, because native-only checks pass
# without it.
if ! "$toolchain_bin/rustup" target list --installed 2>/dev/null | grep -qx wasm32-unknown-unknown; then
    echo "note: wasm32-unknown-unknown is not installed (rustup target add wasm32-unknown-unknown)"
fi

echo
if command -v sccache >/dev/null 2>&1; then
    sccache --version
else
    echo "note: sccache is not installed (cargo install --locked sccache)"
fi

exit "$missing"
