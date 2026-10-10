#!/bin/sh
# The container health probe for `loom-server`.
#
# Purpose: report whether the `loom-server` process in this container answers on
# the address it was *told* to serve. The image's default command can be
# overridden with `--bind`, `--tls-cert` and `--tls-key`, so the probe derives
# host, port and scheme from the running server's own argument list instead of
# curling a fixed address; a hardcoded probe reports a perfectly healthy
# listener as unhealthy whenever a deployment moves the port or terminates TLS
# (loom issue #241). The Dockerfile's HEALTHCHECK runs this script.
#
# Usage: /usr/local/bin/loom-server-healthcheck
#   Exits 0 when `GET <scheme>://<host>:<port>/health` answers, non-zero
#   otherwise. Nothing is printed on success; a failure names the probed URL on
#   stderr and leaves curl's own error there too.
#
# Environment:
#   LOOM_HEALTHCHECK_URL      Full URL to probe. Overrides everything below, for
#                             deployments that terminate TLS in front of the
#                             container or route the probe elsewhere.
#   LOOM_HEALTHCHECK_CMDLINE  File holding the NUL-separated command line to
#                             read, defaulting to /proc/1/cmdline. This is a
#                             test seam: it lets the parsing below be exercised
#                             against a fixture outside a container.
#
# `curl` is the only external command used, and the runtime image installs it to
# back this probe.

set -eu

# The container runs `loom-server` as PID 1, and the health check runs in that
# same container, so /proc/1/cmdline is the entrypoint plus whatever command
# overrode its default. Arguments are NUL-separated, one per argv entry, and can
# contain spaces, so they are turned into lines and read one at a time rather
# than word-split. An unreadable file parses as no arguments, which falls back to
# the documented default probe below rather than failing silently.
cmdline_file=${LOOM_HEALTHCHECK_CMDLINE:-/proc/1/cmdline}

# Parses the command line and prints `<scheme> <bind>`. Both argument forms the
# server accepts are handled: `--bind <addr>` and `--bind=<addr>`. A `--tls-cert`
# anywhere in the argument list means the listener serves `https://`; the server
# requires the matching `--tls-key` alongside it.
parse_cmdline() {
  bind=
  scheme=http
  expect_bind=0
  while IFS= read -r argument; do
    if [ "$expect_bind" = 1 ]; then
      bind=$argument
      expect_bind=0
      continue
    fi
    case $argument in
      --bind) expect_bind=1 ;;
      --bind=*) bind=${argument#--bind=} ;;
      --tls-cert | --tls-cert=*) scheme=https ;;
    esac
  done
  printf '%s %s\n' "$scheme" "$bind"
}

if [ -n "${LOOM_HEALTHCHECK_URL:-}" ]; then
  url=$LOOM_HEALTHCHECK_URL
  # The override names its own scheme; only an explicit `https` needs the
  # certificate-tolerant probe below.
  case $url in
    https://* | HTTPS://*) scheme=https ;;
    *) scheme=http ;;
  esac
else
  resolved=$(tr '\0' '\n' <"$cmdline_file" | parse_cmdline)
  scheme=${resolved%% *}
  bind=${resolved#* }

  # `--bind` is a full socket address, either `host:port` or `[ipv6]:port`.
  host=
  port=
  case $bind in
    \[*\]:[0-9]*)
      host=${bind%%]*} # "[::1" -- everything before the closing bracket
      host=${host#?}   # drop the opening bracket a URL does not want there
      port=${bind##*:}
      ;;
    *:[0-9]*)
      host=${bind%:*}
      port=${bind##*:}
      ;;
  esac
  # Anything unparsable -- a missing `--bind`, a host without a port, a
  # non-numeric port, or port 0, which has no stable address to probe -- uses the
  # server's documented default, so a completely custom invocation still gets a
  # safe probe instead of no probe at all.
  case $port in
    '' | 0 | *[!0-9]*) host=127.0.0.1; port=8765 ;;
  esac
  # A wildcard IPv6 listener always accepts `::1`; IPv4 loopback only does when
  # the socket is not IPv6-only, so a wildcard `::` bind is probed on `::1`. Any
  # other IPv6 literal needs brackets in a URL.
  case $host in
    '' | 0.0.0.0) host=127.0.0.1 ;;
    ::) host='[::1]' ;;
    *:*) host="[$host]" ;;
  esac
  url="$scheme://$host:$port/health"
fi

fail() {
  printf 'loom-server-healthcheck: no answer from %s\n' "$1" >&2
  exit 1
}

if [ "$scheme" = https ]; then
  # `-k` because the probe certificate may be self-signed, or issued for an
  # external name rather than the address this probe dials; the check is that the
  # listener completes a TLS handshake and answers, not that its certificate is
  # trusted here.
  curl -fsSk --connect-timeout 3 --max-time 5 -o /dev/null "$url" || fail "$url"
else
  curl -fsS --connect-timeout 3 --max-time 5 -o /dev/null "$url" || fail "$url"
fi
