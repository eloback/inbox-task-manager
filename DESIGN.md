# inbox-task-manager: design notes

Read this before changing itm. `REQUIREMENTS.md` lists the requirements and
`README.md` how to install and use itm; this file records the facts, rules
and decisions behind them, so later work does not quietly undo them. When
a change breaks one of these rules on purpose, update this file in the same
commit.

## 1. Herdr facts itm depends on

Verified against Herdr 0.9.3 (the user's); the smoke run passes against
0.9.5 too, which the flake bundles as a fallback. Re-check them after a
Herdr upgrade.

- **Sessions are separate servers.** `herdr session list --json` gives each
  session's `socket_path`. To talk to a session, set `HERDR_SOCKET_PATH` to that
  socket. `HERDR_SESSION` does not override an inherited `HERDR_SOCKET_PATH`.
- **Session discovery.** No socket method lists sessions: `herdr session
  list` reads `<config>/herdr/sessions/` (`<config>` is `$XDG_CONFIG_HOME`,
  else `~/.config`, on macOS too), whose folder names are 1–64 bytes of
  `[A-Za-z0-9._-]` (not `.`, `..` or `default`), adds the default session
  (`<config>/herdr/herdr.sock`), and calls a session running when a
  connect to its `herdr.sock` succeeds. itm does the same (`Herdr::sessions`).
- **The socket API.** Each session's server answers on its `herdr.sock`
  (mode `0600`; `herdr-client.sock` next to it is the attach protocol): one
  JSON object per line, one request per connection. A request is `{"id":
  "<string>", "method": "<name>", "params": {…}}` (`params` required, even
  empty); the answer is one line, `{"id", "result": {"type", …}}` or `{"id",
  "error": {"code", "message"}}`, and the server closes. Match errors by
  `code`. The server gives each step 5 s; clients keep their own deadline.
  `herdr api schema --json` prints the JSON Schema (protocol 22 from 0.9.0
  to 0.9.5; 0.9.5's schema is the same as 0.9.3's), and the documented
  contract is that clients ignore unknown fields and treat an unsupported
  method as an ordinary error. The CLI pings first and refuses a server
  whose protocol number differs (`protocol_mismatch`, until the server
  restarts after an upgrade); a raw client is not refused. itm uses
  `session.snapshot` (agents, workspaces, tabs, layouts and panes in one
  answer), `workspace.create`, `workspace.list`, `workspace.rename`,
  `notification.show` and `server.stop`; the smoke run also uses
  `pane.send_input`, `pane.report_agent` and `workspace.close`.
- **Stopping and deleting.** `server.stop` answers, then stops the server,
  so the caller may run inside that session. `herdr session delete`
  refuses a running session and has no socket method.
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
  inherits the server's environment, `EDITOR` included. A
  `[[keys.command]]` on `prefix+tab` replaces Herdr's own
  `cycle_pane_next`, and `prefix+shift+<letter>` is the capital letter
  after the prefix (Herdr 0.9.3, checked with each contrib key).
- **Toasts.** `ui.toast.delivery` defaults to `off`. Then
  `notification.show` (`herdr notification show`) returns `shown: false,
  reason: "disabled"`. Other
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
  `session.snapshot` (`herdr api snapshot`) has the workspaces, the tabs in
  order, each tab's layout with its panes in layout order, and each pane's
  `cwd`, which is enough to find the first pane of the first tab. `session.json` has it too,
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
  `HERDR_WORKSPACE_ID` (a pane's own variable). The same in 0.9.5.
- **Dropped keys (0.9.5).** Besides a client just handed to another session
  (section 6), a 0.9.5 client dropped the first keys sent right after a
  popup closed and a new workspace took focus; 0.9.3 did not. 0.9.5 also
  greets a new config with a welcome dialog and its integrations settings.

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
  detach: it detaches its attach child as a hand-off would, follows
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
  two attaches. The process table comes from `system.rs` (`process_table`):
  `/proc` on Linux, `ps -ww` elsewhere. `ps` joins the arguments with spaces, so there a
  `herdr` path containing a space is not recognized as an attach child.
- Snoozing, archiving or deleting a task moves every itm client on it to the
  next task: one with waiting agents first, then the most recently used, else
  `default`.
- Stopping a session that may host the caller (archive, delete of the current
  task) happens after the hand-off, as a `server.stop` request: Herdr
  answers before it stops, so the caller may be inside that session
  (section 1). Deleting then has Herdr delete the session (`herdr session
  delete`, which refuses a running one and has no socket method) from a
  detached `/bin/sh` in a session of its own (`itm setsid`), which tries
  again every 0.5 s until the server is gone (at most 15 s). (The Python
  itm ran `herdr session stop` and `delete` once from a detached `/bin/sh`.)
- A popup that moved its own client away exits right away. Otherwise it stays
  open in the old session (see the popup facts above) and is still there when
  you come back.
- Clients started with plain `herdr` cannot be switched. Say so in the error.
- The process name is part of the protocol: `is_itm_process`
  (`switching.rs`) accepts a marker only if an argument of the process's
  command line contains `inbox-task-manager` or `inbox_task_manager`, or is
  a file named `itm`: the binary (`[[bin]] name = "itm"`; the Nix wrapper
  keeps argv[0], `…/bin/itm`), and the Python itm's console scripts
  (`python …/bin/itm attach`), so an attach loop of the Python itm still
  running during an upgrade is no stale marker. Renaming the binary breaks
  this: every marker would look stale and be deleted.

### Data safety
- Every read-modify-write of `tasks.json` happens under an exclusive `flock`
  with an atomic write (temp file, fsync, rename). Re-resolve the task inside
  the locked edit: it may have been deleted in the meantime.
- An error returned inside `Store::edit` cancels the write. The daemon
  relies on this.
- Never hold the store lock while waiting for the user (a prompt or a dialog):
  the daemon and every other command would block behind it.
- A deleted task leaves a tombstone until its session is gone, so adoption of
  `task-*` sessions does not bring it back.
- The daemon marks a wait as notified only after the store update succeeded.
  It forgets a wait once the agent is no longer blocked, and keeps the waits
  of sessions it could not poll.
- A missing `herdr` or `git` is `Error::Missing` ("herdr not found;
  install it or add it to PATH"), never a panic: `cargo install` brings
  neither. Only a missing program of that name becomes it (`missing_herdr`,
  `missing_git`), so other missing files still surface. `itm attach` checks for
  `herdr` before it saves anything, so a failed attach leaves no task. The
  daemon logs a repeated error once, not every pass. The switcher says
  "Herdr not answering" when the session list fails, instead of showing
  every task as not started.
- Every Herdr query and notification has a timeout: 1 s to connect (a
  non-blocking connect, section 4, 2026-11-06) and 10 s for the answer.
  `herdr session attach`, the detached delete and the headless `herdr
  server` of a first start run without one on purpose (the server is
  detached; waiting for it to answer is bounded by `SERVER_START`, 5 s).
  One failed daemon pass, even one that panics, must never stop the daemon
  (`catch_unwind` around each pass).
- The status cache is ignored when older than 15 s. A UI must then say the
  daemon is not running instead of showing zero waiting agents.
- Snooze times are in the future and at most 10 years away; `Task::snooze`
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
- Short store reads (`Store::load`) on the UI thread are accepted (the
  daemon holds the lock for milliseconds); writes, Herdr calls, git, folder
  listings and hand-offs run in workers. The switcher's refresh runs in a
  worker too.

### Projects
- Projects are keyed by a stable random id; the name is a field. Tasks store
  the id, so `itm project rename` keeps them. Show and match projects by their
  `name` field, never by the key. Commands accept a name (ignoring case) or an
  id. `itm list --json` rows carry the name as `project` and the id as
  `project_id`.
- A task has a list of project ids (`projects`); the first is where its
  session starts. A `tasks.json` with the older single `project` is
  converted when read (`store.rs` `upgrade`), so every reader sees `projects`.
- A task's session gets its project workspaces on the first start only;
  Herdr restores them afterwards. The attach loop asks Herdr whether the
  session exists (`first_start`); when it does not and the task has projects
  with folders, it starts the session headless and opens one workspace per
  project, named after the project, before attaching (`prepare_first_start`,
  section 1). This runs under a lock per session in
  `<runtime>/starting/`, checking `first_start` again inside: two clients
  handed to the same new task must not both build it; and a session that
  turns out to have workspaces gets none. The server gets no `HERDR_*`
  variables but `HERDR_SESSION`; one that exits with an error is not waited
  for (`SERVER_START` bounds the wait). A failure is printed and the
  attach goes on (Herdr then starts the session as usual). The server runs
  in its own session (setsid), so closing the terminal does not hang it up;
  std can only call setsid in an `unsafe` `pre_exec`, so itm runs itself as
  the hidden `itm setsid herdr server`, which calls setsid and becomes
  `herdr server`, and reaps it in a thread once it was started. Projects
  can still change at any time (`edit_task`): a session that never started
  will start with them, a running one gets a named workspace per added
  project (`workspace.create` with `cwd` and `label`, the first focused when
  it is the caller's own task), and a stopped one keeps its workspaces. Edits report
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
  else by hand (`Edit::found`).
- A project is `{name, url?, path?, added_at, last_used_at}` (`store.rs`
  `Project`; fields it does not know are kept). Its kind is not stored:
  `Project::kind` reads `Git` (a URL; its `path` is always itm's clone),
  `Dir` (a folder) or `Name` (neither). A repository project without its
  clone does not exist, so duplicate repositories are still found by clone
  path, and nothing in the daemon calls `repo_key`. `upgrade` drops a
  stored `kind`; itm binaries older than this format read `kind` and fail
  on a file this version wrote, so itm is upgraded everywhere at once (the
  daemon restarted), as for the `project` → `projects` change.
- A project's folder is an `Option` (`Project::folder`), so no code can use
  one without asking whether there is one; `Paths::place` says where a
  project is (URL, else folder, else "no folder"), and `project_roots`,
  `task_folders`, `project_change_messages` and `deletable_clone` skip
  projects without one. The TUI keeps `Snapshot.missing`, the ids whose
  folder is set and absent; a name-only project is never "missing" (in the
  Python itm, a map from id to "folder exists" read with a default of true
  once made the preview index a name-only project's `path`).
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
  the question is open ends the command with an error, not a panic.
- The project finder's "+ New project" row is the last row and opens the
  add dialog on its Name tab: one Enter on a typo saves nothing. Searching
  never matches "no folder" (display text).
- Workspace sync (`poll_once`, when `projects_from_workspaces` is on): for
  each running task session the daemon reads the workspaces' folders from
  one `session.snapshot` (`observe`, with the agents) and maps each
  workspace to a project (`workspace_project`): a workspace named after a
  saved project (its label is not just its folder's name, which is what an
  unnamed workspace shows) is in that project wherever its first pane went;
  otherwise the innermost saved project holding its folder
  (`folder_project`). The matching runs before the store lock is taken,
  with the project folders resolved once per pass (`project_roots`). The
  projects found are added to the task (`Task::sync_projects`) and
  recorded in `workspace_projects`.
  With `remove_projects_without_workspace`, a found project that was in
  that record and is not open now is removed, but only on a trusted pass:
  not when a workspace's folder is unknown, and not for a session the
  daemon did not read on its pass before (`settled`, the daemon's memory:
  a session just started or restored, or the daemon itself just started).
  An untrusted pass only adds and keeps the record, so a workspace that
  closed meanwhile is noticed on the next trusted pass. A session with no
  workspaces at all is skipped. Archived tasks are left alone. The sync
  writes no notes (the daemon never does). When the snapshot fails, neither
  agents nor workspaces are read for that session on that pass.
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
  R8.4); with two such projects, in neither. `Git::repository` asks git
  (`rev-parse --git-common-dir`, `ls-remote --get-url origin`: no network, a
  timeout, stdin closed) and `Repositories` caches the answer per folder
  for 5 minutes in the process, so the daemon's pass does not run git
  every 2 s. Missing git means no repository.
- Only saved projects are matched. The home folder, where tasks without
  projects start, is in no project unless it is itself saved; a project
  inside it does not claim it (a test keeps this true).
- While sync is on and the daemon runs (a fresh status cache), a hand edit
  keeps a project in `workspace_projects` instead of removing it and says
  so ("could not remove …"); the rest of the edit is saved. The next pass
  would add it back. Closing the workspace is the user's call. The form
  shows this before saving for a running session.
- `itm project add-workspace` refuses the home folder, a folder above it
  and the folder tasks without projects start in (`paths.task_dir`,
  `ITM_TASK_DIR`): saved, such a folder would hold every workspace under
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
  empty host/owner folders it created. SIGHUP (popup closed), SIGTERM and
  SIGINT become a clean exit for this: `ExitSignals` (`system.rs`) catches
  them (signal-hook) until the cleanup is done, ignoring further ones
  meanwhile, and then lets them end itm again (signal-hook alone would
  leave them ignored once its last action goes); git gets SIGTERM, then
  SIGKILL after 10 s, and itm exits with 128 + the signal (`Error::Exit`).
  Signals already ignored when the clone starts (`nohup`) stay ignored;
  which ones are is read from `/proc/self/status` (`SigIgn`), else `ps -o
  sigignore=`: no safe call asks for a signal's disposition.
- A TUI does not clone in its own process: the clone must have no
  controlling terminal (a password or host-key prompt would draw over the
  TUI or hang it), and its cleanup on signals belongs to the process that
  made the hidden folder. It runs a child `itm project add git` instead
  (`Itm::clone_child`: `ITM_SELF`, else the running binary, through `itm
  setsid`; `--name=<name> -- <url>`, so a name or URL starting with "-" is
  no option), from a worker (`tui/clones.rs`, section 4). That child:
  - gets `git clone --progress` (git prints no progress when its output is
    captured);
  - must not prompt: `stdin` closed, `GIT_TERMINAL_PROMPT=0`, ssh in batch
    mode, its own session (no controlling terminal), so a password or
    host-key prompt cannot draw over the TUI or hang; such a clone fails with
    a message to run `itm project add` in a terminal;
  - runs without `ITM_POPUP` (which would delay every error by 2.5 s);
  - is started with SIGTERM not ignored, since an already ignored signal
    stays ignored during the clone and cancel would then not stop it (`itm
    setsid` catches SIGTERM before it execs, which exec resets to the
    default; std cannot reset a signal without `unsafe`);
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
  place (`fs::hard_link` fails if notes exist), so racing writers start
  them once, existing notes are never overwritten, and an append never
  lands before the header. Without hard links (some FUSE or network
  filesystems) it falls back to an exclusive create (`create_new`). An edit of a task
  without notes starts them from the task as it was before the edit, then
  appends. Appends are one write each in append mode.
- Only real changes are appended (stripped values compared): title,
  description and project. The daemon never writes notes.
- An editor that has the file open and saves after itm appended drops that
  entry (nvim warns that the file changed). Accepted.

### Settings
- `config.toml` is read on every use (`Itm::settings`): the daemon every pass,
  so a toggle in the switcher applies without a restart. A missing file is
  the defaults. Unknown settings and values that are not booleans are
  errors, so a typo does not silently do nothing.
- `change_setting` (`config.rs`) edits the TOML document (toml_edit) and
  writes it atomically: comments stay, the one after the changed value too
  (its decor is copied onto the new value), a new key goes after the last
  one of its table, and a missing `[herdr]` table goes after the file's
  comments and one blank line. It follows quoted keys, dotted keys and
  inline tables (the Python itm changed one `key = value` line and refused
  those). It checks that the result reads back with the new value before
  writing, and refuses with "edit it by hand" otherwise (a file with a
  mistake in it, a key that is a table), leaving the file unchanged. A TOML
  syntax error is said on one line ("… is not valid TOML: <what> (at line
  L, column C)").
- A broken file (also one that is not UTF-8) never stops the daemon: it
  logs it once and runs without workspace sync (the safe choice: nothing is
  added or removed) until the file is fixed. Everything else treats it as
  off too (`follows_workspaces`); the Settings screen and the edit form show
  the error.
- `change_setting` holds `.config.lock` next to the file, writes through a
  symlink to its target (home-manager, stow), and turns a failed write into
  an error; the Settings screen saves one toggle at a time and shows any
  failure.

### Notifications
- Herdr's own toasts cover the attached session; the daemon notifies only for
  other sessions. Delivery order: Herdr toast in an attached session, else a
  desktop notification (`system::notify_command`): `$ITM_NOTIFY_COMMAND` with
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
- **Superseded** by the Rust switcher (2026-11-06, ratatui): its rules for
  the TUI are there, and say where they come from. Textual's own rules
  (lazy imports, `check_action`, `App.run_test()`, `$variables` in markup,
  `check_consume_key`, `select_on_focus`, awaiting `dismiss()`,
  `App.suspend()`) and the Python layout (core, CLI and daemon in
  `inbox_task_manager/__init__.py`, the screens in `tui.py`, the OS layer
  in `system.py`) went with the Python itm.
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
- **`system.rs`** (in the Python itm, `system.py`) holds what differs
  between operating systems: the process table (`Processes`: `/proc`, else
  `ps`), notification commands and finding a `date` that reads `-d`; the
  runtime folder is `paths.rs`. `ps` gets one `-o` per column: FreeBSD
  reads everything after the first `=` of an `-o` list as the header. Which
  one runs is chosen at run time, not compile time, so every path is built
  and tested on Linux too (section 4, 2026-11-06).
- **Snooze phrases.** `snooze.rs` (`day_and_clock`) reads clock times (`14:30`, `5pm`),
  `today`, `tomorrow`, weekdays, `next <weekday>` and ISO dates the way GNU
  `date -d` does (a weekday is today when it matches; a day without a clock
  starts at midnight, except `tomorrow`, which keeps the time of day). A
  clock alone rolls over to tomorrow once it passed, as `14:30` always did.
  Anything else still goes to a `date` that reads `-d` (GNU's, `gdate` on
  macOS, or uutils'; `system::date_reads`), found by trying a relative
  phrase (`1970-01-01 +1 day`) rather than by its version text: BusyBox
  `date` reads only absolute times.
- **Packaging** (now section 7). The flake has the package
  (`nix/package.nix`), an overlay, a home-manager module
  (`nix/home-module.nix`: the package and the daemon as a systemd or
  launchd user service) and checks that run the tests.
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
  lives and the test that pins it. (The cutover, below, ended this.)
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
  snoozed until an agent asks. Linux refuses a connection to a full backlog
  with EAGAIN; macOS and the BSDs refuse it with ECONNREFUSED, as they
  refuse a socket nobody listens on. There itm asks the session's other
  socket, `herdr-client.sock` (`refused_by_a_live_server`): a stuck server
  still listens on it, while a session that stopped removed both sockets,
  and one whose server crashed left both with nothing listening (checked
  against Herdr 0.9.3 on Linux; the macOS path has its unit test but has
  not run on macOS yet).
- **Stopping a session** is a `server.stop` request, not a detached
  `/bin/sh herdr session stop`: Herdr answers before it shuts down, so the
  caller may run inside the session it stops (archiving the current task).
- **Errors** are an enum (`error.rs`) whose messages are the Python itm's.
  Its variants are the errors callers tell apart (which Herdr failure, a
  missing `herdr` or `git`, a missing task or project, a folder already
  saved versus a name taken, R8.4 and R8.5, and `Exit`: an exit status
  said by nothing else, for a no to a question and a signal that stopped a
  clone); everything else is a sentence written where it happens.
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
  - ratatui-textarea (crossterm only): the form's description, the one
    field with several lines (cursor moves, word wrap, undo), which
    tui-input cannot edit; from the ratatui organization, and its own
    dependencies are already in the graph through ratatui;
  - rustix: the safe system calls std lacks (`getuid`, `kill` with a
    signal, `setsid`, a non-blocking `connect` with `poll`); `unsafe` code
    is denied in the crate;
  - signal-hook: the attach loop's SIGHUP, SIGTERM and SIGINT, received on
    a thread (already in the graph through crossterm);
  - tempfile: atomic writes and test folders;
  - unicode-normalization: `slugify` like Python's `unicodedata` (NFKD, then
    drop combining marks);
  - shlex: `$ITM_NOTIFY_COMMAND` and `$EDITOR` split like `shlex.split`;
  - toml_edit (`parse` and `display` only): `config.toml` (R10.4), read and
    changed as a document, so a toggle keeps the user's comments and
    layout; one front end for both, where serde with `toml` would read and
    still need toml_edit to write. Its own dependencies are pinned in
    `Cargo.lock` to releases at least two weeks old (winnow 1.0.4,
    toml_writer 1.1.3, toml_datetime 1.1.2), not the newest;
  - insta (tests only): screen snapshots on ratatui's `TestBackend`.
- **Rules for the TUI** (the switcher and every screen; they replace the
  Textual switcher's rules of 2026-11-04, and keep what those were for):
  - The app (`tui/app.rs`) holds state and turns keys into actions with no
    I/O and no clock; the event loop (`tui/mod.rs`) says what time it is,
    draws, and runs anything that can block (Herdr, the store lock, git, a
    hand-off, which waits up to 2 s, a folder listing) in a thread of its
    own, never on the loop. There is nothing to import lazily: the TUI is
    part of the one binary and costs nothing until it runs.
  - A periodic refresh (every 2 s) keeps the highlighted task by its id,
    and writes the store only when there is a session to adopt or a
    tombstone to expire (`Work::load`).
  - Switching happens after the switcher has closed (`tui::run` returns the
    session, and the CLI calls `switch_to`), so the attach loop gets the
    terminal back and a hand-off never runs on the loop.
  - Actions that move the switcher's own client away (snooze, archive,
    delete of the current task) close it afterwards (`App::done`).
  - Ctrl+A is both archive and a common Herdr prefix (`prefix = "ctrl+a"`;
    Herdr's default is Ctrl+B), so archiving a task whose session runs asks
    first; a stray key after the prefix does nothing.
  - Screens over the list (dialogs, the New task and edit form, its project
    finder, the add, rename and remove dialogs, Projects, Settings, help)
    are a stack (`tui/screen.rs`): only the top one gets keys, so list keys
    never act on the task behind a dialog, and Esc closes only the top. A
    new screen is a new `Screen` variant. A key's `Outcome` says whether
    the screen stays, closes, has work done while it stays (`Act`), or
    opens another over itself (`Open`); a screen that closes with an action
    (`CloseAnd`: the finder's projects, the add dialog's new project, a
    question's answer) hands it to the screen below (`Screen::returned`),
    like a dialog's result, and with none below the switcher acts on it.
  - A job's result goes to the screen that asked for it (`Screen::takes`):
    the add dialog's saves, checks and clones, the Settings screen's
    toggles, the Projects screen's renames and removals. When the user went
    on to another screen meanwhile, the asking screen only updates itself
    (no screen opens or closes behind the user's back); when it closed, the
    switcher says what went wrong, if anything.
  - Screens do no I/O: what they need read they say in `Screen::needs` (a
    folder listing, the add dialog's check of what was typed) and when
    (`Screen::due`, a check once typing pauses for 250 ms), and the loop
    reads it after each event and hands it back. A time typed that only
    GNU `date` reads (`next week`, R4.1), in the snooze dialog or the
    form's "Other time…", is a job (`Job::SnoozeTime`): `Until::read`
    reads itm's own phrases in the screen and leaves the others, a worker
    runs `date` (`Until::parse` with `system::date_reads`), and its answer
    comes back to the screen that asked, which then snoozes or saves, or
    says why not with what was typed still there. Keys wait meanwhile (a
    few milliseconds; `date` gets 5 s).
  - The loop waits for a key, a worker's result (over a channel) or a
    deadline (the 2 s refresh, a message's expiry, a screen's check).
    Idle, it blocks on the terminal until the deadline; while a worker runs
    it looks at the results every 10 ms. One load runs at a time, and a
    job that can change tasks asks for a load once done. A worker that
    panics still sends a result, so a refresh always ends.
  - The loop ends (Esc, a switch, a screen alone closing, the terminal
    gone, a signal during a clone) only once the jobs that change state are
    done (`Job::must_finish`): a delete, an archive or a project removal
    stopped halfway leaves a session running unseen or a clone half
    deleted (Textual joined its workers too). Meanwhile it reads no keys and
    says "Finishing first…"; a failure among those jobs keeps the switcher
    open to show it (UX principle 11). A clone is not waited for (it stops
    on the way out and keeps nothing, R8.8), nor is a folder listing.
  - Keys are read on the loop's thread, only while it waits, never by a
    thread of their own: crossterm reads input while it polls, so a reader
    left running after the switcher closed would take keys meant for the
    `herdr session attach` that follows a switch, and it cannot be stopped
    at once (joining one cost 50 ms on every exit). Keys are handled one at
    a time, in order, so a key that opens the finder (typed on the form's
    project field) is followed by the next key in the finder, and no key
    reaches a picker before it is up (which the Textual switcher had to
    guard against). A paste (bracketed paste) is
    fed as keys, its whitespace (newlines too) one space each, so a pasted
    URL opens the finder like typing does and never presses Enter; the
    form's description, the one field with several lines, takes it as it is.
  - A one-line field focused with text in it has that text selected, as
    Textual's `Input` had (`select_on_focus`): a character replaces it,
    Backspace or Delete clears it, Left or Home goes to its start, and any
    other key unselects it (`screen::type_into`, drawn reversed). So a field
    that opens filled in (the rename dialog, the edit form's title, `itm new
    -i "<title>"`, the add dialog `add-workspace` fills in) takes a new value
    without being cleared first. The finder's query is never selected (the
    key that opened it must not be replaced by the next), and a snooze time
    that could not be read puts the cursor back in it unselected, so typing
    goes on.
  - The main list's keys are matched before the search box gets a key,
    except `?`, which opens the help only while the search is empty and is
    typed otherwise. F1 opens it
    always. The footer shows seven keys (`?` among them); the help lists
    the rest (Ctrl+U, Ctrl+A, Ctrl+D, Ctrl+O, Ctrl+S, Alt+1…9, Esc).
  - Text is drawn as ratatui spans, never parsed as markup, so a title or
    project name with `[` styles nothing. Colors are the terminal's named
    ANSI colors (`tui/draw.rs`), so the switcher follows the terminal's
    (and Herdr's) theme; the Textual switcher's `ITM_THEME` is gone.
  - Time, keys and work are injected (`Events`, `Work`, the backend), so
    tests run the loop itself on ratatui's `TestBackend` with a scripted
    clock, folder listings read in real threads, and clones as real child
    processes.
  - One loop runs the switcher and the screens the CLI opens alone
    (`App::alone`): there is no list behind them, nothing is loaded, their
    work runs in workers like the switcher's, and the loop ends when the
    last screen closes, returning what it closed with for the CLI to act on.
    So `itm snooze` without a time (R4.1) runs the snooze screen alone; Esc
    is `cancelled` (exit 1, and no delay in a popup). So do `itm new`
    without a title or with `-i` (the New task form, R1.1), `itm edit`
    without options (the edit form, R5.4), `itm delete`'s question (no is
    exit 1 and says nothing, like the Python itm), `itm project remove`'s
    clone dialog, and `itm project add-workspace`'s add dialog and folder
    question (R10.5).
  - The New task and edit form (`tui/form.rs`) has its fields in Tab order
    (title, description, projects, and for a new task the snooze and a time
    typed for "Other time…"); Enter in the title (or a time) saves, Ctrl+S
    saves from any field, and Esc asks first when something was typed (for
    an edit: also when the projects changed). The description is a
    ratatui-textarea (Enter adds a line, Tab moves on). The snooze (a
    drop-down `Select` in Textual) is a field whose choice ←/→ change;
    typing on it picks "Other time…" and types the time there. Below the fields the form says
    what saving does, worked out from the projects' folders as the last
    refresh saw them (`Saved`: which folders are missing, and each saved
    folder resolved).
  - The project finder (`tui/finder.rs`) and the add dialog (`tui/add.rs`)
    complete folders (`tui/completion.rs`) as the Textual switcher's
    `FolderLister` and `FolderSuggester` did: each screen keeps the listings it asked for
    while it is open, each folder read once in a plain thread that is never
    joined (a scan blocked on NFS, autofs or a cold /nix/store holds up
    neither the loop nor the exit), and filters them in memory on every
    key. A listing comes back to every screen that asked for it, and one
    for a screen that closed meanwhile is dropped. While a folder is read
    the finder has no rows ("Reading the folder…"), so Enter and Tab never
    act on rows left from earlier text; a listing that arrives late rebuilds
    the rows from what is typed then. Symlinked folders are resolved in the
    reading thread (`Listing.links`), so a saved folder reached through a
    link is found, and rows are worked out from the listing alone (no
    filesystem call while typing). Ghost text (`Folders::ghost`) is a completion that starts
    with exactly what was typed (no casefolding), none after a trailing
    "/"; it is worked out when drawn, from the listings the dialog has, so a
    late listing shows it and a kind switch (only folders complete) clears
    it. → at the end of the source, or Tab, takes it.
  - Fuzzy matching is `fuzzy::fuzzy_match` (linear time), for projects and
    folders alike.
  - The add dialog checks what was typed once typing pauses (Python's
    0.25 s debounce): a `Job::CheckProject` in a worker says where the
    project would go and whether it is saved already or its name taken, as
    adding would (`Itm::check_project`, which may ask git how `insteadOf`
    rewrites the URL); only the check of what is typed now is kept. Enter
    adds, and adding checks again; a clone is checked before its child
    starts, so a name taken comes back typed and puts the cursor on the
    name (`Error::is_about_the_name`, R8.5). Its kinds are tabs: Ctrl+←/→
    cycle them, plain ←/→ change them only in an empty source and stop at
    the ends, never while a save or a clone runs.
  - A clone from the add dialog (`tui/clones.rs`) runs the child `itm
    project add git --name=<name> -- <url>` from a worker that reads its
    output as it comes and says the last line of each chunk (git's
    progress, `\r` included). The child is registered with the loop's
    `Clones` and reaped only under its lock, so the pid signalled is never
    another process's. Esc sends it SIGTERM (it removes what it copied; the
    dialog says "Stopped" once it ended, and takes the project if the clone
    finished first); every way out of the loop stops the children that
    run (a guard), and one started after that is stopped at once. While a
    clone runs the loop catches SIGHUP (the popup closed), SIGTERM and
    SIGINT (`ExitSignals`), stops it and exits with 128 + the signal.
  - The Settings screen (Ctrl+S, R10.4) shows the settings each refresh
    read (`Snapshot.settings`, so a file fixed meanwhile shows), saves one
    toggle at a time in a worker, and shows a file with a mistake, with
    nothing to toggle, or why a save failed.
  - The Projects screen (Ctrl+P, R8.3) follows each refresh, keeping the
    highlighted project by its id; Enter closes it and searches for
    `@name` (R6.2). Ctrl+D first reads the clone in a worker
    (`Job::RemovalPlan`, git), then opens the remove dialog.
  - The editor for notes (Ctrl+O) is the one blocking call on the loop's
    thread: the terminal is given back first (`Events::suspended`: raw mode
    and bracketed paste off, main screen), taken back after, and drawn
    again in full; nothing reads keys meanwhile, and no lock is held. The
    preview's notes are read by the load worker with the rest of the
    snapshot, so drawing stays pure.
- **Other systems.** `system.rs` keeps `system.py`'s split: the process
  table from `/proc`, else `ps -ww` with one `-o` per column (bounded by a
  5 s timeout), notifications through `osascript` on macOS (title and
  body as arguments, no Open action), else `notify-send`, and the snooze
  phrases itm does not read itself through `gdate`, else `date`, whichever
  reads a relative phrase (`date_reads`, as `gnu_date` probes). Which one is
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
  `times.rs`, `error.rs`, `git.rs` (URL normalization, `insteadOf`, checkouts,
  local work, the clone, the repository a folder is in), `config.rs` (the
  settings), `workspaces.rs` (which project a workspace is in, and `itm
  project add-workspace`), `fuzzy.rs` (fuzzy matching), `folders.rs`
  (folder completion) and `tui/` (`app.rs` the switcher's state and keys
  without I/O, `screen.rs` the screen stack and the small dialogs,
  `draw.rs` the list, the preview, the small dialogs and the helpers every
  screen draws with, `mod.rs` the event loop and its workers; the larger
  screens each in a module with their state, keys and drawing: `form.rs`,
  `finder.rs`, `add.rs`, `projects.rs` (with the rename dialog),
  `help.rs`; `completion.rs` a screen's folder listings, `clones.rs` the
  clone children). Tests sit
  next to the code; `tests/switching.rs` runs the switching protocol with
  real processes and signals.

### 2026-11-06: the Rust itm replaces the Python itm (the cutover)
- **Why.** The Go-versus-Rust standoff (an MVP of the core in each, from
  one brief, the Python itm as the incumbent; three reviewers scoring
  maintainability 40%, focus 30% and potential 30%) gave Rust 7.95, Go
  7.25 and Python 6.35, and every reviewer ranked Rust first and Python
  last. Their reasons for Rust: a typed domain (task states, `Until {Time,
  Agent}`, `Option` for folders, which turns the optional-folder rules of
  section 2 into compile errors), designed test seams (the `Itm` context,
  the `Processes` trait, the daemon split into `observe` and a pure
  `poll_once`, the switcher as a pure state machine with snapshot tests),
  byte-for-byte compatibility with the Python itm's files (it alone
  tracked the format change made during the standoff), and speed where
  users feel it. Against Python: a 3,034-line core that its 244
  `mock.patch` sites kept from being split, untyped dicts, no type checker
  or linter, and Textual's import as a floor under the switcher's first
  frame. Go read best and built fastest, but rewrote Python's state in the
  old format. Their advice, followed: an incremental rewrite with the
  Python itm as the reference until parity (2026-11-06 above), then this.
- **Measured** (the standoff's method, `scripts/measure.sh`: a sandbox with
  5 tasks, 3 with running headless sessions, one with a blocked fake agent;
  `itm list` and `itm status` the median of 20 runs; the switcher's first
  frame with the tasks the median of 10 runs of `itm ui` with
  `ITM_EXIT_AFTER_DRAW=1` in a 120×40 tmux pane; the daemon's CPU, its own
  and its children's from `/proc/<pid>/stat`, and peak RSS over 60 s;
  release build, Herdr 0.9.3, on the same machine):

  |                       | `itm list` | `itm status` | first draw | daemon CPU | daemon RSS |
  |-----------------------|-----------:|-------------:|-----------:|-----------:|-----------:|
  | Python 0.2.0          | 52.3 ms    | 60.0 ms      | ≈175 ms*   | 0.78%      | 24.5 MB    |
  | Rust MVP (standoff)   | 1.0–1.4 ms | 1.1 ms       | 2.2–2.5 ms | 0.033%     | 4 MB       |
  | Rust at the cutover   | 1.31 ms    | 1.61 ms      | 2.64 ms    | 0.050%     | 5.1 MB     |

  \* The Python switcher has no `ITM_EXIT_AFTER_DRAW`; 175 ms is the
  standoff's measurement of its first frame with tasks, on the user's
  machine with real data (Python start 17 ms, Textual import about 105
  ms). The full product costs the Rust itm little over the MVP: the
  binary is 3.7 MB, and its daemon reads a whole `session.snapshot` of
  each running session (the workspaces as well as the agents) and the
  settings file on every pass.
- **Size.** Rust: 10,380 lines of product and 7,110 of tests (281 unit
  tests, 14 real-process tests in `tests/switching.rs`, 21 screen
  snapshots), 14 direct dependencies (138 crates in `Cargo.lock`); a
  release build of the crate takes about 45 s (LTO, one codegen unit),
  more from clean. Python 0.2.0: 5,594 lines of product and 3,997 of tests
  (280 tests), one dependency (Textual).
- **What changed at the cutover.** The Python package, its tests and
  `pyproject.toml` left the tree (commit cb07b9d has them); the Nix
  package, the flake's checks and CI build the crate (section 7); the
  version went to 0.3.0, after the Python 0.2.0; `PARITY.md` lists every
  requirement and rule with its status, where it lives and the test that
  pins it. The cross-check still runs a pinned build of the Python itm
  (section 6), and the smoke run passes against Herdr 0.9.3 and 0.9.5.
- **Deliberate differences** from the Python itm are listed in
  `PARITY.md` ("Known differences"); the ones a user sees: one command,
  `itm` (no `inbox-task-manager`); no `ITM_THEME` (the terminal's
  colors); the Settings screen edits dotted keys and inline tables;
  errors from the system read as Rust words them; the form's snooze is a
  field, not a drop-down; no mouse.
- **Not done.** No macOS build has linked or run (CI's macOS job is
  written; the code type-checks for macOS and FreeBSD, section 6), the
  BSDs are untested, and the oldest Rust it claims (1.89) is checked only
  by CI.

## 5. Known gaps

- **Agent titles.** The text shown for an agent is its terminal title (for
  Claude Code, the conversation's topic), not the question it asks.
- **Open action.** The desktop notification's Open action moves the most
  recently attached client, whichever session it is in.
- **Short terminals.** The fixed 26-row form popup has not been tried on a
  terminal shorter than that.
- **Workspace folder after a pane swap.** `Layout::workspaces` (`herdr.rs`)
  takes the first pane in layout order; whether Herdr's identity pane follows a
  `pane swap` (position) or keeps the pane (its root) was not checked.
- **First start, end to end.** The headless start with several named
  workspaces was checked by hand against Herdr 0.9.3 (attach adds none); the
  attach loop's use of it is covered by unit tests and the smoke run. While
  it runs (up to `SERVER_START`), the loop's marker already names the new task
  but no attach child exists, so a hand-off in that window (which waits
  2 s) is dropped.
- **Repositories by remote** compare `origin` only; a checkout whose
  remote has another name, or whose remote changed in the last 5 minutes
  (the cache), is not matched by remote.
- **Settings files** are rewritten with LF line endings.
- **Alt+3…9.** Alt+1, Alt+2 and Alt+5 were sent to a Herdr popup; the others
  are assumed to arrive the same way.
- **Ignored signals on macOS and the BSDs** come from `ps -o sigignore=`,
  which was run on Linux's `ps` only.
- **macOS and the BSDs.** The `ps` paths and `osascript` are covered by
  unit tests run on Linux, not by end-to-end runs with Herdr. itm
  type-checks and passes clippy for `aarch64-apple-darwin` and
  `x86_64-unknown-freebsd` (section 6), but it has not been linked or run
  on either: CI's macOS job (build and tests) and the launchd agent are
  written and have not run yet. The BSDs are untested.
- **The Rust switcher's loop** wakes every 100 ms while a clone runs, to look
  for signals, and redraws each time; while a worker runs it looks at the
  results every 10 ms. Neither is measurable in the daemon's or the
  switcher's numbers (section 4), but an idle switcher with a clone running
  is not idle.
- **Help scroll.** The help screen learns how far it can scroll when it is
  drawn (a `Cell` set while drawing), so before its first draw ↓ may go up
  to the number of lines.
- **The attach loop's signals.** On SIGHUP, SIGTERM or SIGINT between two
  attaches, the loop's signal thread looks for an attach child to detach
  for up to 2 s (20 times, 0.1 s apart, as a hand-off does), and the loop
  waits for it before it ends.
- **`Itm.repositories`** (the 5-minute cache of which repository a folder
  is in) is never pruned; the daemon keeps one entry per workspace folder
  it has seen.
- **`Error::Other`** carries every error callers do not tell apart, as a
  sentence; matching on its text would be a bug.
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

- Tests: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`
  and `cargo test` must pass at every commit (`nix develop` has the
  toolchain; `nix flake check` runs all three in Nix's build sandbox, and
  CI on Linux and macOS, section 7). Unit tests sit at the end of each
  module (`#[cfg(test)] mod tests`) and need no Herdr: a fake session is a
  unix socket answering canned JSON (`herdr::fake`), the process table a
  `FakeProcs`, the folders a temporary root (`Itm::under`: every folder of
  itm's under it, the settings too, so no test reads or writes the real
  `config.toml`). Tests never change the test process's environment
  (tests run in parallel threads): what varies is injected. Both
  process-table readers run where they exist (`/proc`, and `ps` when it
  takes BSD-style options). Screens are drawn on ratatui's `TestBackend`,
  some pinned as insta snapshots in `src/tui/snapshots/` (`INSTA_UPDATE=always
  cargo test` rewrites them after an intended change; review the diff). A
  test that pins a requirement names it (`R4.1` in its name or a comment).
  The Python itm's tests (`tests/*.py` until the cutover) were the most
  precise spec; their cases were ported as Rust tests (`PARITY.md`).
- Other systems are type-checked on Linux, every `cfg` path compiled
  without linking or running, with the standard library built for the
  target from nixpkgs' `rustPlatform.rustLibSrc` (the variable is cargo's
  own, for its tests):
  `RUSTC_BOOTSTRAP=1 __CARGO_TESTS_ONLY_SRC_ROOT=<rust-lib-src> cargo clippy
  --target aarch64-apple-darwin -Zbuild-std=std,panic_abort --all-targets
  -- -D warnings` (and `x86_64-unknown-freebsd`).
- Manual end-to-end checks run in a sandbox, never against real sessions:
  - separate `XDG_CONFIG_HOME` (short path: symlink it under
    `$XDG_RUNTIME_DIR`, see the socket path limit), `XDG_STATE_HOME` and
    `XDG_RUNTIME_DIR`;
  - tmux as a scriptable terminal: `send-keys`, `capture-pane -e` (keeps
    colors);
  - wait for a popup to render before sending keys to it, and for it to
    close before the next prefix key: a popup takes every key, the prefix
    too;
  - a client that was handed to another session (re-attached) drops the
    first keys sent to it: wait until it echoes a probe (`smoke.sh` `ready`);
  - start that tmux with `-f /dev/null`, an explicit `-S` socket and a clean
    environment (`env -i`): your own tmux config may restore saved sessions
    into the new server (tmux-resurrect), and `HERDR_*` variables from a
    surrounding Herdr make the sandbox's Herdr refuse to start ("nested herdr
    is disabled").
- A blocked agent without spending tokens: start `claude` in a folder it does
  not trust yet; the trust prompt counts as blocked. A resume test needs one
  short prompt (`claude --model haiku`) in a trusted folder.
- Known pitfalls: `tmux send-keys M-Enter` arrives as Escape then Enter.
  `pgrep -f <pattern>` also matches the shell running it, and processes
  outside the sandbox (an `itm` in your real Herdr session): signal only a
  process whose environment names the sandbox (`ITM_SANDBOX`, which a
  sandboxed Herdr passes on to its popups; `smoke.sh` `sandbox_pids`).
- Non-trivial changes get a review before they are merged.
- `tests/switching.rs` checks the switching protocol with real processes
  and signals: the built `itm`, and the test binary itself playing `herdr`
  (symlinked under that name: an attached client that a signal detaches,
  a headless server answering on its socket, `session delete`) and `git`
  (a clone that waits for a signal or fails as one that may not ask for a
  password), so clones are stopped by real signals, under `nohup` and as
  a TUI's `itm setsid` child. It has its own `main` (`harness = false` in
  `Cargo.toml`); its tests run one after another. Unit tests fake git with
  a small `sh` script (`projects::tests::fake_git`), run as `sh <script>`
  rather than executed, which no parallel test's fork can make busy.
- End to end, against a real headless Herdr with tmux as the terminal:
  `scripts/smoke.sh` (fake agents report themselves blocked over the socket
  API, no real agent runs; its Herdr servers get the keys of
  `contrib/herdr-keys.toml`, and every command there is run through its
  popup, with a fake git so a clone needs no network) and
  `scripts/measure.sh`, both run through
  `scripts/sandbox.sh <name>` (no `HERDR_*` variables, private XDG folders
  under `/tmp/itm-<name>`, notifications to a log).
- `scripts/cross-check.sh` runs the Python itm (the reference: 0.2.0, the
  Rust rewrite's base, built from that commit's flake as the script's
  header says) and the Rust itm on shared state in a sandbox: the same commands
  write the same `tasks.json` byte for byte (timestamps masked) and print
  the same output and errors; each lists the other's state as the other
  does; an edit of the other's file (the old single-project format too)
  changes nothing it should not; a Rust edit waits for the lock. A change
  that touches the files both read adds its commands to it, so the Rust
  itm keeps reading and writing what 0.2.0 does (a user may still run
  both during an upgrade). Its git projects run the Python
  build's inner entry point (`.itm-wrapped`: the wrapper puts its own git
  first on `PATH`) with a git whose clone makes an empty checkout of the
  URL, so no network is needed; where each side adds its own projects,
  their random ids are replaced by names before comparing. For Herdr
  workspaces (R10) each side gets its own headless Herdr session with the
  same workspaces, made in the same order (so their ids match), and its
  own daemon runs for a few passes at a time (the first pass over a
  session only adds). What the Rust itm printed at each step is kept in
  `cross/steps.log`, so a step that passes can be read too.

## 7. Packaging

- itm is one Rust crate (`Cargo.toml`, edition 2024) with one binary,
  `itm`, whose name keeps the attach loop's command line recognizable (the
  process-name rule in section 2). `cargo install --locked --path .` (or
  `--git <repository>`) installs it into `~/.cargo/bin`. Every dependency
  is pinned exactly in `Cargo.toml` and by checksum in `Cargo.lock`
  (section 4, 2026-11-06).
- `ITM_SELF`, when set, is the program child processes (clones) run, so
  they run the same build with the same `PATH`; the Nix wrapper sets it to
  itself. Without it they run the running binary.
- The Nix package (`nix/package.nix`, `rustPlatform.buildRustPackage` with
  `cargoLock.lockFile`, so there is no vendor hash to update) runs `cargo
  test` in its check phase, with git and `ps` as check inputs, and wraps
  `itm` (`wrapProgram`, which keeps argv[0], `…/bin/itm`, for the
  process-name rule) with git, `gdate` (coreutils-prefixed) and, on Linux,
  `notify-send` first on `PATH`. Herdr goes last (`--suffix`): the client
  must match the server's version, so a `herdr` the user already runs
  wins, and the bundled one serves services started without the user's
  `PATH`. `herdr = null` leaves it out.
- The flake has `packages` (the default is the package), `overlays.default`,
  `homeModules.default` (`nix/home-module.nix`: the package and the daemon
  as a systemd or launchd user service, running `lib.getExe`, which is
  `bin/itm`), `checks` (the package, so its tests; clippy with warnings
  denied, on the same sources and crates; rustfmt), a `devShells.default`
  with cargo, rustc, clippy and rustfmt and what the end-to-end scripts
  drive (git, tmux, socat, python3, herdr), and `formatter` (nixfmt).
- The flake follows nixos-unstable, where Herdr is packaged (its lock of
  2026-11-06 has Rust 1.99 and Herdr 0.9.5). It lists no x86_64-darwin,
  which nixpkgs 26.11 dropped; there the overlay and the module's default
  package (`callPackage` with the user's nixpkgs) still work. A
  configuration that installs Herdr from another nixpkgs should make the
  flake's `nixpkgs` input follow that one, so both use the same Herdr.
- CI (`.github/workflows/ci.yml`): `cargo fmt --check`, `cargo clippy
  --all-targets -- -D warnings` and `cargo test` with Rust 1.95.0 (the
  toolchain the crate is developed and checked with, pinned by
  `dtolnay/rust-toolchain@1.95.0`) on Linux and macOS, and `nix build` and
  `nix flake check` on Linux.
- New files need `git add` (`-N` is enough) before a flake build can see them.
- The source fileset is `Cargo.toml`, `Cargo.lock`, `rustfmt.toml`, `src/`
  and the `.rs` files of `tests/`.
