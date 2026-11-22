#!/usr/bin/env bash
#
# End-to-end smoke test against the real compose stack.
#
# This is the only check that exercises the whole system: three containers, a real model,
# a real vector index, a real ingest, a real search. It is slow (minutes) and it is the one
# test that cannot be faked by a stub.
#
# Usage: scripts/smoke.sh [--keep]
#   --keep   leave the stack running afterwards, for poking at

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

COMPOSE=(docker compose -f docker/docker-compose.yml)
PORT="${ADRIAN_PORT:-8080}"
BASE="http://127.0.0.1:${PORT}"
KEEP=false
for arg in "$@"; do
  case "$arg" in
    --keep) KEEP=true ;;
    *) echo "unknown argument: $arg" >&2; exit 2 ;;
  esac
done

STEP=""
fail() {
  echo "" >&2
  echo "SMOKE TEST FAILED at step: ${STEP}" >&2
  echo "$*" >&2
  echo "" >&2
  echo "--- adrian logs ---" >&2
  "${COMPOSE[@]}" logs --no-color --tail 60 adrian >&2 || true
  echo "--- embedder logs ---" >&2
  "${COMPOSE[@]}" logs --no-color --tail 40 embedder >&2 || true
  "${COMPOSE[@]}" down -v >/dev/null 2>&1 || true
  exit 1
}

cleanup() {
  if ! $KEEP; then
    "${COMPOSE[@]}" down -v >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

json() { python3 -c 'import json,sys; d=json.load(sys.stdin); print(eval(sys.argv[1], {"d": d}))' "$1"; }

STEP="check the model weights are present"
if [[ ! -f embedder/weights/SHA256SUMS ]]; then
  fail "embedder/weights/SHA256SUMS is missing; run scripts/fetch_weights.sh"
fi

STEP="build and start the stack"
"${COMPOSE[@]}" up -d --build >/dev/null 2>&1 || fail "docker compose up failed"

STEP="wait for adrian to report both dependencies reachable"
deadline=$((SECONDS + 300))
while true; do
  body="$(curl -fsS "${BASE}/healthz" 2>/dev/null || true)"
  if [[ -n "$body" ]]; then
    ok="$(printf '%s' "$body" | python3 -c '
import json,sys
try:
    d = json.load(sys.stdin)
    print("yes" if d.get("embedder",{}).get("reachable") and d.get("qdrant",{}).get("reachable") else "no")
except Exception:
    print("no")
' 2>/dev/null || echo no)"
    [[ "$ok" == "yes" ]] && break
  fi
  if (( SECONDS > deadline )); then
    fail "adrian never became healthy; last healthz body: ${body:-<empty>}"
  fi
  sleep 2
done

STEP="create a project"
curl -fsS -X POST "${BASE}/v1/projects" \
  -H 'content-type: application/json' \
  -d '{"slug":"smoke","name":"Smoke"}' >/dev/null || fail "project creation failed"

STEP="ingest three ADRs from the fixture corpus"
ADRS=(
  fixtures/madr/real/madr__0013-use-yaml-front-matter-for-meta-data.md
  fixtures/madr/real/madr__0010-support-categories.md
  fixtures/madr/synthetic/minimal-template.md
)
FIRST_ID=""
for path in "${ADRS[@]}"; do
  payload="$(python3 - "$path" <<'PY'
import json, sys
path = sys.argv[1]
with open(path, "r", encoding="utf-8") as handle:
    source = handle.read()
print(json.dumps({
    "project": "smoke",
    "external_ref": path,
    "source": source,
    "actor": "human:smoke",
    "scopes": ["crates/api"],
}))
PY
)"
  response="$(curl -fsS -X POST "${BASE}/v1/adrs" -H 'content-type: application/json' -d "$payload")" \
    || fail "ingest failed for ${path}"
  id="$(printf '%s' "$response" | json 'd["id"]')"
  [[ -z "$FIRST_ID" ]] && FIRST_ID="$id"
  echo "    ingested ${path} -> ${id}"
done

STEP="wait for every chunk to be embedded"
deadline=$((SECONDS + 180))
while true; do
  body="$(curl -fsS "${BASE}/v1/adrs/${FIRST_ID}" 2>/dev/null || true)"
  if [[ -n "$body" ]]; then
    done_count="$(printf '%s' "$body" | python3 -c '
import json,sys
d = json.load(sys.stdin)
chunks = d.get("chunks", [])
print("yes" if chunks and all(c.get("embedded") for c in chunks) else "no")
' 2>/dev/null || echo no)"
    [[ "$done_count" == "yes" ]] && break
  fi
  if (( SECONDS > deadline )); then
    fail "chunks were never embedded"
  fi
  sleep 2
done

STEP="search returns at least one decision with a matched chunk"
results="$(curl -fsS -X POST "${BASE}/v1/search" \
  -H 'content-type: application/json' \
  -d '{"query":"should we put metadata in yaml front matter","project":"smoke","limit":5}')" \
  || fail "search request failed"
printf '%s' "$results" | python3 -c '
import json, sys
d = json.load(sys.stdin)
results = d.get("results", [])
assert results, "search returned no results"
matched = results[0].get("matched_chunks", [])
assert matched, "the top result has no matched chunks"
assert matched[0].get("text"), "the top matched chunk has no text"
print("    top result:", results[0]["title"])
' || fail "search response was not usable"

STEP="get an ADR and confirm its sections are non-empty"
detail="$(curl -fsS "${BASE}/v1/adrs/${FIRST_ID}")" || fail "detail request failed"
printf '%s' "$detail" | python3 -c '
import json, sys
d = json.load(sys.stdin)
sections = d.get("sections", [])
assert sections, "the decision has no sections"
assert any(s.get("text", "").strip() for s in sections), "every section is empty"
' || fail "detail response was not usable"

STEP="context lookup for a path under the declared scope"
curl -fsS "${BASE}/v1/context?path=crates/api/src/main.rs&project=smoke" >/dev/null \
  || fail "context request failed"

STEP="export as MADR and re-ingest the result"
exported="$(curl -fsS -X POST "${BASE}/v1/export" \
  -H 'content-type: application/json' \
  -d "{\"project\":\"smoke\",\"format\":\"madr\",\"adr_ids\":[\"${FIRST_ID}\"]}")" \
  || fail "export failed"
[[ -n "$exported" ]] || fail "export returned nothing"
payload="$(python3 - "$exported" <<'PY'
import json, sys
source = sys.argv[1]
print(json.dumps({
    "project": "smoke",
    "external_ref": "smoke/roundtrip.md",
    "source": source,
    "actor": "human:smoke",
}))
PY
)"
curl -fsS -X POST "${BASE}/v1/adrs" -H 'content-type: application/json' -d "$payload" >/dev/null \
  || fail "the exported document could not be re-ingested"

STEP="MCP tools/list over HTTP"
curl -fsS -X POST "${BASE}/mcp" -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' \
  | python3 -c '
import json, sys
d = json.load(sys.stdin)
tools = d["result"]["tools"]
assert len(tools) == 7, f"expected 7 tools, got {len(tools)}"
' || fail "MCP tools/list failed"

echo ""
echo "SMOKE TEST PASSED"
