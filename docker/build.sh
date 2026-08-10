#!/usr/bin/env bash
#
# Build pipette-mgmt and wrap it in a docker image. The Dockerfile is a
# thin wrapper — it does not compile anything, it just copies the binary
# from target/release into a runtime base image.
#
# Env:
#   TAG     Docker image tag         (default: pipette-mgmt:dev)
#   TARGET  cargo --target triple    (default: host triple, no --target)
#
# Examples:
#   ./docker/build.sh
#   TAG=pipette-mgmt:wip ./docker/build.sh
#   TARGET=aarch64-unknown-linux-gnu ./docker/build.sh
#
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

TAG="${TAG:-pipette-mgmt:dev}"
TARGET="${TARGET:-}"

if [[ -n "${TARGET}" ]]; then
  cargo build --release --locked --target "${TARGET}"
  BIN="target/${TARGET}/release/pipette-mgmt"
else
  cargo build --release --locked
  BIN="target/release/pipette-mgmt"
fi

docker build \
  --build-arg "BIN=${BIN}" \
  -f docker/pipette-mgmt.Dockerfile \
  -t "${TAG}" \
  .
