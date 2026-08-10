# syntax=docker/dockerfile:1.7
#
# Wrapper image for pipette-mgmt. The binary is NOT compiled inside the
# image — build it first (matching the platform you're targeting) and the
# image just packages it on top of the runtime base.
#
# Quick path (host arch):
#
#   ./docker/build.sh                 # cargo build --release && docker build
#
# Manual:
#
#   cargo build --release --locked
#   docker build -f docker/pipette-mgmt.Dockerfile -t pipette-mgmt:dev .
#
# Cross-arch (e.g. arm64 image from an amd64 host):
#
#   cargo build --release --locked --target aarch64-unknown-linux-gnu
#   docker buildx build \
#     --platform linux/arm64 \
#     --build-arg BIN=target/aarch64-unknown-linux-gnu/release/pipette-mgmt \
#     -f docker/pipette-mgmt.Dockerfile -t pipette-mgmt:dev .
#
# TLS uses rustls, so the runtime image only needs ca-certificates plus the
# baseline glibc/libgcc that come with debian-slim.

# Debian 13 (trixie) ships glibc 2.41. The release binary is built on
# ubuntu-24.04 runners (glibc 2.39) and references symbols up to
# GLIBC_2.38, so the runtime base must be at least that new — bookworm
# (glibc 2.36) fails at startup with `GLIBC_2.38 not found`.
FROM debian:trixie-slim AS runtime

ARG BIN=target/release/pipette-mgmt

# Default user is uid 10001. Override at runtime with any uid/gid via:
#   docker run --user $(id -u):$(id -g) ...
# /data and /home/pipette are mode 1777 so any uid can read/write without
# host-side chown of bind-mounted directories.
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system --gid 10001 pipette \
    && useradd --system --uid 10001 --gid 10001 \
               --no-create-home --home-dir /home/pipette \
               --shell /usr/sbin/nologin pipette \
    && mkdir -p /data /etc/pipette-mgmt /home/pipette \
    && chmod 1777 /data /home/pipette

COPY ${BIN} /usr/local/bin/pipette-mgmt

# The image redistributes first-party and third-party code together, so the
# terms covering each travel inside it alongside the binary: the project's own
# license and NOTICE, which Apache-2.0 section 4 requires travel with any
# distribution, plus the notices the dependency licenses require with their
# code.
COPY LICENSE NOTICE THIRD-PARTY-LICENSES.md /usr/share/doc/pipette-mgmt/

ENV PIPETTE_MGMT_CONFIG=/etc/pipette-mgmt/config.toml \
    RUST_LOG=info \
    HOME=/home/pipette

VOLUME ["/data", "/etc/pipette-mgmt"]
WORKDIR /home/pipette
USER 10001:10001
EXPOSE 3000

# /health is unauthenticated by design.
HEALTHCHECK --interval=30s --timeout=3s --start-period=30s --retries=3 \
  CMD curl -fsS "http://127.0.0.1:3000/health" >/dev/null || exit 1

ENTRYPOINT ["pipette-mgmt"]
CMD ["serve"]
