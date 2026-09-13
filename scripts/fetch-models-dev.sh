#!/usr/bin/env bash
# Regenerates src/community-snapshot.json from https://api.models.dev for the
# model metadata chain's offline community tier (L3). Run at release time;
# runtime refreshes go to the model_metadata SQLite cache, not this file.
#
# Requires: curl, jq. The slimmed output keeps only context_length and
# max_output_tokens per model so the committed file stays small.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
out="$root/src/community-snapshot.json"
url="${MODELS_DEV_URL:-https://api.models.dev}"

tmp="$(mktemp)"
trap 'rm -f "$tmp"' EXIT

curl --fail --silent --show-error --max-time 60 "$url" > "$tmp"

jq -n \
    --arg date "$(date -u +%Y-%m-%d)" \
    --arg url "$url" \
    --slurpfile raw "$tmp" '
    {
        snapshot_date: $date,
        source: $url,
        note: "Offline community fallback for the model metadata chain. Regenerate with scripts/fetch-models-dev.sh at release time; runtime refreshes land in the model_metadata SQLite cache instead of this file.",
        models: ($raw[0].models // {}
            | map_values(.models // {}
                | map_values({
                    context_length: (.context_length // null),
                    max_output_tokens: (.max_output_tokens // null)
                })
                | with_entries(select(.value.context_length != null or .value.max_output_tokens != null))))
    }
' > "$out"

count="$(jq '[.models[][].context_length] | length' "$out")"
echo "snapshot written to $out ($count context window entries)"
