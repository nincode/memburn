#!/usr/bin/env bash
# Builds target/usb/memburn-<version>-usb.img in Docker; write it to a stick with Rufus (DD mode), Etcher or dd.
set -euo pipefail
cd "$(dirname "$0")/.."
version=$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\(.*\)"/\1/p' Cargo.toml)
DOCKER_BUILDKIT=1 docker build -f docker/usb.Dockerfile --build-arg VERSION="$version" \
  --output type=local,dest=target/usb "$@" .
echo "target/usb/memburn-$version-usb.img"
