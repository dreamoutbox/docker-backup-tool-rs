#!/usr/bin/env bash
# Run the test suite with cargo-nextest.
#
# Defaults to running tests one at a time (-j 1). The container-backed tests
# each start a real server, and running several at once is the easy way to hit
# OOM on a small machine. Raise the job count only when you know the machine can
# take it.
#
# Usage: scripts/run-tests.sh [-j N] [--all] [-- <extra nextest args>]
#
#   -j, --jobs N     number of tests to run concurrently (default: 1)
#       --all        also run the #[ignore]d container tests (needs Docker)
#       --install    install cargo-nextest if it is missing, then continue
#   -h, --help       show this help
#
# Examples:
#   scripts/run-tests.sh                  # serial, fast, no Docker needed
#   scripts/run-tests.sh -j 4             # four at a time
#   scripts/run-tests.sh --all            # plus the S3 and SFTP containers
#   scripts/run-tests.sh --all -j 2       # containers, two at a time
#
# Container tests are capped at 0.5 CPU and 512 MiB each; see
# tests/support/mod.rs and DVB_TEST_CPU / DVB_TEST_MEM_MB.

set -euo pipefail

readonly REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
readonly NEXTEST="cargo nextest"

jobs=1
all=""
extra_args=()

# Print this script's leading comment block as help text.
print_help() {
    awk 'NR == 1 { next } /^#/ { sub(/^# ?/, ""); print; next } { exit }' "${BASH_SOURCE[0]}"
}

die() {
    echo "error: $*" >&2
    exit 2
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        -j|--jobs)
            [[ $# -ge 2 ]] || die "$1 needs a value"
            jobs="$2"
            shift 2
            ;;
        --all)
            all="--run-ignored all"
            shift
            ;;
        --install)
            cargo nextest --version >/dev/null 2>&1 || cargo install cargo-nextest --locked
            shift
            ;;
        -h|--help)
            print_help
            exit 0
            ;;
        --)
            shift
            extra_args=("$@")
            break
            ;;
        *)
            die "unknown argument: $1"
            ;;
    esac
done

[[ "${jobs}" =~ ^[1-9][0-9]*$ ]] || die "--jobs must be a positive integer, got: ${jobs}"

if ! cargo nextest --version >/dev/null 2>&1; then
    echo "error: cargo-nextest is required." >&2
    echo "       install it with: cargo install cargo-nextest --locked" >&2
    echo "       or re-run with --install" >&2
    exit 1
fi

cd "${REPO_ROOT}"

echo "running tests with ${jobs} job(s)${all:+, including ignored container tests}"
# shellcheck disable=SC2086 # ${all} is an intentional two-word flag
exec ${NEXTEST} run \
    --jobs "${jobs}" \
    ${all} \
    "${extra_args[@]}"