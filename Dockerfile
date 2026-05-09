# syntax=docker/dockerfile:1.7
#
# Mezame on Alpine (musl), with a working Kiro CLI baked in.
#
# Builds a single image that contains:
#  - Rust stable (to compile mezame from crates.io)
#  - Node.js and npm (mezame's build.rs builds the embedded UI)
#  - Kiro CLI, musl variant, with its bundled glibc-linked Bun 
#    swapped for a musl Bun (the fix for `os error 2` on Alpine)
#  - Mezame, installed from crates.io
#
# First run, one-off setup:
#  docker run -it --rm \
#    -v kiro-state:/root/.local/share/kiro-cli \
#    -v kiro-sessions:/root/.kiro \
#    -v mezame-config:/root/.mezame \
#    mezame bash
#  # inside the container:
#  kiro-cli login --use-device-flow # or: export KIRO_API_KEY=ksk_...
#  mezame init
#  exit
#
# Subsequent runs:
#  docker run --rm -p 9510:9510 \
#    -v kiro-state:/root/.local/share/kiro-cli \
#    -v kiro-sessions:/root/.kiro \
#    -v mezame-config:/root/.mezame \
#    mezame
#
# Caveat: If Kiro CLI ever self-updates and replaces its 
#         bundled Bun, the glibc-linked binary will come
#         back and the TUI / ACP child will fail with
#         `os error 2`. Re-apply the Bun swap from this 
#         file`s RUN step, or rebuild the image.

FROM alpine:latest

# Prerequisites:
#   - bash, curl, unzip, ca-certificates: Kiro installer and Bun installer
#   - libstdc++, libgcc: musl C++ runtime needed by Bun and Kiro
#   - git: cargo fetches registries via git
#   - gcc, musl-dev: Rust needs a C toolchain for linking
#   - nodejs, npm: mezame's build.rs builds the React UI bundle
RUN apk add --no-cache \
      bash \
      ca-certificates \
      curl \
      unzip \
      git \
      libstdc++ \
      libgcc \
      gcc \
      musl-dev \
      nodejs \
      npm

# Rust stable, minimal profile. rustup-init writes to /root/.cargo and
# /root/.rustup; we add the former to PATH below.
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
      | sh -s -- -y --default-toolchain stable --profile minimal --no-modify-path

# One PATH for cargo, Kiro CLI, and Bun. Replaces the bash-specific hooks
# the Kiro installer would otherwise drop into ~/.profile and ~/.bashrc,
# which Alpine's ash cannot parse.
ENV PATH="/root/.cargo/bin:/root/.local/bin:/root/.bun/bin:${PATH}"

# Install Kiro CLI, musl variant.
# The standard zip requires glibc 2.34+ and the AppImage is glibc-linked;
# the -musl variant is the only one that runs on Alpine.
RUN set -eux; \
    arch="$(uname -m)"; \
    curl --proto '=https' --tlsv1.2 -fsSL \
      "https://desktop-release.q.us-east-1.amazonaws.com/latest/kirocli-${arch}-linux-musl.zip" \
      -o /tmp/kirocli.zip; \
    unzip -q /tmp/kirocli.zip -d /tmp; \
    /tmp/kirocli/install.sh; \
    rm -rf /tmp/kirocli /tmp/kirocli.zip

# Swap Kiro's bundled Bun for a musl Bun.
# Kiro's musl zip ships a glibc-linked Bun at ~/.local/share/kiro-cli/bun;
# on Alpine the glibc dynamic linker is missing and the TUI and the `acp`
# child both fail with `os error 2`. The Bun installer auto-detects Alpine
# and drops a musl binary under ~/.bun/bin/bun.
# Use cp, not a symlink: Kiro's self-updater may replace the file.
RUN set -eux; \
    curl -fsSL https://bun.sh/install | bash; \
    mv /root/.local/share/kiro-cli/bun /root/.local/share/kiro-cli/bun.glibc.bak; \
    cp /root/.bun/bin/bun /root/.local/share/kiro-cli/bun; \
    chmod +x /root/.local/share/kiro-cli/bun

# Install mezame from crates.io.
RUN cargo install mezame

EXPOSE 9510

# Persist Kiro credentials, Kiro session history, and mezame config so
# users don't re-authenticate or re-run `mezame init` on every container
# start. Users can mount named volumes over these paths.
VOLUME ["/root/.local/share/kiro-cli", "/root/.kiro", "/root/.mezame"]

# Default to running the bridge. Compose's `setup` service overrides
# this with `bash` for the one-off `kiro-cli login` / `mezame init`
# flow. `docker run mezame` on its own will start the bridge directly,
# assuming volumes with prior setup are mounted.
CMD ["mezame"]
