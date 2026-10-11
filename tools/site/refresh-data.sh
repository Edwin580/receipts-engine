#!/usr/bin/env bash
# Fetches one month of NYC 311 requests and builds the site's snapshot
# from it, into <out>. The month is MONTH (YYYY-MM) if set, otherwise the
# latest calendar month that ended at least 30 days ago, so that most of
# its complaints have had time to close.
#
#   tools/site/refresh-data.sh <out>
#
# Writes <out>/{manifest.json,data.arrow,cleaning_log.arrow,rejects.arrow}.
# Reads SOCRATA_APP_TOKEN if set (fewer throttled requests). Raw pages and
# the build go to a temporary directory that is removed afterwards.
set -euo pipefail

out=${1:?usage: refresh-data.sh <out>}
if [[ -n "${MONTH:-}" ]]; then
  [[ "$MONTH" =~ ^[0-9]{4}-[0-9]{2}$ ]] || { echo "MONTH must be YYYY-MM, not $MONTH" >&2; exit 2; }
  from="$MONTH-01"
else
  cutoff=$(date -u -d "-30 days" +%Y-%m-01)
  from=$(date -u -d "$cutoff -1 month" +%Y-%m-01)
fi
to=$(date -u -d "$from +1 month" +%Y-%m-01)

root=$(cd "$(dirname "$0")/../.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

cargo build --release -q -p receipts-snapshot --manifest-path "$root/Cargo.toml"
bin="$root/target/release/receipts-snapshot"

echo "Fetching requests created from $from up to $to."
"$bin" fetch --out "$work/raw" --from "$from" --to "$to"
"$bin" build --raw "$work/raw" --out "$work/snap"
snap=$(find "$work/snap" -mindepth 1 -maxdepth 1 -type d)
[[ $(wc -l <<<"$snap") -eq 1 ]] || { echo "expected one snapshot in $work/snap" >&2; exit 1; }
"$bin" verify "$snap"

mkdir -p "$out"
for f in manifest.json data.arrow cleaning_log.arrow rejects.arrow; do
  cp "$snap/$f" "$out/$f"
done
echo "Snapshot $(basename "$snap") for $from..$to written to $out ($(du -sh "$out" | cut -f1))."
