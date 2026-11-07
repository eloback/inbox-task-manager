#!/usr/bin/env bash
# The README's screenshots (docs/screenshots), taken from the real itm: a
# sandbox with demo tasks and projects, headless Herdr sessions with fake
# agents (as in smoke.sh) and the daemon, the switcher in tmux, each screen
# captured with its colors and photographed by a headless Chromium.
#
#   cargo build --release
#   scripts/sandbox.sh shots scripts/screenshots.sh; rm -rf /tmp/itm-shots
#
# Needs tmux, socat, chromium and ImageMagick's magick. OUT (default
# docs/screenshots) is where the PNGs go.
set -euo pipefail
root=${ITM_SANDBOX:?run this through scripts/sandbox.sh}
repo=$(cd "$(dirname "$0")/.." && pwd)
itm=${ITM:-$repo/target/release/itm}
out=${OUT:-$repo/docs/screenshots}
sessions=$XDG_CONFIG_HOME/herdr/sessions
tmux_sock=$root/tmux.sock
pids=()

call() { printf '%s\n' "$2" | socat -t 5 - "UNIX-CONNECT:$sessions/$1/herdr.sock"; }
field() { python3 -c "import json,sys; print(eval('json.load(sys.stdin)' + sys.argv[1]))" "$1"; }
workspace() { # workspace <session> <label> <cwd>: prints the new workspace's root pane
  call "$1" "{\"id\":\"w\",\"method\":\"workspace.create\",\"params\":{\"cwd\":\"$3\",\"label\":\"$2\",\"focus\":true}}" \
    | field "['result']['root_pane']['pane_id']"
}
agent() { # agent <session> <pane> <name> <state> <title>: a pane running sleep that reports itself as an agent
  call "$1" "{\"id\":\"s\",\"method\":\"pane.send_input\",\"params\":{\"pane_id\":\"$2\",\"text\":\"printf '\\\\033]0;$5\\\\007'; exec sleep 900\",\"keys\":[\"enter\"]}}" >/dev/null
  sleep 0.5
  call "$1" "{\"id\":\"r\",\"method\":\"pane.report_agent\",\"params\":{\"pane_id\":\"$2\",\"source\":\"custom:itmshots\",\"agent\":\"$3\",\"state\":\"$4\"}}" >/dev/null
}
wait_for() { local deadline=$((SECONDS + $1)); shift; until "$@"; do [ $SECONDS -ge $deadline ] && return 1; sleep 0.1; done; }
shows() { tmux -S "$tmux_sock" capture-pane -p -t shots | grep -qF -- "$1"; }
keys() { tmux -S "$tmux_sock" send-keys -t shots "$@"; }
cleanup() {
  for pid in "${pids[@]}"; do kill "$pid" 2>/dev/null; done
  tmux -S "$tmux_sock" kill-server 2>/dev/null
  for dir in "$sessions"/*/; do [ -d "$dir" ] && herdr session stop "$(basename "$dir")" >/dev/null 2>&1; done
}
trap cleanup EXIT

shoot() { # shoot <name> <background>: the pane as docs/screenshots/<name>.png
  sleep 0.6 # a refresh or two
  tmux -S "$tmux_sock" capture-pane -e -p -t shots >"$root/$1.ansi"
  python3 -I "$repo/scripts/ansi-to-html.py" "$root/$1.ansi" "$cols" "$2" >"$root/$1.html"
  chromium --headless --disable-gpu --no-sandbox --hide-scrollbars --force-device-scale-factor=2 \
    --default-background-color=00000000 --window-size=1300,900 \
    --screenshot="$root/$1-full.png" "file://$root/$1.html" >/dev/null 2>&1
  magick "$root/$1-full.png" -trim +repage "$out/$1.png"
  echo "wrote $out/$1.png"
}

mkdir -p "$out" "$XDG_CONFIG_HOME/herdr" "$HOME/code"/{api,web,infra,docs}
for p in api web infra docs; do "$itm" project add dir "$HOME/code/$p" >/dev/null; done

"$itm" new "Fix the login redirect loop" -p api -p web --no-switch \
  -d "Users bounce between /login and /callback once their session expires." >/dev/null
"$itm" new "Migrate CI to Nix flakes" -p infra --no-switch >/dev/null
"$itm" new "Add dark mode to settings" -p web --no-switch >/dev/null
"$itm" new "Rate-limit the public API" -p api --no-switch >/dev/null
"$itm" new "Write the onboarding guide" -p docs --no-switch >/dev/null
"$itm" new "Review PR #482 portability" -p web --snooze 2h >/dev/null
"$itm" new "Upgrade nixpkgs to 26.05" -p infra --snooze 1d >/dev/null
"$itm" new "Investigate flaky e2e tests" -p web --no-switch >/dev/null
"$itm" archive investigate-flaky-e2e-tests >/dev/null
printf 'Repro: log in, wait 5 min, reload /settings. SameSite=Lax is the suspect.\n' \
  | "$itm" notes fix-the-login-redirect-loop --append - >/dev/null
printf 'Agent found the refresh cookie is scoped to /api; asked whether to widen it.\n' \
  | "$itm" notes fix-the-login-redirect-loop --append - >/dev/null

# Sessions with agents: two waiting on you, one working.
for s in task-fix-the-login-redirect-loop task-migrate-ci-to-nix-flakes task-add-dark-mode-to-settings; do
  HERDR_SESSION=$s setsid herdr server >"$root/server-$s.log" 2>&1 &
  wait_for 5 test -S "$sessions/$s/herdr.sock"
done
agent task-fix-the-login-redirect-loop "$(workspace task-fix-the-login-redirect-loop api "$HOME/code/api")" \
  claude blocked "Clear the refresh cookie too?"
workspace task-fix-the-login-redirect-loop web "$HOME/code/web" >/dev/null
agent task-migrate-ci-to-nix-flakes "$(workspace task-migrate-ci-to-nix-flakes infra "$HOME/code/infra")" \
  codex blocked "Pin nixpkgs or follow unstable?"
agent task-add-dark-mode-to-settings "$(workspace task-add-dark-mode-to-settings web "$HOME/code/web")" \
  claude working "Theme tokens for the settings page"
"$itm" daemon 2>"$root/daemon.log" &
pids+=($!)
wait_for 10 sh -c "'$itm' status | grep -q '⚑ 2'"

cols=118 rows=30
open_switcher() { # open_switcher <theme>
  tmux -S "$tmux_sock" kill-server 2>/dev/null || true
  tmux -S "$tmux_sock" -f /dev/null new-session -d -s shots -x $cols -y $rows \
    "env COLORTERM=truecolor ITM_THEME=$1 '$itm' ui; sleep 600"
  wait_for 5 shows 'NEEDS YOU'
}

open_switcher mocha
shoot inbox '#181825'
keys C-n
wait_for 5 shows 'New task'
keys -l "Review the payments PR"
keys Tab
keys -l "Check idempotency keys on retries."
keys Tab
keys -l "api" # the finder, with what is typed
wait_for 5 shows 'complete a folder'
keys Enter
sleep 0.5
if shows 'complete a folder'; then keys Escape; fi
wait_for 5 shows 'Starts in ~/code/api'
shoot new-task '#181825'
keys Escape
wait_for 5 shows 'Discard'
keys y
keys Tab
wait_for 5 shows 'wake & open'
shoot snoozed '#181825'
keys BTab
keys C-p
wait_for 5 shows 'repositories and folders'
shoot projects '#181825'

open_switcher latte
shoot inbox-latte '#eff1f5'
