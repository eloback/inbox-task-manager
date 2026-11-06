#!/usr/bin/env bash
# Speed and footprint, in a sandbox with 5 tasks, 3 of them with running
# headless sessions (one with a blocked fake agent): `itm list` and `itm
# status` (median of 20), the switcher's first frame (ITM_EXIT_AFTER_DRAW=1,
# median of 10 in a 120x40 tmux pane), and the daemon's CPU and RSS over 60 s.
#
#   cargo build --release
#   scripts/sandbox.sh measure scripts/measure.sh; rm -rf /tmp/itm-measure
#
# Also measures the Python itm (ITM_PYTHON) on the same state for reference
# (its switcher has no ITM_EXIT_AFTER_DRAW switch).
set -u
root=${ITM_SANDBOX:?run this through scripts/sandbox.sh}
repo=$(cd "$(dirname "$0")/.." && pwd)
itm=${ITM:-$repo/target/release/itm}
measure="python3 $repo/scripts/measure.py"
python_itm=${ITM_PYTHON:-}
sessions=$XDG_CONFIG_HOME/herdr/sessions
tmux_sock=$root/tmux.sock
call() { printf '%s\n' "$2" | socat -t 5 - "UNIX-CONNECT:$sessions/$1/herdr.sock"; }
cleanup() {
  tmux -S "$tmux_sock" kill-server 2>/dev/null
  for s in task-fix-login task-write-docs task-later; do herdr session stop "$s" >/dev/null 2>&1; done
}
trap cleanup EXIT

"$itm" new "Fix login" -d "Users get logged out" --no-switch >/dev/null
"$itm" new "Write docs" --no-switch >/dev/null
"$itm" new "Old idea" --no-switch >/dev/null
"$itm" new "Later" --snooze 2h >/dev/null
"$itm" new "Shipped" --no-switch >/dev/null && "$itm" archive shipped >/dev/null
for s in task-fix-login task-write-docs task-later; do
  HERDR_SESSION=$s setsid herdr server >/dev/null 2>&1 &
  until [ -S "$sessions/$s/herdr.sock" ]; do sleep 0.05; done
  call "$s" "{\"id\":\"w\",\"method\":\"workspace.create\",\"params\":{\"cwd\":\"$HOME\",\"label\":\"w\",\"focus\":true}}" >/dev/null
done
call task-fix-login '{"id":"s","method":"pane.send_input","params":{"pane_id":"w1:p1","text":"exec sleep 900","keys":["enter"]}}' >/dev/null
sleep 0.5
call task-fix-login '{"id":"r","method":"pane.report_agent","params":{"pane_id":"w1:p1","source":"custom:itmtest","agent":"fakebot","state":"blocked"}}' >/dev/null

echo "rust itm list:   $($measure runs 20 "$itm" list)"
echo "rust itm status: $($measure runs 20 "$itm" status)"
echo "rust daemon:     $($measure daemon 60 "$itm" daemon)"
tmux -S "$tmux_sock" -f /dev/null new-session -d -x 120 -y 40 \
  "ITM_EXIT_AFTER_DRAW=1 $measure tty-runs 10 $itm ui 2> $root/ui.txt"
until [ -s "$root/ui.txt" ]; do sleep 0.2; done
echo "rust switcher first draw (120x40 tmux): $(cat "$root/ui.txt")"
if [ -x "$python_itm" ]; then
  echo "python itm list:   $($measure runs 20 "$python_itm" list)"
  echo "python itm status: $($measure runs 20 "$python_itm" status)"
  echo "python daemon:     $($measure daemon 60 "$python_itm" daemon)"
else
  echo "SKIP python itm (${python_itm:-ITM_PYTHON unset} missing)"
fi
echo "binary: $(stat -c %s "$itm") bytes"
