#!/usr/bin/env bash
# Fetches the Westchester towns' SeeClickFix requests for the latest twelve
# full months that ended at least 30 days ago (or FROM..TO, YYYY-MM-DD,
# end exclusive), and builds the site's snapshot from them into <out>.
#
#   tools/site/refresh-westchester.sh <out>
#
# Writes <out>/{manifest.json,data.arrow,cleaning_log.arrow,rejects.arrow}.
# SeeClickFix rate-limits, so this takes a few minutes. Raw pages and the
# build go to a temporary directory that is removed afterwards.
set -euo pipefail

out=${1:?usage: refresh-westchester.sh <out>}
if [[ -n "${FROM:-}" || -n "${TO:-}" ]]; then
  from=${FROM:?set both FROM and TO}
  to=${TO:?set both FROM and TO}
else
  to=$(date -u -d "-30 days" +%Y-%m-01)
  from=$(date -u -d "$to -12 months" +%Y-%m-01)
fi

root=$(cd "$(dirname "$0")/../.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

cargo build --release -q -p receipts-snapshot --manifest-path "$root/Cargo.toml"
bin="$root/target/release/receipts-snapshot"

echo "Fetching Westchester towns' requests created from $from up to $to."
"$bin" fetch --source westchester --out "$work/raw" --from "$from" --to "$to"
"$bin" build --source westchester --raw "$work/raw" --out "$work/snap"
snap=$(find "$work/snap" -mindepth 1 -maxdepth 1 -type d)
[[ $(wc -l <<<"$snap") -eq 1 ]] || { echo "expected one snapshot in $work/snap" >&2; exit 1; }
"$bin" verify "$snap"

mkdir -p "$out"
for f in manifest.json data.arrow cleaning_log.arrow rejects.arrow; do
  cp "$snap/$f" "$out/$f"
done
echo "Snapshot $(basename "$snap") for $from..$to written to $out ($(du -sh "$out" | cut -f1))."
