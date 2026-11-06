# inbox-task-manager: design notes

Read this before changing itm. `REQUIREMENTS.md` lists the requirements and
`README.md` how to install and use itm; this file records the facts, rules
and decisions behind them, so later work does not quietly undo them. When
a change breaks one of these rules on purpose, update this file in the same
commit.

## 1. Herdr facts itm depends on

Verified against Herdr 0.9.3. Re-check them after a Herdr upgrade.

- **Sessions are separate servers.** `herdr session list --json` gives each
  session's `socket_path`. To talk to a session, set `HERDR_SOCKET_PATH` to that
  socket. `HERDR_SESSION` does not override an inherited `HERDR_SOCKET_PATH`.
- **No nested attach, no detach API.** A client inside Herdr cannot attach to
  another session, and nothing in the socket API detaches a client. Sending
  SIGTERM to the `herdr session attach` process detaches it cleanly: it exits
  with 0, restores the terminal and leaves the server running.
- **Popups.** A `[[keys.command]]` with `type = "popup"` runs its command
  through a shell and gets `HERDR_SOCKET_PATH`. It receives all input while
  open, including Escape and the prefix key. Verified keys: Tab, Shift+Tab,
  Ctrl+a/d/e/j/k/n/o/p/r/s/u/z, Alt+1, Alt+2 and Alt+5 (the other digits were
  not sent), F1, `?`, arrows, Escape. (F1 reached the popup in the test
  sandbox but not in every terminal, so help also opens with `?`.) A popup
  belongs to the session it was opened in: it keeps running there after its
  client switches away, and stopping that session kills it.
  Its size is fixed in the config (cells or a percentage). Its command
  inherits the server's environment, `EDITOR` included.
- **Toasts.** `ui.toast.delivery` defaults to `off`. Then
  `herdr notification show` returns `shown: false, reason: "disabled"`. Other
  reasons: `rate_limited`, `no_foreground_client`, `busy`. Herdr itself toasts
  blocked agents in the attached session, except in the active tab.
- **Agent state.** `agent_status` is one of idle, working, blocked, done,
  unknown. `state_change_seq` changes on every transition.
- **Agent resume.** A restored session resumes an agent only if its
  integration reported a session id (`pane.report_agent_session`). Herdr then
  runs the agent's resume command (`claude --resume <id>`). Users install the
  integrations (`herdr integration install <agent>`; README, Setup).
  Codex wants new hooks approved once with `/hooks`.
- **Socket path length.** Unix socket paths are limited to about 108 bytes.
  Herdr puts sockets under `$XDG_CONFIG_HOME/herdr/sessions/<name>/`, so a
  long config directory (deep temp dirs in tests) makes attach hang without an
  error.
- **Headless sessions.** `HERDR_SESSION=<name> herdr server` starts a session
  server without a client.
- **Tabs.** `herdr tab create --cwd <path> --label <name> --focus|--no-focus`
  against a session's socket opens a tab in a running session (in its active
  workspace). The label names the tab, not the workspace.
- **Workspace names.** A workspace keeps `custom_name` and `identity_cwd` in
  the session's `session.json`. Its label is `custom_name` when set, else
  made up from the folder: a workspace created inside a git repository is
  labeled after the repository's top folder (`api` for `api/src`, a linked
  worktree after its own top folder), and once its first pane changed
  folder, after that folder's name (`src`). Either way it is the name of the
  folder or of one above it. The API's `worktree` field is set only for
  workspaces Herdr's own worktree commands made.
  `identity_cwd` follows the working folder of one pane only: the first pane
  (in layout order) of the first tab (in tab order). A `cd` in another tab or
  pane, or focusing another tab, changes nothing; when that pane or tab
  closes, the next first pane takes over; moving a tab to the front makes its
  first pane the one. Herdr keeps tracking `identity_cwd` for a named
  workspace but shows the name. A name comes from `workspace create --label`
  or `workspace rename` (or `prefix+shift+w`) and survives a restart. There
  is no way back to no name: `workspace rename <id> ""` sets an empty label.
- **The workspace's folder over the API.** No call reports `identity_cwd`.
  `herdr api snapshot` has the workspaces, the tabs in order, each tab's
  layout with its panes in layout order, and each pane's `cwd`, which is
  enough to find the first pane of the first tab. `session.json` has it too,
  but is Herdr's internal format and is saved a few seconds late.
- **Building a session before attaching.** `HERDR_SESSION=<name> herdr
  server` starts a session with no workspace; `workspace create --cwd <path>
  --label <name>` then adds them, and `herdr session attach` to that session
  shows them and adds none. `herdr session attach` itself takes one folder
  (its working folder) and no workspace name.
- **CLI arguments.** Options take their value as the next argument, also one
  starting with `-` (`--label -web`); `--opt=value` is refused ("unknown
  option"), and `--` is not special (`workspace rename w1 -- x` names it
  "-- x"). `workspace rename <id> <label>…` joins its words with spaces. A
  `--cwd` that does not exist silently becomes `$HOME`.
- **Popup environment.** A popup command gets the workspace, tab and pane it
  was opened over as `HERDR_ACTIVE_WORKSPACE_ID`, `HERDR_ACTIVE_TAB_ID`,
  `HERDR_ACTIVE_PANE_ID` and `HERDR_ACTIVE_PANE_CWD`, and no
  `HERDR_WORKSPACE_ID` (a pane's own variable).

## 2. Rules that must stay true

### Ownership
- Only sessions named `task-*` are tasks. itm never stops, deletes or adopts
  any other session. It attaches to one only when it is named explicitly, or
  to `default` as the fallback after leaving a task.
- Herdr owns layouts and agents. itm owns task metadata and its own
  bookkeeping:
  - `$XDG_STATE_HOME/inbox-task-manager/tasks.json`: tasks (title, optional
    description and project ids, state, times, workspace bookkeeping), saved
    projects keyed by id, tombstones;
  - `$XDG_CONFIG_HOME/inbox-task-manager/config.toml`: settings (user
    content; itm changes one line at a time);
  - `$XDG_DATA_HOME/inbox-task-manager/projects/<host>/<path>`: clones of
    git projects (`ITM_PROJECTS_DIR` overrides the root);
  - `$XDG_DATA_HOME/inbox-task-manager/tasks/<session>/notes.md`: task
    notes (user content; itm writes the start and appends edits);
  - `$XDG_STATE_HOME/inbox-task-manager/notified.json`: waits already
    reported (daemon only);
  - `<runtime>/status.json`: agent status cache (daemon writes, UI reads);
  - `<runtime>/clients/`: attach-loop markers.
- `<runtime>` (`runtime_dir`) is `$XDG_RUNTIME_DIR/inbox-task-manager`, else
  `/run/user/<uid>/inbox-task-manager` when that folder is ours, else a
  private `/tmp/inbox-task-manager-<uid>`. The daemon (started by a service
  manager) and every client (started from a shell) must find the same
  folder, so the fallbacks do not depend on the environment: not `$TMPDIR`,
  which differs between a macOS login and a launchd agent. The `/tmp` folder
  is created `0700` and refused when someone else owns it or it is a link.

### Switching protocol
- `itm attach` runs `herdr session attach` in a loop. It registers itself as
  `<runtime>/clients/<pid>`, containing the session it is attached to.
- A switch writes `<pid>.next` first, then sends SIGTERM to the attach child.
  The loop reads `.next` after the child exits. Without `.next` the loop ends,
  which is a normal detach.
- SIGHUP (the terminal closed), SIGTERM and SIGINT end the loop like a
  detach (Rust): it detaches its attach child as a hand-off would, follows
  no `.next`, and removes its markers, instead of leaving them until a
  later check finds the pid stale.
- The loop claims `.next` by renaming it before reading it, and never deletes
  `.next` before an attach: a hand-off written while the next attach starts
  must survive until that attach ends. A stale `.next` from a crashed loop
  with the same pid is removed once, at start-up.
- An in-session switch moves only the clients whose marker names the current
  session. If the current session cannot be determined, refuse: otherwise
  every terminal switches. (The desktop notification's Open action is the
  exception: it moves the most recently attached client, whatever its
  session.)
- Validate a marker by its process's command line, since pids are reused.
  A marker whose process is alive but whose command line cannot be read
  (`ps` failed) is neither a client nor stale: it is skipped, not deleted.
  Retry finding the attach child briefly, because the loop may be between
  two attaches. The process table comes from `system.py`: `/proc` on Linux,
  `ps -ww` elsewhere. `ps` joins the arguments with spaces, so there a
  `herdr` path containing a space is not recognized as an attach child.
- Snoozing, archiving or deleting a task moves every itm client on it to the
  next task: one with waiting agents first, then the most recently used, else
  `default`.
- Stopping a session that may host the caller (archive, delete of the current
  task) happens after the hand-off, from a detached process
  (`start_new_session=True`, `/bin/sh`). The Rust itm sends `server.stop`
  over the session's socket instead: Herdr answers before it stops (section
  4, 2026-11-06).
- A popup that moved its own client away exits right away. Otherwise it stays
  open in the old session (see the popup facts above) and is still there when
  you come back.
- Clients started with plain `herdr` cannot be switched. Say so in the error.
- The process name is part of the protocol: `is_itm_process` accepts a
  marker only if an argument of the process's command line contains
  `inbox-task-manager` or `inbox_task_manager`, or is a file named `itm`
  (the console script pip installs: `python …/bin/itm attach`). A new entry
  point or module layout must keep one of these names, or every marker looks
  stale and is deleted.

### Data safety
- Every read-modify-write of `tasks.json` happens under an exclusive `flock`
  with an atomic write (temp file, fsync, rename). Re-resolve the task inside
  the locked edit: it may have been deleted in the meantime.
- Raising inside `Store.edit()` cancels the write. The daemon relies on this.
- Never hold the store lock while waiting for the user (a prompt or a dialog):
  the daemon and every other command would block behind it.
- A deleted task leaves a tombstone until its session is gone, so adoption of
  `task-*` sessions does not bring it back.
- The daemon marks a wait as notified only after the store update succeeded.
  It forgets a wait once the agent is no longer blocked, and keeps the waits
  of sessions it could not poll.
- A missing `herdr` or `git` is an `ItmError` (`needs`), never a traceback:
  pip installs bring neither. Only a missing program of that name is
  caught, so other missing files still surface. `itm attach` checks for
  `herdr` before it saves anything, so a failed attach leaves no task. The
  daemon logs a repeated error once, not every pass. The switcher says
  "Herdr not answering" when the session list fails, instead of showing
  every task as not started.
- Every Herdr query and notification call has a timeout. `herdr session
  attach`, the detached stop/delete and the headless `herdr server` of a
  first start run without one on purpose (the server is detached; waiting
  for it is bounded by `SESSION_START_TIMEOUT`). One failed
  daemon pass must never stop the daemon.
- The status cache is ignored when older than 15 s. A UI must then say the
  daemon is not running instead of showing zero waiting agents.
- Snooze times are in the future and at most 10 years away; `apply_snooze`
  checks this for every caller (a preset can pass while a dialog is open).
- A snooze without a timer ("until an agent needs me") must be able to end:
  it is refused for a new task and for a session that is not running or
  already has an agent waiting, and the daemon wakes such a task on any
  blocked agent, not only a newly reported one, and when its session is no
  longer running.
- A UI refresh that fails for any reason must still end the refresh (the
  switcher's single-load guard would otherwise stop refreshing for good).
- Without a session list (Herdr not answering), nothing is adopted and no
  tombstone expires: every tombstone would look expired and a session being
  deleted would come back.
- Short store reads (`Store.load`) on the UI thread are accepted (the daemon
  holds the lock for milliseconds); writes, Herdr calls and hand-offs run in
  workers.

### Projects
- Projects are keyed by a stable random id; the name is a field. Tasks store
  the id, so `itm project rename` keeps them. Show and match projects by their
  `name` field, never by the key. Commands accept a name (ignoring case) or an
  id. `itm list --json` rows carry the name as `project` and the id as
  `project_id`.
- A task has a list of project ids (`projects`); the first is where its
  session starts. A `tasks.json` with the older single `project` is
  converted when read (`Store._read`), so every reader sees `projects`.
- A task's session gets its project workspaces on the first start only;
  Herdr restores them afterwards. The attach loop asks Herdr whether the
  session exists (`first_start`); when it does not and the task has projects
  with folders, it starts the session headless and opens one workspace per
  project, named after the project, before attaching (`prepare_first_start`,
  `start_with_workspaces`, section 1). This runs under a lock per session in
  `<runtime>/starting/`, checking `first_start` again inside: two clients
  handed to the same new task must not both build it; and a session that
  turns out to have workspaces gets none. The server gets no `HERDR_*`
  variables but `HERDR_SESSION`; one that exits with an error is not waited
  for (`SESSION_START_TIMEOUT` bounds the wait). A failure is printed and the
  attach goes on (Herdr then starts the session as usual). The server runs
  in its own session (setsid), so closing the terminal does not hang it up;
  std can only call setsid in an `unsafe` `pre_exec`, so the Rust itm runs
  itself as the hidden `itm setsid herdr server`, which calls setsid and
  becomes `herdr server`, and reaps it in a thread once it was started. Projects can
  still change at any time (`edit_task`): a session that never started will
  start with them, a running one gets a named workspace per added project
  (`herdr workspace create --cwd --label`, the first focused when it is the
  caller's own task), and a stopped one keeps its workspaces. Edits report
  which of these happened. The edit is saved first; the session list is read
  only afterwards and only when a project was added, so a Herdr failure
  (listing or opening a workspace) becomes a note and never undoes or blocks
  the edit. A missing project folder is reported as such, and gets no
  workspace (Herdr would open it in `$HOME`).
- itm never closes a workspace: it may run agents and programs the user
  started.
- Projects have two sources (REQUIREMENTS.md R10.1): the daemon's workspace
  sync and hand edits (form, CLI). The sync keeps the ids it added in
  `found_projects` and removes only those; a hand edit removes any, and one
  that names a project (even one already there) takes it out of
  `found_projects`, making it the user's. `itm project add-workspace` adds
  its project as found while sync is on (closing that workspace removes it),
  else by hand (`edit_task(found=True)`).
- A project is `{name, url?, path?, added_at, last_used_at}`. Its kind is
  not stored: `project_kind` reads `git` (a URL; its `path` is always itm's
  clone), `dir` (a folder) or `name` (neither). A repository project without
  its clone does not exist, so duplicate repositories are still found by
  clone path, and nothing in the daemon calls `repo_key`. `Store._read`
  drops a stored `kind`; older itm binaries read `project["kind"]` and fail
  on a file this version wrote, so itm is upgraded everywhere at once (the
  daemon restarted), as for the `project` → `projects` change.
- Code never indexes `project["path"]` without knowing there is one: it
  asks `project_folder` (None for a name), `project_where` (URL, else
  folder, else "no folder"), or skips projects without `path`
  (`project_roots`, `task_folders`, `project_change_messages`,
  `deletable_clone`). The TUI keeps `Snapshot.missing`, the ids whose folder
  is set and absent; a name-only project is never "missing" (a map from id
  to "folder exists" read with a default of true once made the preview index
  a name-only project's `path`).
- A folder belongs to at most one project, nesting allowed
  (`check_project_free` on add and on `set`). `set_project_folder` (`itm
  project set`) is for projects without a repository only: a repository's
  folder is its clone, and moving it would orphan the clone. It changes
  nothing on disk.
- A project without a folder gets no workspace (first start, edits: a
  note, not an error) and is found in a workspace only by the workspace's
  name. Renaming it unbinds workspaces still named the old way, so
  `rename_project` says which tasks will lose it.
- The workspace hotkey uses a project without a folder that the workspace
  is named after, or named like (label or folder name: an unnamed
  workspace's label is not a name, so "client-x" would not bind "Client-X"
  otherwise), instead of saving a new one, and offers it the folder. Giving
  a folder changes which workspaces count as the project in every task, so
  it needs a yes in a terminal (`confirm_folder`), never happens without
  one (no terminal, or `--name` given), and is not asked about when it could
  not be given (`folder_problem`: the home folder or above, in another
  project). `give_folder` checks again under the projects lock, also that
  the project still has none. When it cannot be given, the project is still
  named on the workspace and added to the task; in a popup, why is printed
  to stderr and kept on screen 2.5 s, like an error. A project removed while
  the question is open ends the command with an error, not a traceback.
- The project finder's "+ New project" row is the last row and opens the
  add dialog on its Name tab: one Enter on a typo saves nothing. Searching
  never matches "no folder" (display text).
- Workspace sync (`poll_once`, when `projects_from_workspaces` is on): for
  each running task session the daemon reads the workspaces' folders from
  `herdr api snapshot` (`session_workspaces`) and maps each workspace to a
  project (`workspace_project`): a workspace named after a saved project
  (its label is not just its folder's name, which is what an unnamed
  workspace shows) is in that project wherever its first pane went;
  otherwise the innermost saved project holding its folder
  (`folder_project`). Project folders are resolved once per pass
  (`project_roots`), outside the store lock. The projects found are added
  to the task (`sync_task_projects`) and recorded in `workspace_projects`.
  With `remove_projects_without_workspace`, a found project that was in
  that record and is not open now is removed, but only on a trusted pass:
  not when a workspace's folder is unknown, and not for a session the
  daemon did not read on its pass before (`settled`, the daemon's memory:
  a session just started or restored, or the daemon itself just started).
  An untrusted pass only adds and keeps the record, so a workspace that
  closed meanwhile is noticed on the next trusted pass. A session with no
  workspaces at all is skipped. Archived tasks are left alone. The sync
  writes no notes (the daemon never does).
- `workspace_projects` is only true while the daemon reads the session: the
  daemon drops it for a task whose session is not running, and for every
  task while sync is off; archiving drops it at once. Unnamed workspaces
  follow their first pane, as their Herdr name does, so a `cd` there moves
  the workspace to another project (removal included); named workspaces
  do not.
- A workspace's label counts as a name only when it is not the name of its
  folder or of a folder above it (`automatic_label`): Herdr makes up such
  labels for unnamed workspaces, and another repository called `api` must
  not join the project `api`.
- A workspace whose folder is in no saved project can still be in one by
  its git repository (`repository_project`, the daemon and the workspace
  hotkey): a linked worktree is in the project its main work tree is in
  (Herdr's worktrees live under `~/.herdr/worktrees`, outside the
  project), and another checkout is in the one saved project whose
  checkout has the same `origin` remote (normalized as for duplicates,
  R8.4); with two such projects, in neither. `repository` asks git (`rev-parse
  --git-common-dir`, `ls-remote --get-url origin`: no network, a timeout,
  stdin closed) and caches the answer per folder for 5 minutes in the
  process, so the daemon's pass does not run git every 2 s. Missing git
  means no repository.
- Only saved projects are matched. The home folder, where tasks without
  projects start, is in no project unless it is itself saved; a project
  inside it does not claim it (a test keeps this true).
- While sync is on and the daemon runs (a fresh status cache), a hand edit
  keeps a project in `workspace_projects` instead of removing it and says
  so ("could not remove …"); the rest of the edit is saved. The next pass
  would add it back. Closing the workspace is the user's call. The form
  shows this before saving for a running session.
- `itm project add-workspace` refuses the home folder, a folder above it
  and `task_dir()`: saved, such a folder would hold every workspace under
  it and sync would add it to every task. `itm project add dir` still
  saves them. It finds the workspace from `HERDR_ACTIVE_WORKSPACE_ID` (a
  popup), else the focused one, never a pane's `HERDR_WORKSPACE_ID`, which
  stays what it was when the pane moves to another workspace.
- Adding and removing projects hold `.projects.lock` (`itm new` only updates
  a project's `last_used_at`, under the store lock). A clone runs under the
  projects lock but outside the store lock, because it can take minutes and
  the daemon writes the store every 2 s. Checking for duplicates once before
  cloning is then enough.
- Project names are unique ignoring case. A taken name (two remotes both named
  `api`) is refused with the project that has it; the add dialog then keeps
  the user on the name field. The fix is `--name` for the new project or a
  rename of the old one. A folder inside the projects folder cannot be added
  as a folder project.
- A clone goes to a hidden folder next to its target and is renamed into
  place. A failed or interrupted clone removes the hidden folder and the
  empty host/owner folders it created. `exit_on_signals` turns SIGHUP (popup
  closed), SIGTERM and SIGINT into a clean exit for this, and ignores further
  signals while cleaning up. Signals already ignored when the clone starts
  (`nohup`) stay ignored.
- Python installs signal handlers only in the main thread. A TUI must not
  call `clone_repo` from a worker thread; it runs
  `inbox-task-manager project add …` as a child process, which keeps the
  cleanup above. That child:
  - gets `git clone --progress` (git prints no progress when its output is
    captured);
  - must not prompt: `stdin` closed, `GIT_TERMINAL_PROMPT=0`, ssh in batch
    mode, its own session (no controlling terminal), so a password or
    host-key prompt cannot draw over the TUI or hang; such a clone fails with
    a message to run `itm project add` in a terminal;
  - runs without `ITM_POPUP` (which would delay every error by 2.5 s);
  - is started with SIGTERM not ignored, since an already ignored signal
    stays ignored during the clone and cancel would then not stop it;
  - is stopped with SIGTERM on cancel and whenever the TUI exits, because in
    its own session it no longer gets the popup's SIGHUP.
- Removing a project is refused while any task has it, archived ones
  included. Only a real checkout of the project's repository inside the
  projects folder, holding no other project, is ever deleted. A clone with
  local work is kept unless deletion is asked for explicitly
  (`--delete-clone`). Local work means uncommitted or untracked files (even
  with `status.showUntrackedFiles=no`), commits on no remote (branches, tags
  or a detached HEAD), or stashes; a UI dialog only ever offers to
  keep such a clone. Deleting a task never touches its project. A UI that
  checks the clone to build its dialog checks again under the projects lock
  before deleting.
- Duplicates are found by resolved path for folders and by normalized URL for
  repositories (REQUIREMENTS.md R8.4 lists which URL forms count as the same).

### Notes
- Notes are keyed by session name, which a task keeps for good. Deleting a
  task deletes `notes.md` (and the folder when nothing else is in it), because
  a later task can get the same session name once the tombstone expired.
  Snooze, archive and reopen never touch them.
- `notes_path` refuses names that are not one safe path segment (`/`, `.`,
  `..`, NUL): adopted `task-*` sessions are named by whoever made them.
- itm writes notes only after the store write succeeded, and never under the
  store lock. A failure to write them never fails or undoes the task change:
  creating ignores it (the notes start later), an edit reports it.
- Notes are started by writing a temp file in full and hard-linking it into
  place (`os.link` fails if notes exist), so racing writers start them once,
  existing notes are never overwritten, and an append never lands before the
  header. Without hard links (some FUSE or network filesystems) it falls
  back to an exclusive create (`open(path, "x")`). An edit of a task
  without notes starts them from the task as it was before the edit, then
  appends. Appends are one write each in append mode.
- Only real changes are appended (stripped values compared): title,
  description and project. The daemon never writes notes.
- An editor that has the file open and saves after itm appended drops that
  entry (nvim warns that the file changed). Accepted.

### Settings
- `config.toml` is read on every use (`load_config`): the daemon every pass,
  so a toggle in the switcher applies without a restart. A missing file is
  the defaults. Unknown settings and values that are not booleans are
  errors, so a typo does not silently do nothing.
- `set_config` changes one `key = value` line in its section (or adds it),
  keeps everything else, and checks that the result parses with the new
  value before writing (atomically). A file it cannot follow (dotted keys,
  inline tables) is refused with "edit it by hand" and left unchanged.
- A broken file (also one that is not UTF-8) never stops the daemon: it
  logs it once and runs without workspace sync (the safe choice: nothing is
  added or removed) until the file is fixed. Everything else treats it as
  off too (`follows_workspaces`); the Settings screen and the edit form show
  the error.
- `set_config` holds `.config.lock` next to the file, writes through a
  symlink to its target (home-manager, stow), and turns a failed write into
  an `ItmError`; the Settings screen saves one toggle at a time and shows
  any failure.

### Notifications
- Herdr's own toasts cover the attached session; the daemon notifies only for
  other sessions. Delivery order: Herdr toast in an attached session, else a
  desktop notification (`system.notify_command`): `$ITM_NOTIFY_COMMAND` with
  the title and body as arguments, else `osascript` on macOS (the text goes
  in as arguments, never into the script), else `notify-send`. Only
  `notify-send` offers the Open action. With none of them the daemon logs
  the notification and carries on.
- A blocked agent in a snoozed task notifies and wakes the task.

## 3. UX principles

The switcher and its dialogs follow these principles.

1. **Inbox first.** Tasks waiting for you come first, with what they wait for.
2. **Views are visible.** Inbox, Snoozed, Archived and All are always shown
   with counts. Changing view keeps the search text.
3. **Name the action for the state.** Enter means open, wake and open,
   reopen, or create, and the UI says which.
4. **Human times.** "in 1h 30m", "2d ago", "tomorrow 09:00". Never a
   truncated timestamp.
5. **Show the next step.** At most seven keys in the footer, the rest behind
   F1 or `?`. Key hints are never cut off.
6. **Creating is discoverable** from the search box (placeholder text and a
   "Create task" row when nothing matches).
7. **One frame.** Herdr's popup border is the only outer border. Size the
   popup to its content.
8. **Every notification names a way to act on it.**
9. **Presets before free text** for times, sorted by time, without past
   entries. Free text keeps working.
10. **Destructive actions confirm** and say what will be lost.
11. **Errors stay visible.** Popups close when their command exits, so show
    errors inside the UI instead of printing and exiting.
12. **A task's context travels with it.** Wherever a task is shown, its
    project is shown too; the preview adds the description.
13. **One form, fast path first.** Creating a task is one form whose first
    field is enough: Enter creates with defaults, the other fields are a Tab
    away. No chain of prompts.

## 4. Decisions

### 2026-11-04: Textual for the switcher, not fzf
- **Why.** fzf cannot draw section headers, dialogs or inline confirmations,
  truncates labels, and restarts (flickers) on every view change.
- **Choice.** Textual: same language as itm, packaged in nixpkgs (8.2.6),
  headless tests with `App.run_test()`.
- **Measured in a prototype** (Herdr 0.9.3, warm cache, sample data):
  - Python start plus Textual import: 141 ms (Python alone: 12 ms).
  - 209–240 ms from the popup command starting to Textual's first refresh
    (5 runs, then one with the projects screens added). This excludes Herdr
    spawning the popup and used sample data; the real data source,
    `herdr session list --json`, takes about 2 ms.
  - All keys listed in section 1 arrive.
  - The closure grows by about 27 MiB.
- **Rules for the TUI:**
  - Only the TUI imports Textual, lazily. The CLI and the daemon stay
    standard-library only, so they start in milliseconds and the daemon stays
    small.
  - Keys that act on the task list must be inactive whenever a dialog or
    another screen is on top: put them on the main screen, or disable them
    in `check_action`. Enter, Ctrl+A or arrows in a dialog once opened or
    archived the task behind it. Test every dialog and screen with
    `App.run_test()`.
  - Calls that can block (Herdr calls, the store lock, `handoff`, which waits
    up to 2 s) run in a worker, never on the event loop.
  - A periodic refresh keeps the highlighted task by its id, and writes the
    store only when there is a session to adopt or a tombstone to expire.
  - Switching happens after the app has exited (`App.exit(("switch", name))`
    and the CLI calls `switch_to`), so the attach loop gets the terminal back
    and a hand-off never runs on the event loop.
  - Actions that move the TUI's own client away (snooze, archive, delete of
    the current task) exit the app afterwards.
  - Ctrl+A is both archive and a common Herdr prefix (`prefix = "ctrl+a"`;
    Herdr's default is Ctrl+B), so archiving a task whose session runs asks
    first; a stray key after the prefix does nothing.
  - A clone runs as an asyncio child process in its own session; the app
    resets an ignored SIGTERM before starting (an ignored signal stays ignored
    during a clone), stops children on Esc, on exit, on SIGHUP and at exit.
  - User text reaches markup only through `$variables` (titles may contain
    `[`).
  - Textual drops any binding for a key the focused `Input` would type. A
    printable key (like `?` for help) works only if the input declines it
    (`SearchInput.check_consume_key`), which it does only while empty.
  - Folder listings for completion (`FolderLister`) are read in daemon
    threads, once per folder while a dialog is open, and filtered in memory
    on every key. Not `asyncio.to_thread` or thread workers: a scan blocked
    on NFS, autofs or a cold /nix/store would hold asyncio's executor (store
    loads and core calls stop) and `asyncio.run` waits for it when the app
    exits, so Esc or a switch would hang. A listing that arrives late
    triggers a rebuild from what is typed then, or nothing once the dialog
    closed. While a folder is being read the list is empty, so Enter and Tab
    never act on rows left from earlier text. Symlinked folders are resolved
    in the reading thread, so a saved folder reached through a link is found.
  - Fuzzy matching is `core.fuzzy_match` (standard library, linear time),
    for projects and folders alike. Textual's `FuzzySearch` takes seconds on
    long names with repeated letters, and `Matcher.highlight` parses the
    candidate as markup.
  - Ghost text (`Input.suggester`) must start with exactly what was typed:
    Input draws `suggestion[len(value):]` and → replaces the value with it.
    The default `Suggester` casefolds and caches; the folder suggester does
    neither, offers nothing after a trailing "/" (Tab would otherwise keep
    going down into the first subfolder), and checks again after waiting for a
    listing. A kind switch clears the ghost by posting `SuggestionReady`
    with an empty suggestion (Textual 8.2.6).
  - An input opened by a typed key needs `select_on_focus=False`, or the next
    key replaces the first. Keys that reach the project field before its
    picker is up go into that picker (`ProjectField.picker`), never into a
    second one; a paste opens it too.
  - A screen callback must not return `dismiss()`'s awaitable (a `lambda`
    that does): Textual awaits what a callback returns, and awaiting a
    dismiss from that screen's own handler raises `ScreenError`.
  - The editor for notes (Ctrl+O) is the one blocking call on the event
    loop: the app is suspended (`App.suspend()`) and draws nothing while the
    editor owns the terminal. No lock is held around it. `App.suspend` does
    not resume when an exception leaves its block, so everything inside is
    caught, `KeyboardInterrupt` too (Ctrl+C reaches itm when the editor,
    like `code -w`, leaves the terminal alone); the headless test driver
    raises `SuspendNotSupported`. Reading the end of a notes file for the
    preview on the UI thread is accepted, like `Store.load`.
- **Layout as built.** Core, CLI and daemon stay in one module,
  `inbox_task_manager/__init__.py`; the screens are
  `inbox_task_manager/tui.py`, and operating-system differences
  `inbox_task_manager/system.py` (see the portability decision below).
  Splitting core further was considered, but the tests patch functions on
  the core module, and a split would make those patches miss callers in
  other modules.
  The TUI calls core only as `core.<name>` for the same reason. The lazy
  import helper is `screens()`: a function named `tui` would shadow the
  submodule in `from inbox_task_manager import tui`.
- **Popup sizes** (`contrib/herdr-keys.toml`): switcher 80% × 80%, task form 70% × 26
  rows, snooze 60% × 18, notes 80% × 80%, workspace as project 70% × 18 (the
  add dialog), jump and last 60% × 4 (only an error is ever shown).
  Dialogs are at most 100% of the popup, so a narrow terminal shrinks them.

### 2026-11-05: task notes, project finder, folder completion
- **Notes.** One markdown file per task in itm's data folder, keyed by
  session name, so it survives archive and reopen. itm starts it from the
  title and description and appends title, description and project
  changes; nothing else is logged. Opened with the editor from the CLI
  (`itm notes`), a Herdr popup (`prefix+shift+e`; `prefix+n`,
  `prefix+shift+n` and `prefix+e` are Herdr's) and the switcher (Ctrl+O,
  suspending the app). `itm notes --append` lets an agent in the task add
  to it without an editor. A popup rather than `type = "pane"`: every itm
  key is a popup, and how Herdr restores a temporary pane is unverified.
- **Project finder.** The form's project `Select` became a field that opens
  a fuzzy finder: typing narrows the saved projects; a typed path switches
  to folder completion and adds the folder; a typed URL clones or picks the
  saved repository. Adding goes through the existing add dialog, prefilled,
  so names and duplicates are checked in one place.
- **Not changed:** the Projects screen has no search and `@name` needs the
  whole name; the CLI's `--project` takes a name or id, not a fuzzy query.

### 2026-11-05: portability beyond Linux
- **Why.** itm first assumed Linux and Nix: `/proc`, `notify-send`, GNU
  `date -d`, `bash`, and a Nix wrapper that put every tool on `PATH`.
- **`system.py`** holds what differs between operating systems: the process
  table (`process_argv`, `child_processes`), the runtime folder, the
  notification command and finding a `date` that reads `-d`. Standard
  library only. `ps` gets one `-o` per column: FreeBSD reads everything
  after the first `=` of an `-o` list as the header. The core keeps its
  functions (`is_itm_process`, `attach_children`, `runtime_dir`,
  `desktop_notify`), so tests that patch them still reach every caller; only
  the OS calls moved.
- **Snooze phrases.** `_day_and_clock` reads clock times (`14:30`, `5pm`),
  `today`, `tomorrow`, weekdays, `next <weekday>` and ISO dates the way GNU
  `date -d` does (a weekday is today when it matches; a day without a clock
  starts at midnight, except `tomorrow`, which keeps the time of day). A
  clock alone rolls over to tomorrow once it passed, as `14:30` always did.
  Anything else still goes to a `date` that reads `-d` (GNU's, `gdate` on
  macOS, or uutils'), found by trying a relative phrase
  (`1970-01-01 +1 day`) rather than by its version text: BusyBox `date`
  reads only absolute times.
- **Packaging.** `pyproject.toml` installs `inbox-task-manager` and `itm`
  with pip, pipx or uv. The flake has the package (`nix/package.nix`), an
  overlay, a home-manager module (`nix/home-module.nix`: the package and the
  daemon as a systemd or launchd user service) and checks that run the tests.
  Service managers do not read the shell profile, so the module gives the
  daemon a `PATH` that starts with the user's profile (their herdr wins) and
  the `XDG_CONFIG_HOME` (Herdr's sockets), `XDG_STATE_HOME` and
  `XDG_DATA_HOME` home-manager knows, so it sees the same tasks and sessions
  as a shell.
  Herdr's key bindings are not part of the module: users keep their own Herdr
  config and copy `contrib/herdr-keys.toml`.

### 2026-11-05: several projects per task, Herdr workspaces as a source
- **Why.** A task often spans repositories, and the user works in a Herdr
  workspace per project; Herdr renames a workspace without a name whenever
  its first pane changes folder (section 1), which itm's first workspace
  always was (`herdr session attach` cannot name it).
- **Choices.** Tasks hold a list of projects; the first is where the session
  starts. A session's first start builds one named workspace per project
  (headless start, then attach). Adding a project to a running task opens a
  named workspace instead of the tab it opened before. The daemon follows
  workspaces (on by default, a setting) by their folder, the one Herdr names
  them after, not by the focused pane or every pane: a tab or pane opened to
  look at another repository does not make it part of the task. A workspace
  named after a project stays with it (itm names the ones it opens), so a
  `cd` in its first pane does not move it. A workspace in a linked worktree
  or another checkout of a saved project's repository is in that project.
  Projects have sources and the sync removes only what it found. Settings went into a TOML file with a
  screen in the switcher, which writes it line by line. `prefix+shift+a`
  turns the workspace you are in into a project of the current task in one
  step.
- **Reviewed** (design, core and TUI reviews before merging). Changed after
  them: `workspace_projects` dropped for sessions that stopped and while
  sync is off, and refusals need a running daemon (a stale record blocked
  removals for good); untrusted passes only add; first starts locked per
  session; refused removals keep the rest of an edit; a settings file that
  is not UTF-8 no longer stops the daemon; the hotkey refuses the home
  folder.
- **Considered.** Reading `identity_cwd` from `session.json` (exact, but an
  internal format saved seconds late); matching every workspace by label
  (an unnamed workspace's label is a folder name and can equal another
  project's name: only a label that is not the folder's name counts);
  closing a workspace when its project is removed (it may run agents); a
  removal delay (debounce) for unnamed workspaces whose first pane wanders
  (their Herdr name wanders too; named ones are stable).
- **Not changed:** the form adds projects in order (removing and adding
  changes which is first, and the order is kept); renaming a project leaves
  workspace names alone (such a workspace then goes by its folder again).

### 2026-11-05: a project's folder is optional
- **Why.** A project is what a task is about ("Client X"); a folder is only
  where its workspace opens. Projects that are only a name come first and
  get a folder later.
- **Choices.** A project is a name with an optional folder and, for a
  repository, the URL of its clone; the stored `kind` went (read off the
  fields). One folder per project: several folders were considered and left
  out, since a folder belongs to one project and worktrees and other
  checkouts are matched at runtime (by main work tree and by `origin`). No
  project source is stored: nothing would read it (deleting a clone checks
  the folder; the hotkey is the user creating a project). `itm project add
  name`, `itm project set --dir/--no-dir`, a Name tab in the add dialog, a
  "+ New project" row, and the hotkey giving a name its workspace's folder
  after a yes.
- **Reviewed** (a design review before building). Changed after it: no
  repository project without its clone (orphaned clones, no way to clone
  again, `repo_key` in the daemon); `set` only for projects without a
  repository; the hotkey's folder offer needs a yes and refuses the home
  folder and folders of other projects; the TUI's folder map became a
  "missing" set (a name-only project crashed the preview); "+ New project"
  never saves on one Enter. A code review after building changed: a blank
  `add name` is refused (it saved "project"); no question for a folder that
  cannot be given or with `--name`; a project removed during the question;
  plain ←/→ in the add dialog stop at the ends (← in an empty git source
  jumped to "Name only") and no kind change during a clone; the rename note
  names only tasks sync can drop it from.
- **Not done:** a Projects-screen action for `set` (the screen says how);
  `set --git` (a name cannot become a repository); several folders.

### 2026-11-06: itm in Rust, next to the Python itm until it reaches parity
- **Why.** A Go-versus-Rust standoff (two MVPs of the core, three reviews)
  chose Rust: the switcher draws its first frame in about 2 ms instead of
  175 ms, `itm list` and `itm status` take 1 ms instead of 50 ms, the daemon
  uses 0.03% of a core and 4 MB instead of 0.8% and 24 MB, and the domain is
  typed (task states, snooze until a time or an agent, optional folders), so
  the optional-folder and project rules above become compile errors instead
  of review notes.
- **How.** An incremental rewrite. The Rust crate lives at the repository
  root next to the Python package; the Python itm stays the behavior
  reference (its tests are the most precise spec) until a cutover. Both read
  and write the same files byte for byte, so either can run on the other's
  state; `scripts/cross-check.sh` checks it. `PARITY.md` lists every
  requirement and every rule of section 2 with its status in Rust, where it
  lives and the test that pins it.
- **Herdr over its socket API.** Every query and change goes to the
  session's socket (one JSON line each way per connection, documented with a
  JSON Schema by `herdr api schema`), 10–20 times faster than the CLI and
  not refused after a Herdr upgrade the way the CLI's protocol check is.
  Sessions are found as `herdr session list` finds them, from Herdr's
  sessions folder. Only `herdr session attach` and `herdr server` run the
  `herdr` program. A connection gets 1 s and an answer 10 s; the connect is
  non-blocking, because a blocking connect to a session that stopped
  accepting (a deadlock, SIGSTOP) waits forever once its backlog is full,
  which hung the daemon in the standoff. A session whose socket exists but
  does not take a connection in time counts as running (stuck), so its
  requests time out instead of it looking stopped, which would wake tasks
  snoozed until an agent asks.
- **Stopping a session** is a `server.stop` request, not a detached
  `/bin/sh herdr session stop`: Herdr answers before it shuts down, so the
  caller may run inside the session it stops (archiving the current task).
- **Errors** are an enum (`error.rs`) whose messages are the Python itm's.
  Its variants are the errors callers tell apart (which Herdr failure, a
  missing task or project, a folder already saved versus a name taken,
  R8.4 and R8.5); everything else is a sentence written where it happens.
- **Dependencies**, each pinned exactly (`=x.y.z`, released at least two
  weeks before it was added):
  - clap: arguments and help, with subcommands, aliases and repeated
    options, like argparse;
  - serde, serde_json: typed, tolerant reading of `tasks.json`, the status
    cache, `notified.json` and Herdr's answers (unknown fields kept through
    `flatten`), and Python's exact JSON layout on write;
  - jiff: local times for snooze phrases and presets; DST gaps and folds
    resolve like Python's `mktime`;
  - regex-lite: the snooze grammar, Python's two regexes verbatim;
  - ratatui (crossterm backend, default features off) and tui-input: the
    switcher's drawing and its line editing. ratatui has no forms or focus,
    so those are built in itm;
  - rustix: the safe system calls std lacks (`getuid`, `kill` with a
    signal, `setsid`, a non-blocking `connect` with `poll`); `unsafe` code
    is denied in the crate;
  - signal-hook: the attach loop's SIGHUP, SIGTERM and SIGINT, received on
    a thread (already in the graph through crossterm);
  - tempfile: atomic writes and test folders;
  - unicode-normalization: `slugify` like Python's `unicodedata` (NFKD, then
    drop combining marks);
  - shlex: `$ITM_NOTIFY_COMMAND` and `$EDITOR` split like `shlex.split`;
  - insta (tests only): screen snapshots on ratatui's `TestBackend`.
- **The switcher in Rust** keeps the Textual rules' intent in its own terms:
  - The app (`tui/app.rs`) holds state and turns keys into actions with no
    I/O and no clock; the event loop (`tui/mod.rs`) says what time it is,
    draws, and runs anything that can block (Herdr, the store lock, a
    hand-off) in a worker thread.
  - Screens over the list (dialogs now; the form, Projects, Settings and
    help later) are a stack (`tui/screen.rs`): only the top one gets keys,
    so list keys never act on the task behind a dialog, and Esc closes only
    the top. A new screen is a new `Screen` variant.
  - The loop waits on one channel: keys from a reader thread, results from
    workers; the 2 s refresh and a message's expiry are deadlines of that
    wait, so nothing polls. One load runs at a time, and a job's result
    asks for a load. A worker that panics still sends a result, so a
    refresh always ends.
  - The key reader stops before the terminal is given back: a reader still
    blocked on the terminal would take keys meant for the
    `herdr session attach` that follows a switch.
  - Time, input and work are injected (`Clock`, `Work`, the backend), so
    tests run the loop itself on ratatui's `TestBackend` with a scripted
    clock.
  - `itm snooze` without a time (R4.1) runs the snooze screen alone; Esc
    is `cancelled` (exit 1, and no delay in a popup).
- **Other systems.** `system.rs` keeps `system.py`'s split: the process
  table from `/proc`, else `ps -ww` with one `-o` per column (bounded by a
  5 s timeout), and notifications through `osascript` on macOS (title and
  body as arguments, no Open action), else `notify-send`. Which one is
  chosen at run time, not at compile time, so the `ps` reader and the
  macOS notifier are built and tested on Linux too (captured macOS and
  FreeBSD `ps` output, and Linux's own `ps`). The non-blocking socket
  needs a macOS variant (no `SOCK_NONBLOCK` there); it was compiled on
  Linux by swapping the `cfg`, but no Rust build has run on macOS yet.
- **Layout.** One crate, binary `itm` (the process-name rule): `cli.rs`
  (arguments and output), `itm.rs` (the `Itm` context: folders, Herdr, the
  process table and the time zone of one run, and the task operations the
  CLI, switcher and daemon share), `store.rs`, `tasks.rs`, `snooze.rs`,
  `herdr.rs`, `switching.rs`, `daemon.rs` (`observe` asks Herdr,
  `poll_once` decides and records), `status.rs`, `projects.rs`, `notes.rs`,
  `system.rs` (what differs between operating systems), `paths.rs`,
  `times.rs`, `error.rs` and `tui/` (`app.rs` state and keys without I/O,
  `screen.rs` the screens over the list, `draw.rs` rendering, `mod.rs` the
  event loop and its workers). Tests sit
  next to the code; `tests/switching.rs` runs the switching protocol with
  real processes and signals.

## 5. Known gaps

- **Agent titles.** The text shown for an agent is its terminal title (for
  Claude Code, the conversation's topic), not the question it asks.
- **Open action.** The desktop notification's Open action moves the most
  recently attached client, whichever session it is in.
- **Short terminals.** The fixed 26-row form popup has not been tried on a
  terminal shorter than that.
- **Workspace folder after a pane swap.** `session_workspaces` takes the
  first pane in layout order; whether Herdr's identity pane follows a
  `pane swap` (position) or keeps the pane (its root) was not checked.
- **First start, end to end.** The headless start with several named
  workspaces was checked by hand against Herdr 0.9.3 (attach adds none); the
  attach loop's use of it is covered by unit tests only. While it runs (up
  to `SESSION_START_TIMEOUT`), the loop's marker already names the new task
  but no attach child exists, so a hand-off in that window (which waits
  2 s) is dropped.
- **Repositories by remote** compare `origin` only; a checkout whose
  remote has another name, or whose remote changed in the last 5 minutes
  (the cache), is not matched by remote.
- **Settings files** are rewritten with LF line endings.
- **Alt+3…9.** Alt+1, Alt+2 and Alt+5 were sent to a Herdr popup; the others
  are assumed to arrive the same way.
- **macOS and the BSDs.** The `ps` paths, `osascript` and the launchd agent
  are covered by unit tests and CI, not by end-to-end runs with Herdr. The
  BSDs are untested. The Rust itm has not been built on macOS at all.
- **Open action on macOS.** `osascript` notifications cannot run an action,
  so the Open action exists only with `notify-send`.
- **Socket path length on macOS.** Herdr puts a session's sockets in
  `<config>/herdr/sessions/<name>/` (longest: `herdr-client.sock`), and
  macOS allows 104 bytes (Linux 108). Session names reach 46 characters
  (`task-` plus a 40-character slug plus a suffix), so a long title with a
  home folder longer than about `/Users/` plus 8 characters can make attach
  hang (section 1). Shorter session names, or a budget computed from the
  config folder, would fix it.

## 6. Testing

- Tests: `python3 -m unittest discover -s tests` (with Textual installed, or
  in `nix develop`); the Nix build runs them too (`installCheckPhase`), and
  CI runs them on Linux and macOS. Fixtures resolve their temp folder:
  macOS temp folders sit behind a symlink (`/private`) and itm stores
  resolved paths.
  `test_inbox_task_manager.py` for core, CLI and daemon, `test_notes.py` for
  notes and folder completion, `test_system.py` for the OS layer (both
  process-table readers run where they exist: `/proc`, and `ps` when it
  takes BSD-style options, which BusyBox's does not),
  missing programs and the snooze phrases itm reads itself,
  `test_tui.py` for the screens with `App.run_test()` against a temporary
  home. Every fixture sets `XDG_CONFIG_HOME` to its temporary folder too, so
  no test reads or writes the real `config.toml`. Clones are tested with a fake child (`self_command` patched to a small
  script), including cancel and a failure that needs a password.
- In `buildPythonApplication`, `doCheck = true` is what runs
  `installCheckPhase`; `doInstallCheck` alone is overridden.
- Manual end-to-end checks run in a sandbox, never against real sessions:
  - separate `XDG_CONFIG_HOME` (short path: symlink it under
    `$XDG_RUNTIME_DIR`, see the socket path limit), `XDG_STATE_HOME` and
    `XDG_RUNTIME_DIR`;
  - tmux as a scriptable terminal: `send-keys`, `capture-pane -e` (keeps
    colors);
  - wait for a popup to render before sending keys to it;
  - start that tmux with `-f /dev/null`, an explicit `-S` socket and a clean
    environment (`env -i`): your own tmux config may restore saved sessions
    into the new server (tmux-resurrect), and `HERDR_*` variables from a
    surrounding Herdr make the sandbox's Herdr refuse to start ("nested herdr
    is disabled").
- A blocked agent without spending tokens: start `claude` in a folder it does
  not trust yet; the trust prompt counts as blocked. A resume test needs one
  short prompt (`claude --model haiku`) in a trusted folder.
- Known pitfalls: `tmux send-keys M-Enter` arrives as Escape then Enter.
  `pgrep -f <pattern>` also matches the shell running it.
- Non-trivial changes get a review before they are merged.
- **The Rust itm.** `cargo fmt --check`, `cargo clippy --all-targets -- -D
  warnings` and `cargo test` must pass at every commit. Unit tests sit at the
  end of each module (`#[cfg(test)] mod tests`) and need no Herdr: a fake
  session is a unix socket answering canned JSON (`herdr::fake`), the
  process table a `FakeProcs`, the folders a temporary root (`Itm::under`).
  Tests never change the test process's environment (tests run in parallel
  threads): what varies is injected. Screens are drawn on ratatui's
  `TestBackend`, some pinned as insta snapshots in `src/tui/snapshots/`
  (`INSTA_UPDATE=always cargo test` rewrites them after an intended change;
  review the diff). A test that pins a requirement names it (`R4.1` in its
  name or a comment).
- `tests/switching.rs` checks the switching protocol with real processes
  and signals: the built `itm`, and the test binary itself playing `herdr`
  (symlinked under that name: an attached client that a signal detaches,
  or a headless server answering on its socket). It has its own `main`
  (`harness = false` in `Cargo.toml`); its tests run one after another.
- End to end, against a real headless Herdr with tmux as the terminal:
  `scripts/smoke.sh` (fake agents report themselves blocked over the socket
  API, no real agent runs) and `scripts/measure.sh`, both run through
  `scripts/sandbox.sh <name>` (no `HERDR_*` variables, private XDG folders
  under `/tmp/itm-<name>`, notifications to a log).

## 7. Packaging

- itm is a Python package (pyproject, setuptools) with two console scripts,
  `inbox-task-manager` and `itm`; both names keep the attach loop's command
  line recognizable (see the process-name rule in section 2). Only the TUI
  needs a dependency (Textual).
- `ITM_SELF`, when set, is the command child processes (clones) run, so they
  run the same build; the Nix wrapper sets it. Without it they run
  `python -m inbox_task_manager` with the current interpreter.
- The Nix package (`nix/package.nix`, `buildPythonApplication`) puts git,
  `gdate` and, on Linux, `notify-send` first on `PATH`. Herdr goes
  last (`--suffix`): the client must match the server's version, so a
  `herdr` the user already runs wins, and the bundled one serves services
  started without the user's `PATH`. `herdr = null` leaves it out.
- The flake follows nixos-unstable, where Herdr is packaged. It lists no
  x86_64-darwin, which nixpkgs 26.11 dropped; there the overlay and the
  module's default package (`callPackage` with the user's nixpkgs) still
  work. A configuration that installs Herdr from another nixpkgs should make
  the flake's `nixpkgs` input follow that one, so both use the same Herdr.
- New files need `git add` (`-N` is enough) before a flake build can see them.
- The source fileset excludes `__pycache__`.
