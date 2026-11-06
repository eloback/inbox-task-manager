#!/usr/bin/env bash
# End-to-end smoke run of the Rust itm against a real headless Herdr, with
# tmux as the terminal (DESIGN.md section 6). Build first, then run it in a
# sandbox (a clean environment with private XDG folders), never against
# your real sessions:
#
#   cargo build --release
#   scripts/sandbox.sh e2e scripts/smoke.sh; rm -rf /tmp/itm-e2e
#
# ITM (default target/release/itm) and ITM_PYTHON (the Python itm that
# reads the state at the end; skipped when missing) choose the binaries.
#
# It starts Herdr servers, fake agents (a pane running `sleep` that reports
# itself blocked over the socket API; no real agent), the daemon and an
# `itm attach` loop in tmux, checks each step, and stops everything it
# started. Prints PASS/FAIL per check; exits 1 if any failed.
set -u
root=${ITM_SANDBOX:?run this through scripts/sandbox.sh}
repo=$(cd "$(dirname "$0")/.." && pwd)
itm=${ITM:-$repo/target/release/itm}
python_itm=${ITM_PYTHON:-/tmp/itm-standoff-cache/python-ref/bin/itm}
runtime=$XDG_RUNTIME_DIR/inbox-task-manager
sessions=$XDG_CONFIG_HOME/herdr/sessions
tmux_sock=$root/tmux.sock
failed=0
pids=()

check() { # check <description> <command...>
  local what=$1; shift
  if "$@"; then echo "PASS $what"; else echo "FAIL $what"; failed=1; fi
}
contains() { grep -qF -- "$2" "$1" 2>/dev/null; }
wait_for() { # wait_for <seconds> <command...>
  local deadline=$((SECONDS + $1)); shift
  until "$@"; do [ $SECONDS -ge $deadline ] && return 1; sleep 0.1; done
}
call() { # call <session> <json request>: one request on the session's socket
  printf '%s\n' "$2" | socat -t 5 - "UNIX-CONNECT:$sessions/$1/herdr.sock"
}
field() { python3 -c "import json,sys; print(eval('json.load(sys.stdin)' + sys.argv[1]))" "$1"; }
workspace() { # workspace <session> <label> <focus>: prints the new workspace's root pane
  call "$1" "{\"id\":\"w\",\"method\":\"workspace.create\",\"params\":{\"cwd\":\"$HOME\",\"label\":\"$2\",\"focus\":$3}}" \
    | field "['result']['root_pane']['pane_id']"
}
fake_agent() { # fake_agent <session> <pane>: a non-shell process that reports itself blocked
  call "$1" "{\"id\":\"s\",\"method\":\"pane.send_input\",\"params\":{\"pane_id\":\"$2\",\"text\":\"exec sleep 900\",\"keys\":[\"enter\"]}}" >/dev/null
  sleep 0.5
  call "$1" "{\"id\":\"r\",\"method\":\"pane.report_agent\",\"params\":{\"pane_id\":\"$2\",\"source\":\"custom:itmtest\",\"agent\":\"fakebot\",\"state\":\"blocked\"}}" >/dev/null
}
start_server() { # a headless session, as itm's first start does
  HERDR_SESSION=$1 setsid herdr server >"$root/server-$1.log" 2>&1 &
  wait_for 5 test -S "$sessions/$1/herdr.sock"
}
marker() { cat "$runtime"/clients/[0-9]* 2>/dev/null | head -1; }
# Predicates for wait_for, which runs its command again until it succeeds.
marker_is() { test "$(marker)" = "$1"; }
file_is() { test "$(cat "$1" 2>/dev/null)" = "$2"; }
no_markers() { test -z "$(ls "$runtime/clients")"; }
pane() { tmux -S "$tmux_sock" capture-pane -p -t e2e; }
prompt() { pane | grep -qE '[$#] *$'; }
type_in_pane() { # once the pane's shell shows its prompt: keys typed before it are lost
  wait_for 5 prompt
  tmux -S "$tmux_sock" send-keys -t e2e -l "$1"
  tmux -S "$tmux_sock" send-keys -t e2e Enter
}

cleanup() {
  for pid in "${pids[@]}"; do kill "$pid" 2>/dev/null; done
  tmux -S "$tmux_sock" kill-server 2>/dev/null
  for s in task-fix-login task-write-docs task-later task-old-idea task-two-repos; do herdr session stop "$s" >/dev/null 2>&1; done
}
trap cleanup EXIT

echo "== tasks"
"$itm" new "Fix login" -d "Users get logged out after 5 min" --no-switch >/dev/null
"$itm" new "Write docs" --no-switch >/dev/null
"$itm" new "Old idea" --no-switch >/dev/null
"$itm" new "Later" --snooze 2h >/dev/null
"$itm" new "Shipped" --no-switch >/dev/null
"$itm" archive shipped >/dev/null
check "new, snooze at creation and archive" test "$("$itm" status)" = "⚑ ? ● 3 ◔ 1"
check "list --view active" test "$("$itm" list --view active | cut -f1 | sort | tr '\n' ' ')" = "task-fix-login task-old-idea task-write-docs "

echo "== projects"
mkdir -p "$HOME/code/api" "$HOME/code/web"
check "project add dir (~ expanded by itm)" test "$("$itm" project add dir '~/code/api')" = api
"$itm" project add dir "$HOME/code/web" --name web >/dev/null
check "project list" test "$("$itm" project list | cut -f1,2 | tr '\n' ' ')" = "api	dir web	dir "
"$itm" new "Two repos" -p api -p web --no-switch >/dev/null
check "new -p stores the projects in order" test "$("$itm" list --json | python3 -c 'import json,sys; print([p["name"] for r in json.load(sys.stdin) if r["session"] == "task-two-repos" for p in r["projects"]])')" = "['api', 'web']"

echo "== sessions and fake agents"
for s in task-fix-login task-write-docs task-later; do start_server "$s" || echo "FAIL server $s"; done
agent_pane=$(workspace task-fix-login agent false)
workspace task-fix-login shell true >/dev/null
workspace task-write-docs shell true >/dev/null
later_pane=$(workspace task-later agent true)
fake_agent task-fix-login "$agent_pane"
check "running sessions are listed" test "$("$itm" list --json | python3 -c 'import json,sys; print(sum(r["running"] for r in json.load(sys.stdin)))')" = 3

echo "== daemon"
"$itm" daemon 2>"$root/daemon.log" &
pids+=($!)
check "a blocked agent notifies, naming task and agent" \
  wait_for 6 contains "$root/notifications.log" "Needs input · Fix login | fakebot:"
check "the status cache counts the wait" test "$("$itm" status)" = "⚑ 1 ● 4 ◔ 1"
fake_agent task-later "$later_pane"
check "a blocked agent wakes a snoozed task" wait_for 6 contains "$root/notifications.log" "Woke · Needs input · Later"
sleep 2.5
check "each wait is reported once" test "$(grep -c "Needs input" "$root/notifications.log")" = 2
check "notified.json has Python's keys" contains "$XDG_STATE_HOME/inbox-task-manager/notified.json" "task-fix-login|$agent_pane|"

echo "== attach loop and switching in tmux"
tmux -S "$tmux_sock" -f /dev/null new-session -d -s e2e -x 120 -y 40 "$itm attach task-write-docs; sleep 600"
check "itm attach registers its marker" wait_for 5 marker_is task-write-docs
# A freshly attached Herdr client drops the first typed lines: type a probe until its output shows.
echoed() { pane | grep -qx ' *│*itm-ready *'; }
for _ in 1 2 3 4 5 6 7 8 9 10; do type_in_pane "echo itm-ready"; wait_for 1 echoed && break; done
type_in_pane "$itm current > $root/current.txt"
check "itm current inside the session" wait_for 5 file_is "$root/current.txt" task-write-docs
type_in_pane "$itm ui"
check "the switcher shows the inbox inside Herdr" wait_for 5 sh -c "tmux -S '$tmux_sock' capture-pane -p -t e2e | grep -q 'NEEDS YOU'"
pane >"$root/switcher.txt"
tmux -S "$tmux_sock" send-keys -t e2e Enter
check "Enter in the switcher moves this client" wait_for 5 marker_is task-fix-login
sleep 1
type_in_pane "$itm snooze 1h"
check "snoozing the current task moves its client to the waiting task (R4.7)" wait_for 5 marker_is task-later
check "the snoozed task is snoozed" test "$("$itm" list --view snoozed | cut -f1)" = task-fix-login
"$itm" archive task-later >"$root/archive.txt" 2>&1
check "archiving moves the client off and stops the session" wait_for 5 marker_is task-write-docs
check "the archived session stopped" wait_for 5 sh -c "! socat -u /dev/null UNIX-CONNECT:$sessions/task-later/herdr.sock 2>/dev/null"
"$itm" wake fix-login >/dev/null
check "wake" test "$("$itm" list --view snoozed)" = ""

type_in_pane "$itm attach old-idea"
check "itm attach inside Herdr switches by title" wait_for 5 marker_is task-old-idea
check "a new task's session starts in \$HOME" wait_for 5 sh -c "printf '%s\n' '{\"id\":\"p\",\"method\":\"pane.list\",\"params\":{}}' | socat -t 5 - UNIX-CONNECT:$sessions/task-old-idea/herdr.sock | grep -q '\"cwd\":\"$HOME\"'"

type_in_pane "$itm attach two-repos"
check "a first start through a hand-off" wait_for 10 marker_is task-two-repos
workspaces() { call task-two-repos '{"id":"l","method":"workspace.list","params":{}}' | python3 -c 'import json,sys; print(" ".join(w["label"] for w in json.load(sys.stdin)["result"]["workspaces"]))'; }
check "the session has one named workspace per project" test "$(workspaces)" = "api web"
check "the first project's workspace starts in its folder" sh -c "printf '%s\n' '{\"id\":\"p\",\"method\":\"pane.list\",\"params\":{}}' | socat -t 5 - UNIX-CONNECT:$sessions/task-two-repos/herdr.sock | grep -q '\"cwd\":\"$HOME/code/api\"'"

type_in_pane "$itm ui"
wait_for 5 sh -c "tmux -S '$tmux_sock' capture-pane -p -t e2e | grep -q 'NEEDS YOU'"
tmux -S "$tmux_sock" send-keys -t e2e C-z
check "Ctrl+Z opens the snooze dialog with presets" wait_for 5 sh -c "tmux -S '$tmux_sock' capture-pane -p -t e2e | grep -q '30 minutes'"
tmux -S "$tmux_sock" send-keys -t e2e Enter
check "a preset snoozes the highlighted task" wait_for 5 sh -c "'$itm' list --view snoozed | grep -q task-fix-login"
tmux -S "$tmux_sock" send-keys -t e2e Escape
type_in_pane "$itm jump"
check "itm jump opens the longest wait" wait_for 5 marker_is task-fix-login
type_in_pane "$itm last"
check "itm last goes back" wait_for 5 marker_is task-two-repos

echo "== notes"
notes=$("$itm" notes old-idea --path)
check "notes start with the task" sh -c "head -1 '$notes' | grep -qx '# Old idea'"
echo "from an agent" | "$itm" notes old-idea --append - >/dev/null
check "notes --append adds a dated note" sh -c "tail -2 '$notes' | grep -qx 'from an agent'"

echo "== the Python itm reads this state"
if [ -x "$python_itm" ]; then
  check "python itm list agrees" test "$("$python_itm" list | cut -f1,2 | sort)" = "$("$itm" list | cut -f1,2 | sort)"
else
  echo "SKIP python itm list agrees ($python_itm missing)"
fi

echo "== detach"
attach_child=$(pgrep -f "^herdr session attach" -P "$(ls "$runtime/clients" | grep -E '^[0-9]+$' | head -1)")
kill -TERM "$attach_child"
check "a detach without a hand-off ends the loop and removes its marker" wait_for 5 no_markers

echo "== logs"
sed 's/^/  notification: /' "$root/notifications.log"
sed 's/^/  daemon: /' "$root/daemon.log"
sed 's/^/  switcher: /' "$root/switcher.txt" | head -14
exit $failed
