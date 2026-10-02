#!/usr/bin/env bash
# Build the dvb container image.
#
# Usage: scripts/build-image.sh [-t TAG] [--no-cache] [-- <extra docker build args>]
#
# Examples:
#   scripts/build-image.sh                      # dreamoutbox/dvb:latest
#   scripts/build-image.sh -t dvb:dev           # custom tag
#   scripts/build-image.sh --no-cache           # ignore cached layers
#   scripts/build-image.sh -- --platform linux/arm64
#
# The build uses cargo-chef, so a source-only change reuses the cached
# dependency layer and only recompiles this crate.

set -euo pipefail

readonly REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"

readonly DEFAULT_TAG="dreamoutbox/dvb:latest"
readonly IMAGE="${IMAGE_NAME:-dreamoutbox/dvb}"
readonly PLATFORM="${DOCKER_PLATFORM:-}"

# Print this script's leading comment block as help text.
print_help() {
    awk 'NR == 1 { next } /^#/ { sub(/^# ?/, ""); print; next } { exit }' "${BASH_SOURCE[0]}"
}

tag="${DEFAULT_TAG}"
no_cache=""
platform=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        -t|--tag)
            [[ $# -ge 2 ]] || { echo "error: $1 needs a value" >&2; exit 2; }
            tag="$2"
            shift 2
            ;;
        --no-cache)
            no_cache="--no-cache"
            shift
            ;;
        -p|--platform)
            [[ $# -ge 2 ]] || { echo "error: $1 needs a value" >&2; exit 2; }
            platform="--platform $2"
            shift 2
            ;;
        -h|--help)
            print_help
            exit 0
            ;;
        --)
            shift
            break
            ;;
        *)
            echo "error: unknown argument: $1" >&2
            exit 2
            ;;
    esac
done

# Fall back to the compose platform when the caller did not pass one.
if [[ -z "${platform}" && -n "${PLATFORM}" ]]; then
    platform="--platform ${PLATFORM}"
fi

# `read -ra` is used so a word-split platform value stays two arguments.
read -r -a platform_args <<<"${platform}"

cd "${REPO_ROOT}"

echo "building ${IMAGE}:${tag#${IMAGE}:}"
docker build \
    --tag "${tag}" \
    ${no_cache} \
    "${platform_args[@]}" \
    "$@" \
    .
