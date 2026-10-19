#!/usr/bin/env bash
# Runs Trivy with the repository's trivy.yaml (HIGH/CRITICAL, fixable vulnerabilities, secrets, misconfigurations).
# Uses a local `trivy` when installed, otherwise the pinned container image.
#   scripts/trivy.sh fs .                    # what the pre-push hook and CI run
#   scripts/trivy.sh image investigenie:ci   # scan a built image (local trivy only)
set -euo pipefail

TRIVY_VERSION=0.75.0
cd "$(git rev-parse --show-toplevel)"

if command -v trivy >/dev/null 2>&1; then
  exec trivy --config trivy.yaml "$@"
fi
if command -v docker >/dev/null 2>&1; then
  cache="${XDG_CACHE_HOME:-$HOME/.cache}/trivy"
  docker_args=(-v "$PWD:/src:ro" -w /src -v "$cache:/root/.cache/trivy")
  if [[ "${1:-}" == image ]]; then
    docker_args+=(-v /var/run/docker.sock:/var/run/docker.sock)
  fi
  mkdir -p "$cache"
  exec docker run --rm "${docker_args[@]}" \
    "aquasec/trivy:${TRIVY_VERSION}" --config trivy.yaml "$@"
fi
echo "Trivy is not installed and Docker is not available." >&2
echo "Install it from https://trivy.dev (e.g. brew install trivy), or skip once with SKIP=trivy." >&2
exit 1
