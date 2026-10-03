#!/usr/bin/env bash
# Build and publish the dvb container image to Docker Hub.
#
# Usage: scripts/publish-image.sh [TAG]
#
# Defaults to $GITHUB_REF_NAME when TAG is omitted.
# Reuses scripts/build-image.sh to perform the container build.
#
# Examples:
#   scripts/publish-image.sh v0.1.0
#   scripts/publish-image.sh 0.1.0

set -euo pipefail

readonly REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
readonly TAG="${1:-${GITHUB_REF_NAME:-}}"

# Print this script's leading comment block as help text.
print_help() {
    awk 'NR == 1 { next } /^#/ { sub(/^# ?/, ""); print; next } { exit }' "${BASH_SOURCE[0]}"
}

if [[ "${TAG}" == "-h" || "${TAG}" == "--help" ]]; then
    print_help
    exit 0
fi

if [[ -z "${TAG}" ]]; then
    echo "error: no tag specified and GITHUB_REF_NAME is not set" >&2
    echo "usage: $0 <TAG>" >&2
    exit 2
fi

readonly VERSION="${TAG#v}"
readonly IMAGE="${IMAGE_NAME:-dreamoutbox/dvb}"

echo "Building ${IMAGE}:${VERSION} via scripts/build-image.sh..."
"${REPO_ROOT}/scripts/build-image.sh" -t "${IMAGE}:${VERSION}"

echo "Tagging additional release tags..."
docker tag "${IMAGE}:${VERSION}" "${IMAGE}:${TAG}"
docker tag "${IMAGE}:${VERSION}" "${IMAGE}:latest"

echo "Pushing images to Docker Hub..."
docker push "${IMAGE}:${VERSION}"
docker push "${IMAGE}:${TAG}"
docker push "${IMAGE}:latest"

echo "Successfully published ${IMAGE}:${VERSION}, ${IMAGE}:${TAG}, and ${IMAGE}:latest"
