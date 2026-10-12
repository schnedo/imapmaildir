#!/usr/bin/env bash
# Checks criterion results of the last `--baseline` run for regressions.
#
# usage: bench-check.sh [--profile ci|local] [--tsv]
#   --profile  threshold profile of benches/thresholds.toml (default: local)
#   --tsv      print "bench<TAB>lower_bound" lines only, never fail (used by bench-calibrate)
set -euo pipefail

profile=local
tsv=false
while [[ $# -gt 0 ]]; do
  case $1 in
  --profile)
    profile=$2
    shift 2
    ;;
  --tsv)
    tsv=true
    shift
    ;;
  *)
    echo "unknown argument $1" >&2
    exit 2
    ;;
  esac
done

root=$(git rev-parse --show-toplevel)
criterion_home=${CRITERION_HOME:-${CARGO_TARGET_DIR:-$root/target}/criterion}

if [[ $tsv == false ]]; then
  limit=$(awk -v section="[$profile]" '
    /^\[/ { in_section = ($0 == section) }
    in_section && $1 == "threshold" { print $3; found = 1 }
    END { if (!found) exit 1 }
  ' "$root/benches/thresholds.toml") || {
    echo "no threshold for profile $profile" >&2
    exit 2
  }
fi

mapfile -t estimates < <(find "$criterion_home" -path '*/change/estimates.json' 2>/dev/null | sort)
if [[ ${#estimates[@]} -eq 0 ]]; then
  echo "no criterion comparisons found in $criterion_home" >&2
  exit 2
fi

report="| bench | change | 99% CI | threshold | |
|---|---:|---:|---:|---|"
regressions=0
for estimate in "${estimates[@]}"; do
  bench_dir=${estimate%/change/estimates.json}
  bench=$(jq -r .full_id "$bench_dir/new/benchmark.json")
  read -r mean lower upper < <(jq -r '.mean | "\(.point_estimate) \(.confidence_interval.lower_bound) \(.confidence_interval.upper_bound)"' "$estimate")
  if [[ $tsv == true ]]; then
    printf '%s\t%s\n' "$bench" "$lower"
    continue
  fi
  if awk -v lower="$lower" -v limit="$limit" 'BEGIN { exit !(lower > limit) }'; then
    status="❌ regressed"
    regressions=$((regressions + 1))
  elif awk -v upper="$upper" 'BEGIN { exit !(upper < 0) }'; then
    status="✅ improved"
  else
    status=""
  fi
  report+=$'\n'$(awk -v b="$bench" -v m="$mean" -v l="$lower" -v u="$upper" -v t="$limit" -v s="$status" \
    'BEGIN { printf "| %s | %+.1f%% | [%+.1f%%, %+.1f%%] | %.0f%% | %s |", b, m * 100, l * 100, u * 100, t * 100, s }')
done

[[ $tsv == true ]] && exit 0

echo "$report"
if [[ -n ${GITHUB_STEP_SUMMARY:-} ]]; then
  {
    echo "### Benchmarks (profile $profile)"
    echo
    echo "$report"
  } >>"$GITHUB_STEP_SUMMARY"
fi

if [[ $regressions -gt 0 ]]; then
  echo "$regressions bench(es) regressed beyond the $profile thresholds" >&2
  exit 1
fi
