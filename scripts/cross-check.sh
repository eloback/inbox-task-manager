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
# Python build at /tmp/itm-standoff-cache/python-ref/bin/itm) choose the
# binaries. Prints PASS/FAIL per check; exits 1 if any failed. Later
# milestones add their commands here.
set -u
root=${ITM_SANDBOX:?run this through scripts/sandbox.sh}
repo=$(cd "$(dirname "$0")/.." && pwd)
rust=${ITM:-$repo/target/release/itm}
python=${ITM_PYTHON:-/tmp/itm-standoff-cache/python-ref/bin/itm}
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
side() { # side <name> <command...>
  local dir=$work/$1; shift
  mkdir -p "$dir/state" "$dir/data" "$dir/config" "$dir/runtime"
  env XDG_STATE_HOME="$dir/state" XDG_DATA_HOME="$dir/data" XDG_CONFIG_HOME="$dir/config" \
    XDG_RUNTIME_DIR="$dir/runtime" "$@"
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

step=0
both() { # both <args...>: the same command on side py (Python) and side rs (Rust)
  step=$((step + 1))
  local name
  for name in py rs; do
    local itm=$python; [ $name = rs ] && itm=$rust
    side $name "$itm" "$@" >"$work/$name.out" 2>"$work/$name.err" </dev/null
    echo "exit $?" >>"$work/$name.out"
    cp "$(tasks $name)" "$work/$name.tasks" 2>/dev/null || : >"$work/$name.tasks"
  done
  check "itm $* -- same output" same "$work/py.out" "$work/rs.out"
  check "itm $* -- same errors" same "$work/py.err" "$work/rs.err"
  check "itm $* -- same tasks.json" same "$work/py.tasks" "$work/rs.tasks"
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
both project add dir '~/code/api'
both project add dir '~/elsewhere/api' --name API
both project list
both project list --json

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
