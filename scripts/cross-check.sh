#!/usr/bin/env bash
# The Python itm (the behavior reference) and the Rust itm on shared state:
#
# - the same commands from the same state write the same tasks.json, byte
#   for byte, and print the same output and errors;
# - each lists the other's state as the other does (list, status, project
#   list, plain and --json);
# - an edit by one of a file the other wrote changes nothing it should not
#   (snooze then wake leaves it byte for byte), the old single-project
#   format included, and one waits for the other's lock.
#
# Timestamps are the only values masked (to <t>): the two runs of a step
# happen a moment apart. Project ids are random, so projects are added by
# one implementation and the state copied to the other.
#
#   cargo build --release
#   scripts/sandbox.sh cross scripts/cross-check.sh; rm -rf /tmp/itm-cross
#
# ITM (default target/release/itm) and ITM_PYTHON (default the pinned
# Python build at ~/.cache/itm/python-ref/bin/itm, set by sandbox.sh) choose the
# binaries. The Python itm left the tree at the cutover; its last version
# (0.2.0, the Rust rewrite's base) builds from that commit's flake:
#
#   nix build -o ~/.cache/itm/python-ref \
#     "git+file://$PWD?rev=7bdbaa491bd7d2d6ca7e4297c7199fcf3d1fa852"
# The Nix build of the Python itm puts its own git first on PATH,
# so git projects run its inner entry point, `.itm-wrapped` next to it
# (ITM_PYTHON_BARE), with a git whose clone makes an empty checkout of the
# URL instead of fetching it (no network). Herdr workspaces (R10) need
# `herdr` and `socat`: each side gets its own headless Herdr session with
# the same workspaces, and its own daemon runs for a few passes at a time.
# Prints PASS/FAIL per check (what the Rust itm printed at each step is in
# cross/steps.log); exits 1 if any failed. Later milestones add their
# commands here.
set -u
root=${ITM_SANDBOX:?run this through scripts/sandbox.sh}
repo=$(cd "$(dirname "$0")/.." && pwd)
rust=${ITM:-$repo/target/release/itm}
python=${ITM_PYTHON:-}
python_bare=${ITM_PYTHON_BARE:-$(dirname "$python")/.itm-wrapped}
[ -x "$rust" ] || { echo "no Rust itm at $rust (cargo build --release, or set ITM)"; exit 2; }
[ -x "$python" ] || { echo "no Python itm at $python (set ITM_PYTHON)"; exit 2; }
work=$root/cross
rm -rf "$work"
mkdir -p "$work"
failed=0

check() { # check <description> <command...>
  local what=$1; shift
  if "$@"; then echo "PASS $what"; else echo "FAIL $what"; failed=1; fi
}

# Each side has its own state, data, runtime and Herdr folders; the home folder is shared.
# With IN_SESSION set, a command runs inside that session of the side's own Herdr.
side() { # side <name> <command...>
  local dir=$work/$1; shift
  mkdir -p "$dir/state" "$dir/data" "$dir/config" "$dir/runtime"
  env XDG_STATE_HOME="$dir/state" XDG_DATA_HOME="$dir/data" XDG_CONFIG_HOME="$dir/config" \
    XDG_RUNTIME_DIR="$dir/runtime" ${IN_SESSION:+HERDR_SOCKET_PATH=$dir/config/herdr/sessions/$IN_SESSION/herdr.sock} "$@"
}
tasks() { echo "$work/$1/state/inbox-task-manager/tasks.json"; }
# What may differ between two runs of one command: when, and which side's folders.
normalize() { sed -E -e 's/[0-9]{10}/<t>/g' -e "s#$work/[a-z0-9-]+/#<side>/#g" -e 's/[0-9]{4}-[0-9]{2}-[0-9]{2} [0-9]{2}:[0-9]{2}/<date>/g'; }
same() { # same <file> <file>: equal once normalized; shows the difference otherwise
  diff <(normalize <"$1") <(normalize <"$2") >"$work/diff.txt" 2>&1 && return 0
  sed 's/^/    /' "$work/diff.txt" | head -20
  return 1
}
same_bytes() { cmp -s "$1" "$2" || { diff "$1" "$2" | sed 's/^/    /' | head -20; return 1; }; }

# Project ids are random: where each side adds its own projects, tasks.json
# compares with every id replaced by its project's name.
canon() { # canon <tasks.json>
  python3 - "$1" <<'EOF'
import json, sys
data = json.load(open(sys.argv[1]))
names = {pid: "<" + p["name"] + ">" for pid, p in data.get("projects", {}).items()}
data["projects"] = {names[pid]: p for pid, p in data.get("projects", {}).items()}
for task in data.get("tasks", {}).values():
    for key in ("projects", "found_projects", "workspace_projects"):
        if isinstance(task.get(key), list):
            task[key] = [names.get(pid, pid) for pid in task[key]]
print(json.dumps(data, indent=2, sort_keys=True))
EOF
}

step=0
ids=same # "canon" once the sides have added their own projects
both() { # both <args...>: the same command on side py (Python) and side rs (Rust); stdin from $STDIN
  step=$((step + 1))
  local name
  for name in py rs; do
    local itm=$python; [ $name = rs ] && itm=$rust
    side $name "$itm" "$@" >"$work/$name.out" 2>"$work/$name.err" <"${STDIN:-/dev/null}"
    echo "exit $?" >>"$work/$name.out"
    if [ ! -f "$(tasks $name)" ]; then : >"$work/$name.tasks"
    elif [ $ids = canon ]; then
      canon "$(tasks $name)" >"$work/$name.tasks"
      sed -i -E 's/"id": "[0-9a-f]{8}"/"id": "<id>"/' "$work/$name.out"
    else cp "$(tasks $name)" "$work/$name.tasks"; fi
  done
  { echo "## itm $*"; cat "$work/rs.out" "$work/rs.err"; } >>"$work/steps.log" # what Rust said, for a reader
  check "itm $* -- same output" same "$work/py.out" "$work/rs.out"
  check "itm $* -- same errors" same "$work/py.err" "$work/rs.err"
  check "itm $* -- same tasks.json" same "$work/py.tasks" "$work/rs.tasks"
}
notes_same() { # notes_same <session>: both sides' notes of a task, the same once normalized
  check "$1's notes are the same" same "$work/py/data/inbox-task-manager/tasks/$1/notes.md" \
    "$work/rs/data/inbox-task-manager/tasks/$1/notes.md"
}

reads() { # reads <side> <args...>: both implementations on one side's state print the same
  local state=$1; shift
  side "$state" "$python" "$@" >"$work/py.out" 2>&1 </dev/null; echo "exit $?" >>"$work/py.out"
  side "$state" "$rust" "$@" >"$work/rs.out" 2>&1 </dev/null; echo "exit $?" >>"$work/rs.out"
  check "itm $* on the $state state -- the same" same_bytes "$work/py.out" "$work/rs.out"
}

copy_state() { # copy_state <from> <to>
  mkdir -p "$work/$2/state/inbox-task-manager"
  cp "$(tasks "$1")" "$(tasks "$2")"
}

mkdir -p "$HOME/code/api" "$HOME/code/web" "$HOME/elsewhere/api"
echo "python: $python"
echo "rust:   $rust"

echo "== the same commands write the same state"
both new "Fix login" -d "  Users get logged out  " --no-switch
both new "Café crème: résumé #42!" --no-switch
both new "Later" --snooze 2h
both new "Fix login" --no-switch
both new "  " --no-switch
both new "Asked" --snooze agent
both new "Late" --snooze 0m
# Phrases itm does not read itself go to GNU date (R4.1).
both new "Next week" --snooze "next week"
both snooze task-next-week "+3 days"
both new "Monkey" --snooze monkey
both snooze fix-login 1h
both snooze task-later --until-agent
both wake fix-login
both wake fix-login
both archive task-fix-login-2
both archive no-such-task
both status
both status --json
both status --format '{waiting}/{active} {{x}} {daemon}'
both status --format '{nope}'
both list
both list --json
both list --view snoozed
both jump

snoozed() { # snoozed <side> <session> <seconds>: snoozed_until is that far after created_at
  python3 - "$(tasks "$1")" "$2" "$3" <<'EOF'
import json, sys
task = json.load(open(sys.argv[1]))["tasks"][sys.argv[2]]
sys.exit(not abs(task["snoozed_until"] - task["created_at"] - int(sys.argv[3])) <= 2)
EOF
}
check "py: new --snooze 2h wakes 2 h after it was made" snoozed py task-later 7200
check "rs: new --snooze 2h wakes 2 h after it was made" snoozed rs task-later 7200
check "py: snooze +3 days (GNU date) wakes 3 days after it was made" snoozed py task-next-week 259200
check "rs: snooze +3 days (GNU date) wakes 3 days after it was made" snoozed rs task-next-week 259200

echo "== notes"
both notes fix-login --append "first finding"
check "notes start and append the same" same "$work/py/data/inbox-task-manager/tasks/task-fix-login/notes.md" \
  "$work/rs/data/inbox-task-manager/tasks/task-fix-login/notes.md"
check "a unicode title's notes are the same" same "$work/py/data/inbox-task-manager/tasks/task-cafe-creme-resume-42/notes.md" \
  "$work/rs/data/inbox-task-manager/tasks/task-cafe-creme-resume-42/notes.md"

echo "== projects (added by Python, the state copied: their ids are random)"
side py "$python" project add dir '~/code/api' >/dev/null
side py "$python" project add dir "$HOME/code/web" --name web >/dev/null
side py "$python" project add name "Client X" >/dev/null
copy_state py rs
both new "Two repos" -p api -p WEB -p "client x" --no-switch
both new "Nope" -p missing --no-switch
# The New task and edit forms need a terminal; what was given is checked first (R1.1, R5.4).
both new --no-switch
both new -i "Asked for" -p api --no-switch
both new -p missing
both new --snooze agent
both new -i --snooze soonish
both edit two-repos
both edit no-such-task
both project add dir '~/code/api'
both project add dir '~/elsewhere/api' --name API
both project list
both project list --json

echo "== edit (R5.4) and the notes it writes (R9.3)"
both edit "Two repos" --title "  Two repos, renamed " --description "  why it matters  "
both edit "two repos, renamed" --title "Two repos" --description ""
both edit "Two repos" --remove-project api --add-project "Client X"
both edit "Two repos" --add-project api
both edit "Two repos" --remove-project nope
both edit "Two repos" --project web --add-project api
both edit "Two repos" --title " "
both edit "Two repos"
both edit nope --title x
both edit "Fix login" --project api --project WEB --project api
both edit "Fix login" --no-project
notes_same task-two-repos
notes_same task-fix-login

echo "== notes (R9.4)"
both notes "Two repos" --path
both notes "Two repos" --append " "
printf 'from stdin\nsecond line\n' >"$work/stdin"
STDIN=$work/stdin both notes "Two repos" --append -
both notes "Two repos"
both notes
both notes nope --append x
notes_same task-two-repos

echo "== project set and rename (R8.2)"
both project set "client x" --dir '~/elsewhere/api'
both project set "Client X" --dir '~/elsewhere/api'
both project set "Client X" --dir '~/code/api'
both project set "Client X" --dir '~/nowhere'
both project set "Client X" --no-dir
both project set "Client X" --no-dir
both project set web --dir '~/elsewhere/api'
both project rename "Client X" "Client Z"
both project rename "client z" "CLIENT Z"
both project rename "Client Z" web
both project rename web " "
both project rename nope x
both project remove web
both project list
both project list --json

echo "== delete (R5.3)"
both new "Doomed" --no-switch
both notes doomed --append "a finding"
both delete doomed
both delete doomed --yes
both delete doomed --yes
check "py: the notes went with the task" test ! -e "$work/py/data/inbox-task-manager/tasks/task-doomed"
check "rs: the notes went with the task" test ! -e "$work/rs/data/inbox-task-manager/tasks/task-doomed"
both list

echo "== projects each side adds (ids differ from here on)"
ids=canon
both project add name "Client Y"
both project add name "client y"
both project add name "  "
both project add name Other --name another
both project add dir '~/code/api' --name again
both project add dir '~/nowhere'
both project remove "Client Y"

echo "== git projects (R8.4 to R8.7), cloned without the network"
mkdir -p "$work/bin"
real_git=$(command -v git)
cat >"$work/bin/git" <<EOF
#!/bin/sh
# git, but a clone makes an empty checkout of the URL instead of fetching it
if [ "\$1" = clone ]; then
  "$real_git" init -q "\$5" && exec "$real_git" -C "\$5" remote add origin "\$4"
fi
exec "$real_git" "\$@"
EOF
chmod +x "$work/bin/git"
python_wrapped=$python
python=$python_bare
saved_path=$PATH
export PATH="$work/bin:$PATH" GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_COUNT=2 \
  GIT_CONFIG_KEY_0=url.https://example.com/.insteadOf GIT_CONFIG_VALUE_0=ex: \
  GIT_CONFIG_KEY_1=init.defaultBranch GIT_CONFIG_VALUE_1=main
both project add git git@example.com:acme/api.git
both project add git git@example.com:acme/api.git --name acme-api
both project add git https://example.com/acme/api
both project add git ssh://git@EXAMPLE.com:22/acme/api.git --name x
both project add git ex:acme/web --name acme-web
both project add git https://example.com/acme/web.git/ --name y
both project add git https://dev.azure.com/Acme/Platform/_git/api --name azure
both project add git git@ssh.dev.azure.com:v3/acme/Platform/api --name z
both project add git https://acme.visualstudio.com/DefaultCollection/Platform/_git/api --name z
both project add git git@my-alias:acme/api.git --name alias
both project add git /home/me/repo
both project add git 'ext::sh -c touch% /tmp/x'
both project add git https://example.com/acme/team/
both project set acme-api --no-dir
both project list
both project list --json
clones() { (cd "$work/$1/data/inbox-task-manager/projects" && find . | sort); }
check "both cloned into the same places" same <(clones py) <(clones rs)
both new "Uses azure" -p azure --no-switch
both project remove azure --delete-clone
both project remove acme-api
both project remove acme-web --delete-clone
both project remove alias --keep-clone
check "both kept and deleted the same clones" same <(clones py) <(clones rs)
both edit "Uses azure" --no-project
touch "$work/py/data/inbox-task-manager/projects/dev.azure.com/acme/Platform/api/draft"
touch "$work/rs/data/inbox-task-manager/projects/dev.azure.com/acme/Platform/api/draft"
both project remove azure
check "a clone with local work is kept" same <(clones py) <(clones rs)
both project add git https://dev.azure.com/acme/Platform/_git/api --name azure
both project remove azure --delete-clone
check "--delete-clone deletes it anyway" same <(clones py) <(clones rs)
python=$python_wrapped
export PATH=$saved_path
unset GIT_CONFIG_COUNT

echo "== Herdr workspaces and settings (R10), each side with its own headless Herdr"
# Both sides get the same session with the same workspaces, made in the same
# order, so Herdr gives them the same ids. Their daemons run for a few passes
# at a time (the first pass over a session only adds), one side after the other.
session=task-workspaces
sock() { echo "$work/$1/config/herdr/sessions/$session/herdr.sock"; }
call() { printf '%s\n' "$2" | socat -t 5 - "UNIX-CONNECT:$(sock "$1")"; }
open_ws() { # open_ws <folder> <label or ""> <focus>: on both sides
  local label="" name; [ -n "$2" ] && label=",\"label\":\"$2\""
  for name in py rs; do
    call $name "{\"id\":\"w\",\"method\":\"workspace.create\",\"params\":{\"cwd\":\"$1\"$label,\"focus\":$3}}" >/dev/null
  done
}
close_ws() { local name; for name in py rs; do call $name "{\"id\":\"c\",\"method\":\"workspace.close\",\"params\":{\"workspace_id\":\"$1\"}}" >/dev/null; done; }
focus_ws() { local name; for name in py rs; do call $name "{\"id\":\"f\",\"method\":\"workspace.focus\",\"params\":{\"workspace_id\":\"$1\"}}" >/dev/null; done; }
daemons() { # daemons <seconds>: each side's daemon, one side after the other
  local name itm pid
  for name in py rs; do
    itm=$python; [ $name = rs ] && itm=$rust
    (exec env XDG_STATE_HOME="$work/$name/state" XDG_DATA_HOME="$work/$name/data" XDG_CONFIG_HOME="$work/$name/config" \
      XDG_RUNTIME_DIR="$work/$name/runtime" "$itm" daemon 2>>"$work/$name-daemon.log") &
    pid=$!
    sleep "$1"
    kill "$pid"; wait "$pid" 2>/dev/null
  done
}
task_has() { # task_has <side> <names...>: the projects of the task Workspaces, in order
  local name=$1 has; shift
  has=$(canon "$(tasks $name)" | python3 -c 'import json,sys; print(" ".join(json.load(sys.stdin)["tasks"]["task-workspaces"].get("projects", [])))')
  [ "$has" = "$*" ] || { echo "    it has: $has"; return 1; }
}
same_state() { # same_state <what>: both tasks.json, ids replaced by names, are the same
  canon "$(tasks py)" >"$work/py.tasks"; canon "$(tasks rs)" >"$work/rs.tasks"
  check "$1 -- same tasks.json" same "$work/py.tasks" "$work/rs.tasks"
}
both new "Workspaces" --no-switch # before its session starts, which would take the name
servers=()
for name in py rs; do
  (exec env XDG_CONFIG_HOME="$work/$name/config" HERDR_SESSION=$session setsid herdr server >"$work/$name-server.log" 2>&1) &
  servers+=($!)
done
stop_servers() {
  local name
  for name in py rs; do side $name herdr session stop $session >/dev/null 2>&1; done
  for pid in "${servers[@]}"; do kill "$pid" 2>/dev/null; wait "$pid" 2>/dev/null; done
}
trap stop_servers EXIT
for name in py rs; do
  for _ in $(seq 50); do [ -S "$(sock $name)" ] && break; sleep 0.1; done
done
check "py: its Herdr session answers" test -S "$(sock py)"
check "rs: its Herdr session answers" test -S "$(sock rs)"
mkdir -p "$HOME/ws/one/src" "$HOME/ws/two" "$HOME/ws/new" "$HOME/ws/cw"
both project add dir '~/ws/one'
both project add dir '~/ws/two'
both project add name "Client W"
both project add-workspace --name outside

open_ws "$HOME/ws/one/src" "" true          # w1: in one
open_ws "$HOME/ws/two" "" false             # w2: in two
open_ws "$HOME/elsewhere" "Client W" false  # w3: named after a project without a folder
daemons 3
same_state "the daemons add the projects the workspaces are in (R10.3)"
check "rs: they are one, two and Client W, in workspace order" task_has rs "<one>" "<two>" "<Client W>"
reads rs list --json
close_ws w2
daemons 4.5
same_state "the daemons remove the project whose workspace closed"
check "rs: two left" task_has rs "<one>" "<Client W>"
IN_SESSION=$session both edit Workspaces --remove-project one
IN_SESSION=$session both project rename "Client W" "Client V"
IN_SESSION=$session both project rename "Client V" "Client W"

echo "-- add-workspace (R10.5), without a terminal"
open_ws "$HOME/ws/new" "" true               # w4: a new folder, focused
IN_SESSION=$session both project add-workspace
IN_SESSION=$session HERDR_ACTIVE_WORKSPACE_ID=w1 both project add-workspace
IN_SESSION=$session HERDR_ACTIVE_WORKSPACE_ID=w3 both project add-workspace
open_ws "$HOME/ws/cw" "Client W" true        # w5: named after it, in a folder it could get
IN_SESSION=$session both project add-workspace
IN_SESSION=$session both project add-workspace --name other
open_ws "$HOME" "" true                      # w6: the home folder
IN_SESSION=$session both project add-workspace --name everything
open_ws "/" "" true                          # w7: above it
IN_SESSION=$session both project add-workspace
focus_ws w4
IN_SESSION=$session HERDR_ACTIVE_WORKSPACE_ID=nope both project add-workspace --name deeper
IN_SESSION=task-gone both project add-workspace --name gone

echo "-- settings (R10.4)"
settings() { # settings <text>: both sides' config.toml
  local name
  for name in py rs; do mkdir -p "$work/$name/config/inbox-task-manager"; printf "$1" >"$work/$name/config/inbox-task-manager/config.toml"; done
}
settings '[herdr]\nprojects_from_workspaces = false\n'
open_ws "$HOME/ws/two" "" false              # w8: two again
IN_SESSION=$session HERDR_ACTIVE_WORKSPACE_ID=w8 both project add-workspace
daemons 3
same_state "off: projects are added by hand and the daemon keeps no record"
settings '[herdr]\nremove_projects_without_workspace = false\n'
daemons 3
close_ws w1
daemons 4.5
same_state "without removal, a closed workspace's project stays"
# one moved last when the edit above kept it (R5.4), in both.
check "rs: one (found in w1) stays" task_has rs "<Client W>" "<one>" "<new>" "<two>"
both project rename "Client W" "Client U"
for text in '[herdr]\nnope = true\n' '[other]\n' '[herdr]\nprojects_from_workspaces = 1\n' 'herdr = 2\n'; do
  settings "$text"
  : >"$work/py-daemon.log"; : >"$work/rs-daemon.log"
  daemons 2.5
  check "the daemons ignore '$(printf "$text" | head -1)' the same way, once" same "$work/py-daemon.log" "$work/rs-daemon.log"
done
check "a broken file is logged once" test "$(grep -c 'settings ignored' "$work/rs-daemon.log")" = 1
settings '[herdr\n'
both project rename "Client U" "Client T"
IN_SESSION=$session HERDR_ACTIVE_WORKSPACE_ID=w5 both project add-workspace
same_state "a broken settings file: nothing follows workspaces"
settings '# kept\n'
stop_servers
trap - EXIT

echo "== each reads the other's state"
for state in py rs; do
  reads $state list
  reads $state list --json
  reads $state list --view archived --json
  reads $state status --json
  reads $state project list --json
done

echo "== an edit of the other's file changes only what it changes"
for writer in py rs; do
  other=rs; [ $writer = rs ] && other=py
  itm_other=$rust; [ $other = py ] && itm_other=$python
  cp "$(tasks $writer)" "$work/before.json"
  side $writer "$itm_other" snooze "Two repos" 1h >/dev/null
  side $writer "$itm_other" wake "Two repos" >/dev/null
  check "a $other snooze and wake leave the $writer file byte for byte" same_bytes "$work/before.json" "$(tasks $writer)"
done

echo "== the old single-project format with a stored kind (N1)"
legacy() { # legacy <side>: a file from before tasks had several projects
  mkdir -p "$work/$1/state/inbox-task-manager"
  cat >"$(tasks "$1")" <<EOF
{
  "deleted": {},
  "projects": {
    "a1b2c3d4": {"added_at": 1700000000, "kind": "dir", "last_used_at": 1700000100, "name": "api", "path": "$HOME/code/api"},
    "b2c3d4e5": {"added_at": 1700000000, "kind": "name", "last_used_at": 1700000100, "name": "Client X"}
  },
  "tasks": {
    "task-old": {"created_at": 1700000000, "last_used_at": 1700000200, "project": "a1b2c3d4", "state": "active", "title": "Old é"},
    "task-none": {"created_at": 1700000000, "last_used_at": 1700000300, "project": "", "state": "active", "title": "None"}
  },
  "version": 1
}
EOF
}
for name in py rs; do
  legacy $name
  itm=$python; [ $name = rs ] && itm=$rust
  side $name "$itm" snooze task-old 1h >/dev/null
  side $name "$itm" wake task-old >/dev/null
done
check "both rewrite an old file the same, byte for byte" same_bytes "$(tasks py)" "$(tasks rs)"
legacy rs
reads rs list --json
reads rs project list --json

echo "== locks"
side rs "$rust" list >/dev/null
lock=$work/rs/state/inbox-task-manager/.tasks.lock
python3 -c 'import fcntl, sys, time; f = open(sys.argv[1], "a"); fcntl.flock(f, fcntl.LOCK_EX); print("held", flush=True); time.sleep(1.5)' "$lock" >"$work/held" &
holder=$!
until grep -q held "$work/held" 2>/dev/null; do sleep 0.05; done
start=$(date +%s%N)
side rs "$rust" new "Waited" --no-switch >/dev/null
waited=$(( ($(date +%s%N) - start) / 1000000 ))
wait $holder
check "a Rust edit waits for a lock Python's way holds (${waited} ms)" test "$waited" -ge 1000

exit $failed
