#!/bin/sh
# Installs Loom from a GitHub release.
#
#   curl -fsSL https://github.com/bearmuckle/loom/releases/latest/download/install.sh | sh
#   curl -fsSL https://github.com/bearmuckle/loom/releases/latest/download/install.sh | sh -s -- --client
#
# By default it installs `loom-server`, the backend, into ~/.local/bin. With
# --client it installs the desktop client `loom-ui` as well; the client archive
# carries its own `loom-server`, so that is the one that is installed.
#
#   --client            install the desktop client and the server it starts
#   --version <tag>     install this release (for example v0.16.0), not the latest
#   --prefix <dir>      install into <dir>/bin (default: ~/.local)
#   --dry-run           print what would happen and change nothing
#   --uninstall         remove the binaries this script installs
#   -h, --help          print this help
#
# Environment: LOOM_VERSION and LOOM_PREFIX stand in for --version and --prefix.
#
# What it does and does not do:
#   - Linux (x86_64, aarch64) and macOS only; any other platform stops with a
#     pointer to the release page. Windows archives are on the release page.
#   - Every download is checked against the release's SHA256SUMS before it is
#     unpacked. A mismatch installs nothing.
#   - It never runs sudo and never edits a shell profile. A prefix it cannot
#     write to is an error; it tells you what to do about PATH instead.
#   - Re-running it upgrades. --uninstall removes only the binaries; the
#     state a worker keeps (see docs/deployment.md) is left alone.
#
# The whole script is one function that runs on the last line, so a download
# that is cut off part-way through never executes a partial command.

set -eu

REPOSITORY="bearmuckle/loom"

say() {
    printf '%s\n' "$*"
}

fail() {
    printf 'install.sh: error: %s\n' "$*" >&2
    exit 1
}

usage() {
    cat <<'USAGE'
Installs Loom from a GitHub release.

  curl -fsSL https://github.com/bearmuckle/loom/releases/latest/download/install.sh | sh
  ... | sh -s -- --client

Options:
  --client            install the desktop client (loom-ui) and the server it starts
  --version <tag>     install this release (for example v0.16.0), not the latest
  --prefix <dir>      install into <dir>/bin (default: ~/.local)
  --dry-run           print what would happen and change nothing
  --uninstall         remove the binaries this script installs
  -h, --help          print this help

By default only loom-server is installed. LOOM_VERSION and LOOM_PREFIX stand in
for --version and --prefix. Downloads are checked against the release's
SHA256SUMS; the script never uses sudo and never edits a shell profile.
USAGE
}

have() {
    command -v "$1" >/dev/null 2>&1
}

# Prints the body of URL on stdout.
fetch() {
    if have curl; then
        curl -fsSL --proto '=https,file' --tlsv1.2 "$1"
    elif have wget; then
        wget -qO- "$1"
    else
        fail "curl or wget is required"
    fi
}

# Saves URL to the file DEST.
download() {
    if have curl; then
        curl -fsSL --proto '=https,file' --tlsv1.2 -o "$2" "$1"
    else
        wget -q -O "$2" "$1"
    fi
}

# The release platform label of this machine, as used in archive names.
detect_platform() {
    os="$(uname -s)"
    arch="$(uname -m)"
    case "$os" in
        Linux)
            case "$arch" in
                x86_64 | amd64) echo "linux-x86_64" ;;
                aarch64 | arm64) echo "linux-aarch64" ;;
                *) return 1 ;;
            esac
            ;;
        Darwin)
            case "$arch" in
                x86_64 | arm64 | aarch64) echo "macos-universal" ;;
                *) return 1 ;;
            esac
            ;;
        *) return 1 ;;
    esac
}

# The tag of the newest release, read from the redirect of /releases/latest so
# the GitHub API's rate limit never applies.
latest_tag() {
    if have curl; then
        url="$(curl -fsS --proto '=https' --tlsv1.2 -o /dev/null -w '%{url_effective}' -L \
            "https://github.com/$REPOSITORY/releases/latest")" || return 1
    else
        url="$(wget -q --max-redirect=5 -O /dev/null --server-response \
            "https://github.com/$REPOSITORY/releases/latest" 2>&1 \
            | sed -n 's/^ *Location: //p' | tail -n 1)" || return 1
    fi
    tag="${url##*/}"
    case "$tag" in
        v[0-9]*) echo "$tag" ;;
        *) return 1 ;;
    esac
}

# Prints the SHA-256 of the file named by $1.
sha256_of() {
    if have sha256sum; then
        sha256sum "$1" | cut -d ' ' -f 1
    elif have shasum; then
        shasum -a 256 "$1" | cut -d ' ' -f 1
    else
        fail "sha256sum or shasum is required to verify the download"
    fi
}

# Checks the archive $1, which is the release asset named $2, against the
# SHA256SUMS file $3.
verify() {
    expected="$(awk -v name="$2" '$2 == name || $2 == "*" name { print $1; exit }' "$3")"
    [ -n "$expected" ] || fail "SHA256SUMS has no entry for $2; refusing an unverified download"
    actual="$(sha256_of "$1")"
    [ "$actual" = "$expected" ] || fail "checksum mismatch for $2 (expected $expected, got $actual)"
}

main() {
    client=0
    dry_run=0
    uninstall=0
    version="${LOOM_VERSION:-}"
    prefix="${LOOM_PREFIX:-$HOME/.local}"
    # Tests point this at a local directory of release assets.
    base_url="${LOOM_INSTALL_BASE_URL:-}"

    while [ $# -gt 0 ]; do
        case "$1" in
            --client) client=1 ;;
            --dry-run) dry_run=1 ;;
            --uninstall) uninstall=1 ;;
            --version)
                [ $# -ge 2 ] || fail "--version needs a release tag"
                version="$2"
                shift
                ;;
            --version=*) version="${1#--version=}" ;;
            --prefix)
                [ $# -ge 2 ] || fail "--prefix needs a directory"
                prefix="$2"
                shift
                ;;
            --prefix=*) prefix="${1#--prefix=}" ;;
            -h | --help)
                usage
                return 0
                ;;
            *) fail "unknown option '$1' (try --help)" ;;
        esac
        shift
    done

    bin_dir="$prefix/bin"

    if [ "$uninstall" -eq 1 ]; then
        for name in loom-server loom-ui; do
            if [ -e "$bin_dir/$name" ]; then
                if [ "$dry_run" -eq 1 ]; then
                    say "would remove $bin_dir/$name"
                else
                    rm -f "$bin_dir/$name"
                    say "removed $bin_dir/$name"
                fi
            fi
        done
        return 0
    fi

    platform="$(detect_platform)" ||
        fail "no Loom release is built for $(uname -s) $(uname -m); see https://github.com/$REPOSITORY/releases for the available archives"

    if [ -z "$version" ]; then
        version="$(latest_tag)" || fail "could not find the latest release; pass --version <tag>"
    fi
    case "$version" in
        v[0-9]*) ;;
        [0-9]*) version="v$version" ;;
        *) fail "'$version' is not a release tag such as v0.16.0" ;;
    esac
    # A tag becomes part of a URL and a file name, so keep it to what a tag is.
    case "$version" in
        *[!A-Za-z0-9._-]*) fail "'$version' is not a release tag such as v0.16.0" ;;
    esac

    if [ "$client" -eq 1 ]; then
        component="loom-ui"
        binaries="loom-ui loom-server"
    else
        component="loom-server"
        binaries="loom-server"
    fi
    asset="$component-$version-$platform.tar.gz"
    if [ -z "$base_url" ]; then
        base_url="https://github.com/$REPOSITORY/releases/download/$version"
    fi

    say "Installing Loom $version ($component, $platform) into $bin_dir"
    if [ "$dry_run" -eq 1 ]; then
        say "would download $base_url/$asset"
        say "would verify it against $base_url/SHA256SUMS"
        for name in $binaries; do
            say "would install $bin_dir/$name"
        done
        return 0
    fi

    have tar || fail "tar is required"
    mkdir -p "$bin_dir" 2>/dev/null || true
    [ -d "$bin_dir" ] && [ -w "$bin_dir" ] ||
        fail "cannot write to $bin_dir; choose another directory with --prefix (this script never uses sudo)"

    work="$(mktemp -d "${TMPDIR:-/tmp}/loom-install.XXXXXX")" || fail "could not create a temporary directory"
    trap 'rm -rf "$work"' EXIT INT TERM

    say "Downloading $asset"
    download "$base_url/$asset" "$work/$asset" || fail "could not download $base_url/$asset"
    download "$base_url/SHA256SUMS" "$work/SHA256SUMS" || fail "could not download $base_url/SHA256SUMS"
    verify "$work/$asset" "$asset" "$work/SHA256SUMS"
    say "Checksum verified"

    mkdir "$work/unpacked"
    tar -xzf "$work/$asset" -C "$work/unpacked" || fail "could not unpack $asset"
    for name in $binaries; do
        [ -f "$work/unpacked/$name" ] || fail "$asset does not contain $name"
    done
    for name in $binaries; do
        chmod 755 "$work/unpacked/$name"
        # Moved into place from the same directory, so a running binary is
        # replaced atomically instead of being overwritten in place.
        cp "$work/unpacked/$name" "$bin_dir/.$name.new"
        mv -f "$bin_dir/.$name.new" "$bin_dir/$name"
        say "Installed $bin_dir/$name"
    done

    case ":$PATH:" in
        *":$bin_dir:"*) ;;
        *)
            say ""
            say "$bin_dir is not on your PATH. Add it, for example:"
            say "  export PATH=\"$bin_dir:\$PATH\""
            ;;
    esac

    say ""
    say "Next steps:"
    say "  loom-server --help           start a worker (see docs/deployment.md)"
    if [ "$client" -eq 1 ]; then
        say "  loom-ui                      open the desktop client"
    else
        say "  Re-run with --client to add the desktop client."
    fi
    say "  Upgrade by running this script again; remove with --uninstall."
}

main "$@"
