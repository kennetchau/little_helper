#!/usr/bin/env bash
# Build the little_helper container image from this repo.
set -euo pipefail
cd "$(dirname "$0")"

IMAGE="${IMAGE:-little_helper}"

# Keep a local git bundle so the image build always has the current tree
# (mirrors the reference build.sh "git pull local master" habit — but this
# repo is the source, so we just build in place).
podman build -t "$IMAGE" .

echo "Built $IMAGE"
