#!/bin/sh
# Tests scripts/install.sh against a local directory of fake release assets.
#
#   sh scripts/install.test.sh
set -eu

here="$(cd "$(dirname "$0")" && pwd)"
installer="$here/install.sh"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

failures=0
pass() { printf 'ok   %s\n' "$1"; }
flunk() { printf 'FAIL %s\n' "$1"; failures=$((failures + 1)); }
expect() { # description, command...
    description="$1"
    shift
    if "$@"; then pass "$description"; else flunk "$description"; fi
}
refuse() { # description, command...: the command must fail
    description="$1"
    shift
    if "$@" >/dev/null 2>&1; then flunk "$description"; else pass "$description"; fi
}

case "$(uname -s)-$(uname -m)" in
    Linux-x86_64 | Linux-amd64) platform=linux-x86_64 ;;
    Linux-aarch64 | Linux-arm64) platform=linux-aarch64 ;;
    Darwin-*) platform=macos-universal ;;
    *) echo "skipped: no release platform for this machine"; exit 0 ;;
esac

tag=v9.9.9
assets="$work/assets"
mkdir -p "$assets" "$work/server" "$work/client"
printf '#!/bin/sh\necho fake loom-server\n' > "$work/server/loom-server"
printf '#!/bin/sh\necho fake loom-ui\n' > "$work/client/loom-ui"
cp "$work/server/loom-server" "$work/client/loom-server"
tar -czf "$assets/loom-server-$tag-$platform.tar.gz" -C "$work/server" loom-server
tar -czf "$assets/loom-ui-$tag-$platform.tar.gz" -C "$work/client" loom-ui loom-server
(cd "$assets" && if command -v sha256sum >/dev/null 2>&1; then sha256sum -- *; else shasum -a 256 -- *; fi > SHA256SUMS)

run() { # prefix, args...
    prefix="$1"
    shift
    LOOM_INSTALL_BASE_URL="file://$assets" sh "$installer" --prefix "$prefix" --version "$tag" "$@"
}

# A default install puts only the server in place, and it runs.
p1="$work/p1"
run "$p1" >/dev/null 2>&1
expect "installs loom-server by default" test -x "$p1/bin/loom-server"
expect "does not install the client by default" test ! -e "$p1/bin/loom-ui"
expect "the installed server runs" test "$("$p1/bin/loom-server")" = "fake loom-server"
expect "leaves no temporary files in bin" test "$(ls -A "$p1/bin" | wc -l | tr -d ' ')" = 1

# --client adds the app, and a second run upgrades in place.
run "$p1" --client >/dev/null 2>&1
expect "--client installs loom-ui" test -x "$p1/bin/loom-ui"
expect "--client keeps loom-server" test -x "$p1/bin/loom-server"

# A version without the leading v is accepted.
p2="$work/p2"
LOOM_INSTALL_BASE_URL="file://$assets" sh "$installer" --prefix "$p2" --version 9.9.9 >/dev/null 2>&1
expect "accepts a version without the v" test -x "$p2/bin/loom-server"

# --dry-run changes nothing.
p3="$work/p3"
run "$p3" --dry-run >/dev/null 2>&1
expect "--dry-run installs nothing" test ! -e "$p3"

# Uninstall removes the binaries.
run "$p1" --uninstall >/dev/null 2>&1
expect "--uninstall removes loom-server" test ! -e "$p1/bin/loom-server"
expect "--uninstall removes loom-ui" test ! -e "$p1/bin/loom-ui"

# A tampered archive installs nothing.
p4="$work/p4"
tampered="$work/tampered"
cp -R "$assets" "$tampered"
printf 'x' >> "$tampered/loom-server-$tag-$platform.tar.gz"
refuse "refuses a checksum mismatch" \
    env LOOM_INSTALL_BASE_URL="file://$tampered" sh "$installer" --prefix "$p4" --version "$tag"
expect "a mismatch installs nothing" test ! -e "$p4/bin/loom-server"

# An archive with no SHA256SUMS entry is refused.
p5="$work/p5"
nosums="$work/nosums"
cp -R "$assets" "$nosums"
: > "$nosums/SHA256SUMS"
refuse "refuses an archive missing from SHA256SUMS" \
    env LOOM_INSTALL_BASE_URL="file://$nosums" sh "$installer" --prefix "$p5" --version "$tag"
expect "a missing entry installs nothing" test ! -e "$p5/bin/loom-server"

# Bad input is refused before anything is downloaded.
refuse "refuses a version that is not a tag" run "$work/p6" --version "v1/../x"
refuse "refuses an unknown option" run "$work/p6" --nope
refuse "refuses a missing release" \
    env LOOM_INSTALL_BASE_URL="file://$work/none" sh "$installer" --prefix "$work/p6" --version "$tag"

# An unwritable prefix is an error, never a sudo.
if [ "$(id -u)" != 0 ]; then
    ro="$work/ro"
    mkdir -p "$ro/bin"
    chmod 555 "$ro/bin"
    refuse "refuses an unwritable prefix" run "$ro"
    chmod 755 "$ro/bin"
fi

# An unsupported platform stops with a pointer to the release page.
fakebin="$work/fakebin"
mkdir -p "$fakebin"
printf '#!/bin/sh\necho Plan9\n' > "$fakebin/uname"
chmod +x "$fakebin/uname"
if [ "$(PATH="$fakebin:$PATH" uname -s)" = Plan9 ]; then
    message="$(PATH="$fakebin:$PATH" sh "$installer" --prefix "$work/p7" --version "$tag" 2>&1 || true)"
    case "$message" in
        *"no Loom release is built for Plan9"*"releases"*) pass "names the release page on an unsupported platform" ;;
        *) flunk "names the release page on an unsupported platform: $message" ;;
    esac
else
    # A shell with a built-in uname (BusyBox) ignores the stand-in.
    echo "skip names the release page on an unsupported platform"
fi

# The script is one function that runs on the last line, so a truncated
# download defines nothing it could execute.
last="$(tail -n 1 "$installer")"
expect "runs main only on the last line" test "$last" = 'main "$@"'
expect "parses as POSIX sh" sh -n "$installer"

if [ "$failures" -ne 0 ]; then
    printf '%s test(s) failed\n' "$failures"
    exit 1
fi
printf 'all install tests passed\n'
