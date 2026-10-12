#!/usr/bin/env bash
# Benchmarks a base ref and the working tree on the same machine and checks for regressions.
#
# usage: bench-compare.sh [--profile ci|local] [base-ref (default: main)]
#
# The base ref is checked out into a temporary git worktree, the working tree is left untouched.
# Set EXPECT_IOURING=1 to fail if the working tree benches did not run on an io_uring driver.
#
# For less noise on a local machine: run on AC power, close heavy applications and pin the cpu
# frequency, e.g. `sudo cpupower frequency-set -g performance`.
set -euo pipefail

profile=local
base=main
while [[ $# -gt 0 ]]; do
  case $1 in
  --profile)
    profile=$2
    shift 2
    ;;
  *)
    base=$1
    shift
    ;;
  esac
done

script_dir=$(dirname "$(realpath "$0")")
check=$(command -v bench-check || echo "$script_dir/bench-check.sh")
root=$(git rev-parse --show-toplevel)
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$root/target}
export CRITERION_HOME=$CARGO_TARGET_DIR/criterion
worktree=$CARGO_TARGET_DIR/bench-base
base_target=$CARGO_TARGET_DIR/bench-base-target

log=$(mktemp)

bench() {
  cargo bench --features bench --bench imapmaildir -- --noplot "$@" | tee "$log" >&2
  sed -n 's/^runtime: //p' "$log"
}

cleanup() {
  git -C "$root" worktree remove --force "$worktree" 2>/dev/null || true
  rm -f "$log"
}
trap cleanup EXIT
cleanup
git -C "$root" worktree add --detach "$worktree" "$base"

if [[ ! -f $worktree/benches/imapmaildir.rs ]]; then
  echo "no benches on $base, nothing to compare"
  exit 0
fi

echo "::group::bench $base"
base_runtime=$(cd "$worktree" && CARGO_TARGET_DIR=$base_target bench --save-baseline base)
echo "::endgroup::"

find "$CRITERION_HOME" -type d -name change -prune -exec rm -rf {} +

echo "::group::bench working tree"
runtime=$(cd "$root" && bench --baseline-lenient base)
echo "::endgroup::"

echo "runtime: $base_runtime ($base) -> $runtime (working tree)"
if [[ -n ${GITHUB_STEP_SUMMARY:-} ]]; then
  echo "runtime: \`$base_runtime\` ($base) → \`$runtime\` (working tree)" >>"$GITHUB_STEP_SUMMARY"
fi
if [[ ${EXPECT_IOURING:-} == 1 && $runtime != *io_uring* ]]; then
  echo "expected io_uring driver, got $runtime" >&2
  exit 1
fi

"$check" --profile "$profile"
