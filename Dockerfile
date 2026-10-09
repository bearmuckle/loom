# Builds the standalone `loom-server` backend and packages it into a slim
# runtime image.
#
#   docker build -t loom-server .
#   docker run --rm loom-server --version
#
# The backend is AGPL-3.0-only, so the license texts ship inside the image.
# `loom-server` is guarded by a single bearer token, and this image's default
# command publishes a plaintext `ws://` port with the explicit
# `--allow-insecure-remote` opt-in; see SECURITY.md before publishing its port
# anywhere untrusted.

FROM rust:1.98-bookworm AS builder

WORKDIR /src

# The `git2` dependency enables the `https` and `ssh` features, and the version
# range libgit2-sys accepts for a *system* libgit2 (1.9.7..1.10.0) is in no
# Debian or Ubuntu release, so libgit2, libssh2 and zlib are built from source.
# That needs CMake, a C toolchain, and the OpenSSL and zlib headers; `rusqlite`
# is bundled, so it needs the C toolchain too.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        build-essential \
        ca-certificates \
        cmake \
        git \
        libssl-dev \
        pkg-config \
        zlib1g-dev \
    && rm -rf /var/lib/apt/lists/*

# Every workspace member is a path dependency of the backend, so Cargo needs the
# whole manifest graph to resolve the build.
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates

# Stamp the release version into the binary. `.git` is not copied into the
# image, so `build.rs` reports the revision passed here rather than the one it
# could not read.
ARG LOOM_BUILD_VERSION=dev
ARG LOOM_GIT_REVISION=unknown
ENV LOOM_BUILD_VERSION=$LOOM_BUILD_VERSION \
    LOOM_GIT_REVISION=$LOOM_GIT_REVISION

RUN cargo build --release --locked --package loom-server --bin loom-server

FROM debian:bookworm-slim AS runtime

# Runtime shared libraries, taken from the built binary's `ldd` output (the list
# is verified in the release workflow's image smoke test, not guessed):
#   libssl3, zlib1g  OpenSSL and zlib that the source-built libgit2 links
#   libgcc-s1        unwinder used by the Rust runtime
#   ca-certificates  outbound HTTPS (provider and forge requests)
#   curl             only backs the HEALTHCHECK probe below
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
        libgcc-s1 \
        libssl3 \
        zlib1g \
    && rm -rf /var/lib/apt/lists/*

RUN useradd --system --create-home --home-dir /var/lib/loom --shell /usr/sbin/nologin loom

COPY --from=builder /src/target/release/loom-server /usr/local/bin/loom-server
COPY LICENSE-AGPL LICENSES.md /usr/share/doc/loom/

USER loom
WORKDIR /var/lib/loom

EXPOSE 8765

# The backend serves an unauthenticated plain-HTTP `/health` endpoint wherever it
# is bound, so this probe mirrors the default `--bind` in `CMD`; overriding
# `--bind` (or publishing the port at a different mapping) means overriding this
# probe as well.
HEALTHCHECK --interval=30s --timeout=5s --start-period=5s --retries=3 \
  CMD curl -fsS http://127.0.0.1:8765/health || exit 1

# Serving requires a bearer token, so the default command fails closed: mount one
# at /etc/loom/token, or override `CMD` with `--token ...`.
#
# `--bind 0.0.0.0` inside the container's own network namespace is what makes
# `-p` reach the backend, and the server refuses a non-loopback plaintext bind
# unless TLS is configured or the operator opts in, so the default command passes
# `--allow-insecure-remote`. That accepts the single shared bearer token and
# every protocol frame travelling unencrypted between this container and its
# clients: publish the port only to trusted peers, or override `CMD` with
# `--tls-cert`/`--tls-key` to serve `wss://` instead. The HEALTHCHECK below
# probes plain HTTP, so a TLS override needs it overridden too.
ENTRYPOINT ["loom-server"]
CMD ["--bind", "0.0.0.0:8765", \
     "--allow-insecure-remote", \
     "--token-file", "/etc/loom/token", \
     "--persistence", "/var/lib/loom/loom.db"]
