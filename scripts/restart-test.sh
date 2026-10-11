#!/usr/bin/env bash
# restart persistence test: create state -> SIGTERM -> relaunch -> verify replay
set -u
APP="${1:?slate|orc}"
if [ "$APP" = slate ]; then
  SOCK=/tmp/slate-e2e.sock; TOKF=$HOME/slate-e2e-data/control-token; HDR=x-slate-token
  START() { LD_LIBRARY_PATH=$HOME/gtkroot/usr/lib SLATE_SOCKET_PATH=$SOCK SLATE_TEST_USER_DATA=$HOME/slate-e2e-data nohup ~/slate/native/target/release/slate --headless >/tmp/slate-e2e.log 2>&1 </dev/null & }
  PAT='slate/target/release/slate --headless'
else
  SOCK=/tmp/orcspace.sock; TOKF=$HOME/.config/orcspace/control-token; HDR=x-orcspace-token
  START() { ORCSPACE_DISABLE_GPU=1 nohup dbus-run-session -- xvfb-run -a -s "-screen 0 1600x1000x24" $HOME/opt/OrcSpace/orcspace --no-sandbox --disable-gpu --no-first-run >/tmp/orc-e2e.log 2>&1 </dev/null & }
  PAT='opt/OrcSpace/orcspace --no-sandbox'
fi
req(){ curl -s --unix-socket "$SOCK" -H "$HDR: $(cat $TOKF)" -H 'content-type: application/json' "$@"; }
echo "=== $APP restart test ==="
# state before: widgets + tasks count
W1=$(req http://localhost/widgets?agentId=e2e | python3 -c 'import sys,json;d=json.load(sys.stdin);print(len(d.get("widgets") or (d.get("data") or {}).get("widgets") or (d if isinstance(d,list) else [])))' 2>/dev/null)
T1=$(req http://localhost/orchestration/tasks?agentId=e2e | python3 -c 'import sys,json;d=json.load(sys.stdin);print(len(d.get("tasks", (d.get("data") or {}).get("tasks", []))))' 2>/dev/null)
echo "before: widgets=$W1 tasks=$T1"
pkill -TERM -f "$PAT"; sleep 3
rm -f "$SOCK"
START; sleep 6
H=$(req http://localhost/health)
echo "after health: $(echo "$H" | head -c 120)"
W2=$(req http://localhost/widgets?agentId=e2e | python3 -c 'import sys,json;d=json.load(sys.stdin);print(len(d.get("widgets") or (d.get("data") or {}).get("widgets") or (d if isinstance(d,list) else [])))' 2>/dev/null)
T2=$(req http://localhost/orchestration/tasks?agentId=e2e | python3 -c 'import sys,json;d=json.load(sys.stdin);print(len(d.get("tasks", (d.get("data") or {}).get("tasks", []))))' 2>/dev/null)
echo "after: widgets=$W2 tasks=$T2"
[ "${W2:-0}" -ge "${W1:-1}" ] && echo "PASS widgets replayed" || echo "FAIL widgets $W1->$W2"
[ "${T2:-0}" -ge "${T1:-1}" ] && echo "PASS tasks replayed" || echo "FAIL tasks $T1->$T2"
