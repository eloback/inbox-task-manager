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

fails=0
check() { # check <description> <command...>; a failure saves the terminal and the tasks
  local what=$1; shift
  if "$@"; then echo "PASS $what"; return; fi
  fails=$((fails + 1)); failed=1
  echo "FAIL $what (see $root/fail-$fails.txt)"
  { tmux -S "$tmux_sock" capture-pane -p -t e2e 2>/dev/null; "$itm" list; } >"$root/fail-$fails.txt" 2>&1
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
marker_is_not() { test -n "$(marker)" && test "$(marker)" != "$1"; }
sandbox_pids() { # sandbox_pids <command line>: the processes with exactly it, of this sandbox only
  for pid in $(pgrep -fx "$1"); do
    tr '\0' '\n' <"/proc/$pid/environ" 2>/dev/null | grep -qx "ITM_SANDBOX=$root" && echo "$pid"
  done
}
file_is() { test "$(cat "$1" 2>/dev/null)" = "$2"; }
snoozed_for() { # snoozed_for <session> <seconds>: its snooze ends that far from now, give or take 15 s
  "$itm" list --json --view all | python3 -c 'import json,sys,time; r=[r for r in json.load(sys.stdin) if r["session"]==sys.argv[1]]; sys.exit(not (r and r[0]["snoozed_until"] and abs(r[0]["snoozed_until"] - time.time() - int(sys.argv[2])) < 15))' "$1" "$2"
}
no_markers() { test -z "$(ls "$runtime/clients")"; }
pane() { tmux -S "$tmux_sock" capture-pane -p -t e2e; }
shows() { pane | grep -qF -- "$1"; }
gone() { ! shows "$1"; }
keys() { tmux -S "$tmux_sock" send-keys -t e2e "$@"; }
probes=0
ready() { # a freshly (re)attached Herdr client drops the first keys: echo a new token until it shows
  probes=$((probes + 1))
  local token="itm-ready-$probes"
  for _ in 1 2 3 4 5 6 7 8 9 10; do
    keys -l "echo $token"; keys Enter
    wait_for 1 sh -c "tmux -S '$tmux_sock' capture-pane -p -t e2e | grep -qE '^[ │]*$token[ │]*\$'" && return 0
  done
  echo "FAIL the pane never echoed $token"; failed=1
}
# Only at a shell prompt: typed into the switcher, a probe would create a task. After
# Escape closes the switcher, wait for it to go (ESC and a quick "e" arrive as Alt+e).
type_in_pane() { ready; keys -l "$1"; keys Enter; }

cleanup() {
  for pid in "${pids[@]}"; do kill "$pid" 2>/dev/null; done
  tmux -S "$tmux_sock" kill-server 2>/dev/null
  # Every session in the sandbox is this run's, whatever made it.
  for dir in "$sessions"/*/; do [ -d "$dir" ] && herdr session stop "$(basename "$dir")" >/dev/null 2>&1; done
}
trap cleanup EXIT

# Herdr's popups (contrib/herdr-keys.toml, its keys; not its toast setting,
# which would change where the checks below find notifications) run `itm`
# from the server's PATH, with the server's EDITOR. Every server reads it
# when it starts, so it goes in first. $root/bin gets a fake git below: a
# clone in a popup never reaches the network.
mkdir -p "$root/bin-itm" "$root/bin" "$XDG_CONFIG_HOME/herdr"
ln -sf "$itm" "$root/bin-itm/itm"
export PATH="$root/bin-itm:$root/bin:$PATH"
sed '/^\[ui.toast\]/,$d' "$repo/contrib/herdr-keys.toml" >"$XDG_CONFIG_HOME/herdr/config.toml"
printf '#!/bin/sh\necho "written in the editor" >>"$1"\n' >"$root/editor.sh"
chmod +x "$root/editor.sh"
export EDITOR=$root/editor.sh

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
type_in_pane "$itm current > $root/current.txt"
check "itm current inside the session" wait_for 5 file_is "$root/current.txt" task-write-docs
type_in_pane "$itm ui"
check "the switcher shows the inbox inside Herdr" wait_for 5 shows 'NEEDS YOU'
pane >"$root/switcher.txt"
# Two tasks wait (Fix login, Later), in the order of their last use, which can share a second: search.
keys -l "Fix login"
wait_for 5 shows '› Fix login'
keys Enter
check "Enter in the switcher moves this client" wait_for 5 marker_is task-fix-login
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

# last_used_at counts seconds (the format the Python itm shares), and these steps take less:
# let the clock move so that `itm last` below has one most recent task, not a tie.
sleep 1.1
type_in_pane "$itm attach two-repos"
check "a first start through a hand-off" wait_for 10 marker_is task-two-repos
workspaces() { call task-two-repos '{"id":"l","method":"workspace.list","params":{}}' | python3 -c 'import json,sys; print(" ".join(w["label"] for w in json.load(sys.stdin)["result"]["workspaces"]))'; }
# The marker names the task before its session is built (DESIGN.md section 5): wait for the build.
has_workspaces() { test "$(workspaces 2>/dev/null)" = "api web"; }
check "the session has one named workspace per project" wait_for 10 has_workspaces
check "the first project's workspace starts in its folder" sh -c "printf '%s\n' '{\"id\":\"p\",\"method\":\"pane.list\",\"params\":{}}' | socat -t 5 - UNIX-CONNECT:$sessions/task-two-repos/herdr.sock | grep -q '\"cwd\":\"$HOME/code/api\"'"

type_in_pane "$itm ui"
wait_for 5 shows 'NEEDS YOU'
keys C-z
check "Ctrl+Z opens the snooze dialog with presets" wait_for 5 shows '30 minutes'
keys Enter
check "a preset snoozes the highlighted task" wait_for 5 sh -c "'$itm' list --view snoozed | grep -q task-fix-login"
keys Escape
check "Esc closes the switcher" wait_for 5 gone '? all keys'
type_in_pane "$itm jump"
check "itm jump opens the longest wait" wait_for 5 marker_is task-fix-login
type_in_pane "$itm last"
check "itm last goes back" wait_for 5 marker_is task-two-repos
type_in_pane "$itm snooze"
check "itm snooze without a time opens the snooze dialog (R4.1)" wait_for 5 shows 'Snooze “Two repos”'
keys Enter
check "its first preset snoozes the current task and moves its client (R4.7)" wait_for 5 marker_is task-fix-login
check "the task is snoozed for 30 minutes" sh -c "'$itm' list --view snoozed | grep -q task-two-repos"
type_in_pane "$itm snooze old-idea"
check "itm snooze <task> without a time opens the dialog for that task" wait_for 5 shows 'Snooze “Old idea”'
keys -l "next week"
keys Enter
check "a phrase only GNU date reads, typed in the dialog, snoozes it (R4.1)" wait_for 5 snoozed_for task-old-idea 604800
"$itm" wake old-idea >/dev/null

echo "== notes"
notes=$("$itm" notes old-idea --path)
check "notes start with the task" sh -c "head -1 '$notes' | grep -qx '# Old idea'"
echo "from an agent" | "$itm" notes old-idea --append - >/dev/null
check "notes --append adds a dated note" sh -c "tail -2 '$notes' | grep -qx 'from an agent'"

echo "== edit and delete in running sessions (R5.4, R5.3)"
mkdir -p "$HOME/code/docs"
"$itm" project add dir '~/code/docs' >/dev/null
"$itm" project add name "Client X" >/dev/null
check "edit opens a named workspace in a running session" \
  test "$("$itm" edit "Two repos" --add-project docs --add-project "Client X")" \
  = "task-two-repos updated; Client X has no folder, so it gets no workspace; opened a workspace in ~/code/docs"
labels_are() { test "$(workspaces 2>/dev/null)" = "$1"; }
check "the new workspace is named after its project" labels_are "api web docs"
check "the edit is in the notes" contains "$("$itm" notes two-repos --path)" "Project added"
check "delete --yes of a running task" test "$("$itm" delete write-docs --yes)" = "task-write-docs deleted"
check "its session stops and Herdr deletes it" wait_for 10 sh -c "! test -d '$sessions/task-write-docs'"
check "its tombstone expires once the session is gone" sh -c "'$itm' list >/dev/null; ! grep -q task-write-docs '$XDG_STATE_HOME/inbox-task-manager/tasks.json'"
type_in_pane "$itm delete old-idea"
check "itm delete asks first" wait_for 5 shows 'Delete “Old idea”?'
keys n
check "n keeps the task" wait_for 5 sh -c "'$itm' list | grep -q task-old-idea"
wait_for 5 gone 'y delete'
type_in_pane "$itm delete old-idea"
wait_for 5 shows 'Delete “Old idea”?'
keys y
check "y deletes it, its session and its notes" wait_for 10 sh -c "! test -d '$sessions/task-old-idea' && ! test -e '$notes'"

echo "== the switcher's notes and delete keys (R9.4, R9.5)"
type_in_pane "EDITOR=$root/editor.sh $itm ui"
wait_for 5 shows '? all keys'
keys -l "Two repos"
wait_for 5 shows '› Two repos'
keys Tab
wait_for 5 shows 'wake & open'
keys C-o
check "Ctrl+O runs the editor on the highlighted task's notes" wait_for 5 contains "$("$itm" notes two-repos --path)" "written in the editor"
check "the switcher comes back with the notes in the preview" wait_for 5 shows 'written in the editor'
keys C-d
check "Ctrl+D asks before deleting" wait_for 5 shows 'Delete “Two repos”?'
keys n
wait_for 5 gone 'y delete'
keys Escape
check "Esc closes the switcher" wait_for 5 gone '? all keys'

echo "== git projects (R8.6, R8.7), cloned without the network"
mkdir -p "$root/bin"
real_git=$(command -v git)
# A URL with "slow" in it clones until it is stopped, saying how far it got.
printf '#!/bin/sh\nif [ "$1" = clone ]; then case "$4" in *slow*) printf "Receiving objects:  10%%%% (1/10)\\r" >&2; exec sleep 60;; esac; "%s" init -q "$5" && exec "%s" -C "$5" remote add origin "$4"; fi\nexec "%s" "$@"\n' \
  "$real_git" "$real_git" "$real_git" >"$root/bin/git"
chmod +x "$root/bin/git"
clone=$XDG_DATA_HOME/inbox-task-manager/projects/example.com/acme/tool
check "project add git clones under the projects folder" \
  test "$(PATH="$root/bin:$PATH" "$itm" project add git https://example.com/acme/tool.git)" = tool
check "the clone is in place" test -d "$clone/.git"
type_in_pane "PATH=$root/bin:\$PATH $itm project remove tool"
check "removing a clean clone asks, keeping it by default" wait_for 5 shows 'Keep the clone  (default)'
keys Down
keys Enter
check "the clone is deleted when chosen" wait_for 5 sh -c "! test -e '$clone' && ! test -e '$(dirname "$clone")'"
check "the project is forgotten" sh -c "! '$itm' project list | grep -q tool"

echo "== projects follow workspaces, add-workspace and settings (R10)"
# The client is on task-fix-login; the daemon runs. Workspaces are opened over the socket.
open_workspace() { # open_workspace <session> <folder> <label or ""> <focus>: prints its id
  local label=""; [ -n "$3" ] && label=",\"label\":\"$3\""
  call "$1" "{\"id\":\"w\",\"method\":\"workspace.create\",\"params\":{\"cwd\":\"$2\"$label,\"focus\":$4}}" \
    | field "['result']['workspace']['workspace_id']"
}
close_workspace() { call "$1" "{\"id\":\"c\",\"method\":\"workspace.close\",\"params\":{\"workspace_id\":\"$2\"}}" >/dev/null; }
task_projects() { # task_projects <session>: its projects' names, in order
  "$itm" list --json | python3 -c 'import json,sys; print(" ".join(p["name"] for r in json.load(sys.stdin) if r["session"] == sys.argv[1] for p in r["projects"]))' "$1"
}
projects_are() { test "$(task_projects "$1")" = "$2"; }
before=$(task_projects task-fix-login)
web_ws=$(open_workspace task-fix-login "$HOME/code/web" "" false)
check "a workspace in a project's folder adds the project to its task (R10.3)" wait_for 8 projects_are task-fix-login "${before:+$before }web"
check "while it is open, an edit keeps it and says so (R5.4)" \
  sh -c "'$itm' edit 'Fix login' --remove-project web | grep -qF 'could not remove web: it still has a workspace in this task'"
check "the project stays" projects_are task-fix-login "${before:+$before }web"
close_workspace task-fix-login "$web_ws"
check "closing the workspace removes the project it brought (R10.3)" wait_for 8 projects_are task-fix-login "$before"

mkdir -p "$HOME/code/new-thing" "$HOME/code/client-y"
open_workspace task-fix-login "$HOME/code/new-thing" "" true >/dev/null
type_in_pane "$itm project add-workspace"
check "add-workspace in a terminal opens the add dialog with the workspace's folder (R10.5)" wait_for 5 shows 'Source  ~/code/new-thing'
check "and its name" shows 'Name    new-thing'
keys Enter
check "Enter saves the folder" wait_for 5 sh -c "'$itm' project list | grep -q '^new-thing	dir	~/code/new-thing'"
check "and gives it to the current task" wait_for 5 sh -c "'$itm' list --json | grep -q '\"name\": \"new-thing\"'"
check "as found in its workspace (projects follow workspaces)" \
  sh -c "python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); i=[k for k,p in d[\"projects\"].items() if p[\"name\"]==\"new-thing\"][0]; sys.exit(i not in d[\"tasks\"][\"task-fix-login\"].get(\"found_projects\",[]))' '$XDG_STATE_HOME/inbox-task-manager/tasks.json'"
"$itm" project add name "Client Y" >/dev/null
open_workspace task-fix-login "$HOME/code/client-y" "Client Y" true >/dev/null
type_in_pane "$itm project add-workspace"
check "a workspace named after a project without a folder asks first" wait_for 5 shows 'Give ~/code/client-y to Client Y?'
keys y
check "y gives it the folder" wait_for 5 sh -c "'$itm' project list | grep -q '^Client Y	dir	~/code/client-y'"
check "no second project was saved" test "$("$itm" project list | grep -c '^Client Y')" = 1
popup_ws=$(open_workspace task-fix-login "$HOME/code/docs" "" false)
HERDR_SOCKET_PATH="$sessions/task-fix-login/herdr.sock" HERDR_ACTIVE_WORKSPACE_ID="$popup_ws" "$itm" project add-workspace --name popup </dev/null >"$root/popup-ws.txt" 2>&1
# The daemon may have found docs in that workspace first: then the task has it already.
docs_picked() { grep -qxE "project docs \(~/code/docs\); (added to 'Fix login'|'Fix login' has it already)" "$root/popup-ws.txt"; }
check "a popup's workspace is the one it was opened over, given by HERDR_ACTIVE_WORKSPACE_ID" docs_picked
home_ws=$(open_workspace task-fix-login "$HOME" "" false)
HERDR_SOCKET_PATH="$sessions/task-fix-login/herdr.sock" HERDR_ACTIVE_WORKSPACE_ID="$home_ws" "$itm" project add-workspace --name everything </dev/null >"$root/home.txt" 2>&1
check "the home folder is refused" contains "$root/home.txt" "itm: ~ holds every workspace under it"

config=$XDG_CONFIG_HOME/inbox-task-manager/config.toml
type_in_pane "$itm ui"
wait_for 5 shows '? all keys'
keys C-s
check "Ctrl+S opens the settings (R10.4)" wait_for 5 shows 'Herdr integration'
keys Down
keys Enter
check "a toggle writes config.toml" wait_for 5 contains "$config" "remove_projects_without_workspace = false"
keys Enter
check "and toggles back" wait_for 5 contains "$config" "remove_projects_without_workspace = true"
keys Escape
wait_for 5 gone 'Herdr integration'
keys Escape
check "Esc closes the settings, then the switcher" wait_for 5 gone '? all keys'
printf '# mine\n[herdr\n' >"$config"
sleep 4.5
check "a broken settings file is logged once by the daemon" test "$(grep -c 'settings ignored:' "$root/daemon.log")" = 1
check "with what is wrong" contains "$root/daemon.log" "config.toml is not valid TOML: unclosed table, expected \`]\` (at line 2, column 7)"
type_in_pane "$itm ui"
wait_for 5 shows '? all keys'
keys C-s
check "the Settings screen shows the mistake" wait_for 5 shows 'is not valid TOML:'
keys Escape
wait_for 5 gone 'Herdr integration'
keys Escape
wait_for 5 gone '? all keys'
printf '# mine\n[herdr]\nprojects_from_workspaces = true  # keep\n' >"$config"

echo "== the switcher's screens: help, @filters, Projects, the add dialog, the New task form (milestone 4)"
"$itm" new "Filter me" -p web --no-switch >/dev/null
"$itm" project add name "Scratch" >/dev/null
type_in_pane "PATH=$root/bin:\$PATH $itm ui"
wait_for 5 shows '? all keys'
keys '?'
check "? on an empty search shows every key" wait_for 5 shows 'Project finder'
keys Escape
wait_for 5 gone 'Project finder'
keys -l "@web"
only_web() { shows 'Filter me' && ! shows 'Fix login'; }
check "@web shows only the tasks with the project web (R6.2)" wait_for 5 only_web
keys BSpace BSpace BSpace BSpace
keys C-p
check "Ctrl+P shows the projects (R8.3)" wait_for 5 shows 'repositories and folders tasks start in'
check "each with its tasks and last use" shows 'used now'
keys C-r
check "Ctrl+R renames the project highlighted, the most recent first" wait_for 5 shows 'Rename project “Scratch”'
keys BSpace BSpace BSpace BSpace BSpace BSpace BSpace
keys -l "Scratched"
keys Enter
check "the rename is saved" wait_for 5 sh -c "'$itm' project list | grep -q '^Scratched'"
check "and said" wait_for 5 shows 'Renamed Scratch to Scratched.'
keys C-d
check "Ctrl+D asks before removing it (R8.7)" wait_for 5 shows 'Remove project “Scratched”?'
keys Enter
check "Enter forgets it" wait_for 5 sh -c "! '$itm' project list | grep -q '^Scratched'"
keys C-n
check "Ctrl+N adds a project, a repository first" wait_for 5 shows 'Git repository'
keys -l "https://example.com/acme/slow.git"
check "what is typed is checked: where the clone goes (R8.5)" wait_for 5 shows "Clones to $XDG_DATA_HOME/inbox-task-manager/projects/example.com/acme/slow"
keys Enter
check "a clone shows git's progress (R8.8)" wait_for 10 shows 'Cloning… Receiving objects:  10% (1/10)'
keys Escape
check "Esc stops the clone" wait_for 15 shows 'Stopped. Nothing of the clone was kept.'
check "and nothing of it is left" sh -c "! test -e '$XDG_DATA_HOME/inbox-task-manager/projects/example.com/acme' && ! pgrep -f '[p]roject add git -- https://example.com/acme/slow'"
keys C-u
keys -l "https://example.com/acme/tool2.git"
keys Enter
check "a clone that ends is said on the Projects screen" wait_for 10 shows 'Project added.'
check "and saved" sh -c "'$itm' project list | grep -q '^tool2	git'"
keys Escape
wait_for 5 gone 'repositories and folders'
keys C-n
check "Ctrl+N opens the New task form (R1.1)" wait_for 5 shows 'enter create'
keys -l "Form task"
keys Tab
keys -l "from the form"
keys Tab
keys -l "~/code/"
check "a path on the projects field lists its folders, saved ones named (R1.6)" wait_for 5 shows '~/code/api/  ◆ api saved'
keys BSpace BSpace BSpace BSpace BSpace BSpace BSpace
keys -l "wb"
check "a project is found by its letters in order" wait_for 5 shows '◆ web '
keys Enter
# The form started with the first project of the task this switcher runs in (R1.6).
check "Enter adds it after the current task's first project; the form says where it starts" wait_for 5 shows 'Workspaces new-thing · web'
keys C-s
check "Ctrl+S creates the task and switches to it" wait_for 10 marker_is task-form-task
form_task() { "$itm" list --json | python3 -c 'import json,sys; r=[r for r in json.load(sys.stdin) if r["session"]=="task-form-task"][0]; print(r["description"], [p["name"] for p in r["projects"]])'; }
check "with its description and projects" test "$(form_task)" = "from the form ['new-thing', 'web']"
type_in_pane "$itm edit 'Form task'"
check "itm edit without options opens the edit form (R5.4)" wait_for 5 shows 'Edit task'
keys End
keys -l " now"
keys C-s
check "Ctrl+S saves the edit" wait_for 5 sh -c "'$itm' list | grep -q '	Form task now	'"
type_in_pane "$itm new"
check "itm new without a title opens the form" wait_for 5 shows 'enter create'
keys Escape
check "Esc closes it when nothing was typed" wait_for 5 gone 'enter create'
type_in_pane "$itm new"
wait_for 5 shows 'enter create'
keys -l "Week task"
keys Tab Tab Tab
keys -l "+3 days"
keys C-s
check "the form's time typed is read by GNU date when itm does not read it (R4.1)" wait_for 5 snoozed_for task-week-task 259200
check "and a snoozed task is not switched to" marker_is task-form-task
# R1.4: a terminal whose Herdr client itm attach did not start (here: the
# socket of a session no itm client is on) cannot be switched.
type_in_pane "HERDR_SOCKET_PATH=$sessions/task-fix-login/herdr.sock $itm new 2>$root/plain.txt"
check "there the form says it saves without switching (R1.4)" wait_for 5 shows 'Saved without switching: this Herdr client'
keys -l "Plain task"
keys Enter
check "Enter saves the task" wait_for 5 sh -c "'$itm' list | grep -q task-plain-task"
check "then says why it did not switch" wait_for 5 contains "$root/plain.txt" "task-plain-task saved without switching: this Herdr client was not started by \`itm attach\`"
check "and no client moved" marker_is task-form-task

echo "== every contrib/herdr-keys.toml command in a Herdr popup"
keys C-b S
check "prefix+shift+s: the switcher" wait_for 5 shows '? all keys'
keys Escape
check "Esc closes it" wait_for 5 gone '? all keys'
keys C-b S
wait_for 5 shows '? all keys'
keys C-p
wait_for 5 shows 'repositories and folders'
keys C-n
keys -l "https://example.com/acme/slow2.git"
keys Enter
check "a clone runs in the popup's switcher" wait_for 10 shows 'Cloning… Receiving objects'
popup_itm=$(sandbox_pids itm)
# What closing the popup sends; the clone child, in a session of its own, gets nothing.
[ -n "$popup_itm" ] && kill -HUP $popup_itm
# Nothing of it: no process, no clone, no hidden folder (acme/ holds tool2, cloned above).
clone_gone() { ! pgrep -f '[p]roject add git -- https://example.com/acme/slow2' >/dev/null && ! ls -a "$XDG_DATA_HOME/inbox-task-manager/projects/example.com/acme" | grep -q slow2; }
check "SIGHUP during a clone stops it, keeping nothing (R8.8)" wait_for 10 clone_gone
# A signal is no error: nothing is said, so the popup does not stay to show it.
check "the switcher ends on the signal, not on a terminal error" sh -c "! tmux -S '$tmux_sock' capture-pane -p -t e2e | grep -q 'cannot read the terminal'"
check "and the popup closes" wait_for 5 gone 'repositories and folders'
# A form alone (prefix+t) has no 2 s refresh to wake it: the signal is still seen at once.
keys C-b t
wait_for 5 shows 'enter create'
keys Tab Tab
keys -l "https://example.com/acme/slow3.git"
keys Enter
wait_for 5 shows 'Clones to'
keys Enter
check "a clone runs from the New task form in its popup" wait_for 10 shows 'Cloning… Receiving objects'
[ -n "$(sandbox_pids 'itm new')" ] && kill -HUP $(sandbox_pids 'itm new')
slow3_gone() { ! pgrep -f '[p]roject add git -- https://example.com/acme/slow3' >/dev/null && ! ls -a "$XDG_DATA_HOME/inbox-task-manager/projects/example.com/acme" | grep -q slow3; }
check "SIGHUP stops it within a second, keeping nothing" wait_for 1 slow3_gone
wait_for 5 gone 'enter create'
keys C-b t
check "prefix+t: the New task form" wait_for 5 shows 'enter create'
keys -l "Popup task"
keys Enter
check "Enter in the title creates the task and switches to it" wait_for 10 marker_is task-popup-task
ready
keys C-b Z
check "prefix+shift+z: the snooze dialog for the current task" wait_for 5 shows 'Snooze “Popup task”'
keys Enter
check "a preset snoozes it and moves the client on (R4.7)" wait_for 10 marker_is_not task-popup-task
check "it is snoozed" sh -c "'$itm' list --view snoozed | grep -q task-popup-task"
notes_here=$("$itm" notes "$(marker)" --path)
ready # the client was handed to another session: it drops the first keys
keys C-b E
check "prefix+shift+e: the current task's notes in the editor" wait_for 5 contains "$notes_here" "written in the editor"
# The editor wrote; the popup closes once its itm exits, and takes every key until then.
no_itm() { test -z "$(sandbox_pids "$1")"; }
wait_for 5 no_itm "itm notes"
here=$(marker)
mkdir -p "$HOME/code/popup"
open_workspace "$here" "$HOME/code/popup" "" true >/dev/null
ready # Herdr 0.9.5 drops the first keys after that popup closed and the new workspace took focus
keys C-b A
check "prefix+shift+a: the add dialog with the workspace's folder" wait_for 5 shows 'Source  ~/code/popup'
keys Enter
check "Enter saves it as a project" wait_for 5 sh -c "'$itm' project list | grep -q '^popup	dir	~/code/popup'"
wait_for 5 gone 'Source  ~/code/popup' # a popup takes every key, the prefix too, until it closed
keys C-b Tab
check "prefix+tab: the previous task" wait_for 10 marker_is_not "$here"
ready
keys C-b i
check "prefix+i: the task waiting longest" wait_for 10 marker_is task-fix-login

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
