#!/usr/bin/env bash
# Measures benchmark noise with A/A runs of the working tree and suggests thresholds.
#
# usage: bench-calibrate.sh [runs (default: 5)]
#
# Saves a baseline, then compares the same code against it `runs` times. The largest lower bound
# of the 99% confidence interval per class is pure noise; suggested threshold is 1.5 times that.
# Copy the suggestion into the matching profile of benches/thresholds.toml.
set -euo pipefail

runs=${1:-5}

script_dir=$(dirname "$(realpath "$0")")
check=$(command -v bench-check || echo "$script_dir/bench-check.sh")
root=$(git rev-parse --show-toplevel)
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$root/target}
export CRITERION_HOME=$CARGO_TARGET_DIR/criterion
results=$(mktemp)
trap 'rm -f "$results"' EXIT

cd "$root"
cargo bench --features bench --bench imapmaildir -- --noplot --save-baseline aa
for run in $(seq "$runs"); do
  echo "::group::A/A run $run/$runs"
  find "$CRITERION_HOME" -type d -name change -prune -exec rm -rf {} +
  cargo bench --features bench --bench imapmaildir -- --noplot --baseline aa
  "$check" --tsv >>"$results"
  echo "::endgroup::"
done

summary=$(awk -F '\t' '
  { if (!($1 in max) || $3 > max[$1]) { max[$1] = $3; bench[$1] = $2 } }
  END {
    print "| class | max noise (lower bound) | bench | suggested threshold |"
    print "|---|---:|---|---:|"
    for (class in max) {
      suggestion = max[class] * 1.5
      if (suggestion < 0.01) suggestion = 0.01
      printf "| %s | %+.1f%% | %s | %s = %.2f |\n", class, max[class] * 100, bench[class], class, int(suggestion * 100 + 0.999) / 100
    }
  }
' "$results")

echo "$summary"
if [[ -n ${GITHUB_STEP_SUMMARY:-} ]]; then
  {
    echo "### Benchmark noise ($runs A/A runs)"
    echo
    echo "$summary"
  } >>"$GITHUB_STEP_SUMMARY"
fi
