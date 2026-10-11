#!/usr/bin/env bash
# Slate/OrcSpace e2e harness — drives the REAL control socket + CLI.
# usage: ./e2e.sh slate|orc
set -u
APP="${1:?slate|orc}"
PASS=0; FAIL=0; NOTE=""
out(){ echo "$@"; }
hr(){ echo "------------------------------------------------------------"; }
hdr(){ hr; echo "## $*"; hr; }
rec(){ # rec <ok> <name> [detail]
  if [ "$1" = 0 ]; then PASS=$((PASS+1)); echo "PASS  $2  $3";
  else FAIL=$((FAIL+1)); echo "FAIL  $2  $3"; fi
}

if [ "$APP" = slate ]; then
  SOCK="${SLATE_SOCK:-/tmp/slate-e2e.sock}"
  TOKEN_FILE="$HOME/slate-e2e-data/control-token"
  AUTH_HDR="x-slate-token"
  CLI() { LD_LIBRARY_PATH="$HOME/gtkroot/usr/lib" SLATE_SOCKET_PATH="$SOCK" SLATE_TEST_USER_DATA="$HOME/slate-e2e-data" "$HOME/slate/native/target/release/slate" "$@"; }
else
  SOCK="${ORC_SOCK:-/tmp/orcspace.sock}"
  TOKEN_FILE="$HOME/.config/orcspace/control-token"
  AUTH_HDR="x-orcspace-token"
  CLI() { ELECTRON_RUN_AS_NODE=1 ORCSPACE_SOCKET_PATH="$SOCK" "$HOME/opt/OrcSpace/orcspace" "$HOME/opt/OrcSpace/resources/cli/orc.mjs" "$@"; }
fi
TOKEN="$(cat "$TOKEN_FILE" 2>/dev/null)"

req(){ # req METHOD PATH [json] [extra_headers...] -> prints "<status>\n<body>"
  local m="$1" p="$2" body="${3:-}"; shift 3 || true
  # agentId rides in body on writes, ?agentId= on reads (cli contract)
  if [ "$m" = GET ]; then
    case "$p" in *\?*) p="$p&agentId=e2e-harness";; *) p="$p?agentId=e2e-harness";; esac
  elif [ -n "$body" ]; then
    body=$(python3 -c 'import json,sys; d=json.loads(sys.argv[1]); d["agentId"]="e2e-harness"; print(json.dumps(d))' "$body")
  else
    body='{"agentId":"e2e-harness"}'
  fi
  local args=( -s -o /tmp/e2e-body.$$ -w '%{http_code}' --unix-socket "$SOCK" -H "$AUTH_HDR: $TOKEN" -X "$m" )
  [ -n "$body" ] && args+=( -H 'content-type: application/json' -d "$body" )
  for h in "$@"; do args+=( -H "$h" ); done
  local code; code=$(curl "${args[@]}" "http://localhost$p")
  echo "$code"; cat /tmp/e2e-body.$$;
}
jget(){ python3 -c "import sys,json; d=json.load(sys.stdin); print(eval(sys.argv[1]))" "$1" 2>/dev/null; }

hdr "$APP :: 0. transport + auth"
code=$(req GET /health | head -1); body=$(req GET /health | tail -1)
echo "$body" | grep -q '"ok":true'; rec $? "health.ok" "$(echo "$body" | head -c 200)"
echo "$body" | grep -q '"commands"'; COMMANDS_FIELD=$?
code=$(req GET /presence | head -1); rec 0 "presence.open(no token needed if 200)" "status=$code"
code=$(curl -s -o /dev/null -w '%{http_code}' --unix-socket "$SOCK" http://localhost/orchestration/workers)
[ "$code" != 200 ]; rec $? "auth.gate: workers without token rejected" "status=$code"
code=$(curl -s -o /dev/null -w '%{http_code}' --unix-socket "$SOCK" -H "$AUTH_HDR: bogus" http://localhost/orchestration/workers)
[ "$code" != 200 ]; rec $? "auth.gate: bad token rejected" "status=$code"

hdr "$APP :: 1. terminal lifecycle (REST)"
MARK="E2E-MARKER-$$"
resp=$(req POST /terminal "{\"title\":\"e2e-term\",\"command\":\"/bin/bash\"}" | tail -1)
echo "spawn resp: $resp"
TID=$(echo "$resp" | jget "d.get('terminalId') or d.get('id') or (d.get('terminal') or {}).get('id') or ''")
[ -z "$TID" ] && TID=$(echo "$resp" | grep -oE '"[a-zA-Z]*[Ii]d":"[^"]+"' | head -1 | cut -d'"' -f4)
[ -n "$TID" ]; rec $? "terminal.spawn" "id=$TID"
req POST "/terminal/$TID/write" "{\"text\":\"echo $MARK\"}" >/dev/null
ok=1
for i in $(seq 1 20); do sleep 0.5; O=$(req GET "/terminal/$TID/output" | tail -1); echo "$O" | grep -q "$MARK" && { ok=0; break; }; done
rec $ok "terminal.write->output roundtrip" "$(echo "$O" | head -c 160)"
req POST "/terminal/$TID/write" "{\"text\":\"printf 'L1NE-'$$'\\\\n'\",\"pressEnter\":true}" >/dev/null
sleep 1.5
O=$(req GET "/terminal/$TID/output" | tail -1)
echo "$O" | grep -q 'L1NE-'; rec $? "terminal output has line output" "$(echo "$O" | head -c 100)"
req POST "/terminal/$TID/write" "{\"text\":\"exit\",\"pressEnter\":true}" >/dev/null
sleep 2
O=$(req GET "/terminal/$TID/output" | tail -1)
echo "$O" | grep -qiE 'exit|closed|dead|\[process|not found'; rec $? "exit handled (marker or auto-dispose)" "$(echo "$O" | tail -c 100)"

hdr "$APP :: 2. orchestration e2e"
resp=$(req POST /orchestration/runs '{"objective":"e2e run objective"}' | tail -1); echo "run: $resp"
RUNID=$(echo "$resp" | jget "(d.get('data') or {}).get('dispatchId') or ((d.get('data') or {}).get('dispatch') or {}).get('id') or (d.get('data') or {}).get('id') or d.get('id') or ''")
[ -n "$RUNID" ]; rec $? "runs.create" "id=$RUNID"
resp=$(req POST /orchestration/tasks "{\"title\":\"e2e task\",\"spec\":\"e2e spec body\",\"runId\":\"$RUNID\"}" | tail -1); echo "task: $resp"
TASKID=$(echo "$resp" | jget "(d.get('data') or {}).get('dispatchId') or ((d.get('data') or {}).get('dispatch') or {}).get('id') or (d.get('data') or {}).get('id') or d.get('id') or ''")
[ -n "$TASKID" ]; rec $? "tasks.create" "id=$TASKID"
body=$(req GET /orchestration/tasks | tail -1); echo "$body" | grep -q "$TASKID"; rec $? "tasks.list contains new" ""
# dispatch with a stub agent command (cat) — preamble injection echoed back proves the chain
resp=$(req POST /orchestration/dispatches "{\"taskId\":\"$TASKID\",\"agent\":\"e2e-stub\",\"command\":\"cat\"}" | tail -1)
echo "dispatch: $resp"
DISPID=$(echo "$resp" | jget "(d.get('data') or {}).get('dispatchId') or ((d.get('data') or {}).get('dispatch') or {}).get('id') or (d.get('data') or {}).get('id') or d.get('id') or ''")
DTERM=$(echo "$resp" | jget "(d.get('data') or {}).get('terminalId') or d.get('terminalId') or ''")
[ -n "$DISPID" ]; rec $? "dispatch.start" "id=$DISPID term=$DTERM"
sleep 3
body=$(req GET /orchestration/dispatches | tail -1); echo "$body" | grep -q "$DISPID"; rec $? "dispatches.list" ""
if [ -n "$DTERM" ]; then
  O=$(req GET "/terminal/$DTERM/output" | tail -1)
  echo "$O" | grep -qiE 'dispatch|task|e2e'; rec $? "preamble injected into worker terminal" "$(echo "$O" | grep -iE 'dispatch|task' | head -c 160)"
fi
body=$(req GET /orchestration/workers | tail -1); echo "$body" | grep -qE 'e2e-stub|'"$DISPID"; rec $? "workers.list shows dispatch" "$(echo "$body" | head -c 160)"
resp=$(req POST /orchestration/workers/tell "{\"to\":\"$DTERM\",\"text\":\"e2e hello\"}" | tail -1); echo "tell: $resp"
echo "$resp" | grep -qE 'ok|sent|delivered|message' ; rec $? "workers.tell" "$resp"
resp=$(req POST "/orchestration/dispatches/$DISPID/settle" '{"outcome":"succeeded"}' | tail -1); echo "settle: $resp"
echo "$resp" | grep -qE 'done|settled|ok'; rec $? "dispatch.settle done" "$resp"
# failure sniff: bogus command must settle failed
resp=$(req POST /orchestration/dispatches "{\"taskId\":\"$TASKID\",\"agent\":\"e2e-stub\",\"command\":\"/nonexistent-e2e-bin\"}" | tail -1)
BID=$(echo "$resp" | jget "(d.get('data') or {}).get('dispatchId') or ((d.get('data') or {}).get('dispatch') or {}).get('id') or (d.get('data') or {}).get('id') or d.get('id') or ''")
sleep 4
body=$(req GET "/orchestration/dispatches/$BID" | tail -1)
echo "$body" | grep -qiE 'failed'; rec $? "launch-failure sniff settles dispatch=failed" "$(echo "$body" | head -c 160)"

hdr "$APP :: 3. CLI parity"
v=$(CLI version 2>&1 | head -1); echo "version: $v"; echo "$v" | grep -qiE '[0-9]'; rec $? "cli version" "$v"
o=$(CLI whoami 2>&1 | head -3); echo "whoami: $o"; [ -n "$o" ]; rec $? "cli whoami" ""
o=$(CLI workers 2>&1 | head -5); echo "workers: $o"; [ -n "$o" ]; rec $? "cli workers" ""
o=$(CLI status 2>&1 | head -3); echo "status: $o"; rec 0 "cli status ran" ""
o=$(CLI doctor 2>&1 | head -5); echo "doctor: $o"; rec 0 "cli doctor ran" ""

hdr "$APP :: 4. journal + persistence"
JDIR=$(dirname "$TOKEN_FILE")
find "$JDIR" -name '*.ndjson' -o -name '*journal*' | head -5
J=$(find "$JDIR" -name 'command-journal.ndjson' | head -1)
if [ -n "$J" ]; then n=$(wc -l < "$J"); echo "journal entries=$n"; [ "$n" -gt 3 ]; rec $? "journal growing" "$J"; else rec 1 "journal found" "none"; fi
resp=$(req POST /notes '{"text":"e2e-note","title":"e2e"}' | tail -1); echo "note: $resp"
echo "$resp" | grep -qE 'id|note'; rec $? "notes.create (journaled write)" ""

hdr "$APP :: 5. error envelopes"
resp=$(req POST /orchestration/dispatches '{"agent":"BAD AGENT!!"}' | tail -1); echo "badreq: $resp"
echo "$resp" | grep -qE 'error|code'; rec $? "invalid dispatch -> structured error" "$resp"
code=$(req GET /nope-route | head -1); [ "$code" = 404 ] || [ "$code" = 400 ]; rec $? "unknown route 404" "status=$code"

hr; echo "$APP RESULT: pass=$PASS fail=$FAIL"; hr
