# syntax=docker/dockerfile:1.7
#
# Mezame on Alpine (musl), built from this repository's source.
#
# Two stages. The builder carries Rust and Node.js, because build.rs runs
# the Vite build and embeds the bundle in the binary. The runtime carries
# the binary and a certificate bundle, and nothing else.
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
# See compose.yaml for the volume and port wiring.

# Alpine 3.23 is the first release whose `nodejs` package is 24, which is
# the floor the UI build needs. The release is pinned in the tag for that
# reason: a builder on an older Alpine would install Node.js 22 and the
# build would fail the version check in build.rs.
FROM rust:1-alpine3.23 AS builder

# musl-dev supplies the C runtime headers and static archives the musl
# target links against; the image already carries gcc as the linker
# driver. nodejs and npm are for the UI build.
RUN apk add --no-cache musl-dev nodejs npm

WORKDIR /src
COPY . .

# --locked so the build resolves exactly what Cargo.lock records.
RUN cargo build --release --locked

FROM alpine:3.23

# The whole install list. The musl target links the Rust standard
# library, the unwinder and the C runtime statically, so the binary needs
# nothing else from the image. The certificate bundle is what an outbound
# HTTPS call will read.
RUN apk add --no-cache ca-certificates

COPY --from=builder /src/target/release/mezame /usr/local/bin/mezame

EXPOSE 9510

# Config and cross-device UI state. Mount a named volume here so
# `mezame init` is answered once.
VOLUME ["/root/.mezame"]

CMD ["mezame"]
