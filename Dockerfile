# Mezame on Alpine (musl), built from this repository's source.
#
# Two stages. The builder carries Rust and Node.js, because build.rs runs
# the Vite build and embeds the bundle in the binary. The runtime carries
# the binary and a certificate bundle, and nothing else, and runs as an
# unprivileged user.
#
# First run, one-off setup:
#   docker compose run --rm setup
#   # answer the bind prompt with 0.0.0.0:9510
#   # or, with no terminal:
#   docker compose run -T --rm setup mezame init --bind 0.0.0.0:9510
#
# Subsequent runs:
#   docker compose up -d
#
# See compose.yaml for the volume and port wiring, and for the one-off
# ownership fix a volume created by an earlier, root-running image needs.
#
# Both base images are pinned by tag and by the digest of their multi-arch
# index, so a build resolves the same bytes until the digest is bumped.
# The tag stays in front for the reader and for Dependabot, which refreshes
# the digest when the tag moves. To read a current digest without Docker,
# ask the registry (the same for library/rust and its tag):
#   TOKEN=$(curl -sS "https://auth.docker.io/token?service=registry.docker.io&scope=repository:library/alpine:pull" | sed -n 's/.*"token":"\([^"]*\)".*/\1/p')
#   curl -sSI -H "Authorization: Bearer $TOKEN" -H "Accept: application/vnd.oci.image.index.v1+json" https://registry-1.docker.io/v2/library/alpine/manifests/3.23 | grep -i docker-content-digest
# Pin the index digest, never a per-platform manifest digest: the latter
# would break the build on the other architecture.

# Alpine 3.23 is the first release whose `nodejs` package is 24, which is
# the floor the UI build needs. The release is pinned in the tag for that
# reason: a builder on an older Alpine would install Node.js 22 and the
# build would fail the version check in build.rs.
FROM rust:1-alpine3.23@sha256:4743b6231029d726d7a0f81d730a7c9f4eff23225a4499c01e275efb5e260235 AS builder

# musl-dev supplies the C runtime headers and static archives the musl
# target links against; the image already carries gcc as the linker
# driver. nodejs and npm are for the UI build: nodejs comes from Alpine's
# main repository and npm from community, and the image enables both.
RUN apk add --no-cache musl-dev nodejs npm

WORKDIR /src
COPY . .

# --locked so the build resolves exactly what Cargo.lock records.
RUN cargo build --release --locked

FROM alpine:3.23@sha256:fd791d74b68913cbb027c6546007b3f0d3bc45125f797758156952bc2d6daf40

ARG MEZAME_UID=1000
ARG MEZAME_GID=1000

# The whole install list. The musl target links the Rust standard
# library, the unwinder and the C runtime statically, so the binary needs
# nothing else from the image. The certificate bundle is what an outbound
# HTTPS call will read. The user comes from busybox, no package: uid 1000
# so a future workspace bind mount lines up with the usual first Linux
# account; rebuild with --build-arg MEZAME_UID=$(id -u) for another. Its
# .mezame directory is created and owned here, before VOLUME, so a fresh
# named volume inherits the ownership.
RUN apk add --no-cache ca-certificates \
 && addgroup -g "$MEZAME_GID" mezame \
 && adduser -D -H -u "$MEZAME_UID" -G mezame -h /home/mezame -s /sbin/nologin mezame \
 && mkdir -p /home/mezame/.mezame \
 && chown -R mezame:mezame /home/mezame

COPY --from=builder /src/target/release/mezame /usr/local/bin/mezame

# Explicit: a compose `user:` override to a uid with no passwd entry would
# otherwise get HOME=/ from the runtime, and Mezame reads $HOME.
ENV HOME=/home/mezame
USER mezame

EXPOSE 9510

# Config and cross-device UI state. Mount a named volume here so
# `mezame init` is answered once.
VOLUME ["/home/mezame/.mezame"]

# Liveness, not correctness: one GET of the UI shell on the exposed port,
# from inside the container, with busybox wget. A loopback bind inside the
# container answers this probe and nothing on the published port, so it
# does not catch that prompt answer; a bind on another port makes the
# probe fail, so override or disable the check in compose then. Docker
# reports the state and does not restart on it. The interval is short
# because the first probe runs one full interval after start.
HEALTHCHECK --interval=10s --timeout=3s --start-period=5s --retries=3 \
  CMD ["wget", "-q", "-T", "2", "-O", "/dev/null", "http://127.0.0.1:9510/"]

CMD ["mezame"]
