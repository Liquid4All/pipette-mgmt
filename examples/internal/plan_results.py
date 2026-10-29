#!/usr/bin/env python3
"""Download a plan's results CSV, signed by tools/sign.py. Standard library only.

    python3 examples/internal/plan_results.py [plan_id]    writes <plan_id>-results.csv

PIPETTE_KEY picks the key as tools/sign.py reads it; default the dashboard service
account's.
"""

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "tools"))
from sign import signed_request, signer_from_env  # noqa: E402

DASHBOARD_KEY = "op://Pipette/32co4kiyioas75l3r7smwccgmm/private key"

plan = sys.argv[1] if len(sys.argv) > 1 else "plan-01a12eb9-699e-716e-a48b-f0459a130f06"
url = f"https://collector.pipette.liquid.ai/api/v1/plans/{plan}/results.csv"
status, body = signed_request(signer_from_env(DASHBOARD_KEY), url)
if status != 200:
    sys.exit(f"{status} {body.decode(errors='replace')}")
Path(f"{plan}-results.csv").write_bytes(body)
print(f"wrote {plan}-results.csv")
