#!/usr/bin/env bash
# Download a plan's results CSV with curl, signed by tools/sign.py (python3, stdlib only).
#
#   examples/internal/plan_results.sh [plan_id]      writes <plan_id>-results.csv
#
# PIPETTE_KEY picks the key as tools/sign.py reads it; default the dashboard service account's.
set -euo pipefail

PLAN=${1:-plan-01a12eb9-699e-716e-a48b-f0459a130f06}
export PIPETTE_KEY=${PIPETTE_KEY:-"op://Pipette/32co4kiyioas75l3r7smwccgmm/private key"}
URL=https://collector.pipette.liquid.ai/api/v1/plans/$PLAN/results.csv
OUT=$PLAN-results.csv
# Signed before curl so a signer failure stops here, not as an unsigned request.
HEADERS=$(python3 "$(dirname "$0")/../../tools/sign.py" "$URL")

if ! curl -sS --fail-with-body -o "$OUT.part" -H @<(printf '%s\n' "$HEADERS") "$URL"; then
  [[ -f $OUT.part ]] && cat "$OUT.part" >&2
  rm -f "$OUT.part"
  exit 1
fi
mv "$OUT.part" "$OUT"
echo "wrote $OUT"
