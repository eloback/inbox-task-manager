# inbox-task-manager: requirements

What itm does, numbered so changes and tests can point at them. The README
explains how to install and use it; DESIGN.md records the rules and decisions
behind these requirements.

Every task is a Herdr session named `task-<slug>`; sessions without that
prefix (`default`, `work`, …) are never managed, only attached when asked
explicitly.

## Task states

| State    | Meaning                                                  | Herdr session                            |
|----------|----------------------------------------------------------|------------------------------------------|
| active   | in progress, listed in the Inbox                         | running                                  |
| snoozed  | hidden until a timer, or until one of its agents asks    | keeps running, agents keep working       |
| archived | done, can be reopened                                    | stopped (Herdr keeps layout and agents)  |

## Requirements

### R1 Create a task
- R1.1 `itm new "<title>" [-d TEXT] [-p PROJECT] [--snooze WHEN] [--no-switch] [-i]`. Without a title, or with `-i`, it opens the New task form (title, description, project, snooze) with what was given filled in; `prefix+t` (suggested Herdr keys, README) opens the same form. Enter on the title creates the task with the defaults; Tab reaches the other fields; Ctrl+S creates from any field. Esc cancels and asks first when something was typed; a cancelled form writes no task (a project added from it stays saved).
- R1.2 A task's session starts in its project's folder, else in `$HOME` (`ITM_TASK_DIR` overrides it). This applies to the session's first start only; Herdr restores it afterwards. With `--no-switch` only the metadata is written; the session starts on first attach.
- R1.3 Session name is `task-<slug>`, with a numeric suffix when taken; the title is stored verbatim.
- R1.4 The new task is active and the client switches to it immediately, unless it is snoozed (R1.7). In a Herdr client not started by `itm attach` (which cannot be switched), `itm new "<title>"` fails before saving; the form and the switcher save the task without switching and say why, so nothing typed is lost.
- R1.5 A task has an optional free-text description (leading and trailing whitespace trimmed), shown in the preview and `itm list --json`.
- R1.6 A task has zero or one project (R8). The form's project field opens a fuzzy finder (Enter, or just start typing): the saved projects, most recently used first, narrowed by the letters typed in order and in any case (names, and a repository's URL or a folder's path). A typed path (`~/…`, `/…`, `./…`) lists folders instead, with Tab going into the highlighted one and Enter adding it as a project (or picking it when it is saved already); a typed git URL offers to clone it (or picks it when saved). It can also add a repository or folder through the add dialog. It starts with the project of an `@name` filter in the switcher, else with the project of the task you are in. The task stores the project's id, so renaming the project keeps it.
- R1.7 `--snooze WHEN` creates the task snoozed (R4.1 syntax; not "until an agent", since a new task has no agents) and does not switch to it; like `--no-switch`, only the metadata is written and the session starts on first attach. Invalid input (snooze time, unknown project) writes nothing.

### R2 Switch and attach
- R2.1 `itm attach [task]` outside Herdr runs the client in a loop and reattaches it whenever a switch is requested.
- R2.2 `itm` (or `itm ui`) opens the switcher, a terminal UI; `prefix+shift+s` opens it in a Herdr popup. Choosing a task switches after the switcher has closed: in place inside Herdr (SIGTERM to the client, the outer loop reattaches), by attaching outside it.
- R2.3 The Inbox lists tasks whose agent is waiting for input first, under "Needs you", with the agent and how long it has waited; the other active tasks follow under "Active". Rows show the project, the preview shows the project, the description and each agent.
- R2.4 Enter opens the highlighted task; on a snoozed task it wakes it, on an archived one it reopens it, and the preview names which. Alt+1…9 open row 1–9.
- R2.5 `itm jump` opens the task whose agent has waited longest (`prefix+i`); `itm last` goes back to the most recently used other active task (`prefix+tab`).
- R2.6 Sessions without the prefix can still be attached by name (`itm attach default`) but are not listed as tasks.

### R3 Notifications (agents only)
- R3.1 One notification when an agent in any task becomes `blocked`; plain shells never notify.
- R3.2 Delivery: a Herdr toast in the attached session when a client is attached, else a desktop notification: `$ITM_NOTIFY_COMMAND` when set (it gets the title and body as arguments), else `notify-send` (Linux, BSD) or `osascript` (macOS). Without any of them the daemon logs the notification instead.
- R3.3 The notification names the task and agent; the desktop notification's "Open" action switches the most recently attached `itm attach` client to the task (offered only by `notify-send`, and only when such a client exists).
- R3.4 A wait is reported once (deduplicated per pane and state change).
- R3.5 Snoozed tasks are included: a blocked agent notifies **and** wakes the task.

### R4 Snooze
- R4.1 `itm snooze [task] [when]` (`30m`, `2h`, `1h30m`, `2d`, `45` minutes, `14:30` or `5pm` (the next time the clock shows it), `today 17:00`, `tomorrow 9:00`, `friday 2pm`, `next mon`, `2026-12-01 14:30`; other phrases such as `next week` when GNU `date` is installed, as `gdate` on macOS; at most 10 years); without a time it opens the snooze dialog. The first word is the task when it names one, else the current task is snoozed. The dialog offers presets in time order (past ones left out) and accepts a typed time.
- R4.2 Snoozed tasks leave the Inbox; their session keeps running.
- R4.3 When the timer expires the task becomes active again, with a notification.
- R4.4 A blocked agent in a snoozed task wakes it early, with a notification.
- R4.5 `itm snooze --until-agent` (the dialog's "Until an agent needs me") snoozes without a timer: any blocked agent wakes the task, and so does its session stopping (nothing could ask any more). It is refused (and not offered) when the session is not running or an agent is already waiting, since nothing could wake the task then.
- R4.6 `itm wake [task]` (or `unsnooze`, Ctrl+U in the switcher) wakes a snoozed task.
- R4.7 Snoozing, archiving or deleting a task moves every `itm attach` client on it to the next active task (waiting agents first, then most recently used), else to `default`. The switcher closes when it moved its own client.

### R5 Archive, delete, edit
- R5.1 `itm archive [task]` stops the session, keeps its saved state, marks it archived. In the switcher (Ctrl+A, also a common Herdr prefix) it asks first when the session is running.
- R5.2 `itm reopen <task>` (or opening it from the Archived view) makes it active and attaches.
- R5.3 `itm delete <task>` removes the task and its session permanently, after confirmation. A tombstone keeps the session from being re-adopted while it is being deleted. Its notes (R9) are deleted too; its project stays.
- R5.4 `itm edit <task> [--title T] [--description D] [--project P | --no-project]` (Ctrl+E in the switcher; without options it opens the form) changes a task's title, description (an empty one clears it) or project. A session starts in its project's folder only once, so a new project for a task whose session runs opens a tab in the project's folder in that session (focused when it is the task you are in); a stopped session keeps its folders and itm says so. The form shows which of these will happen before you save.

### R6 Views, search and counts
- R6.1 Switcher views: Inbox (active tasks), Snoozed, Archived, All, each with its count; Tab / Shift+Tab change view and keep the search text.
- R6.2 Search matches titles. `@name` (or `@"a name"`) shows only that project's tasks; the name matches in any case, an id works too, and an unknown name says so. When nothing matches, a "Create task" row (Enter) creates the task with the search text as its title.
- R6.3 Snoozed entries show when they wake, archived entries when they were archived, in words ("in 1h 30m", "2d ago").
- R6.4 `itm list [--view active|snoozed|archived|all] [--json]` for scripts; the plain form prints session, state, title, project and the number of waiting agents, tab-separated. (The switcher's `itm ui --view` calls the active view `inbox`.)
- R6.5 `itm status [--format F] [--json]` prints counts for a window title or bar, by default `⚑ waiting ● active ◔ snoozed`; `?` for waiting when the daemon is not running.

### R7 Daemon
- R7.1 `itm daemon` (a user service: systemd on Linux, launchd on macOS) polls agents in every running task session, sends notifications and wakes snoozed tasks.
- R7.2 It works with no Herdr client attached.
- R7.3 Its status cache records when each wait started. When the cache is older than 15 s, the switcher says the daemon is not running instead of showing nothing waiting.

### R8 Projects
- R8.1 A project is a named git repository or local folder with a stable id, saved once and reused by any number of tasks. Tasks sharing a project work in the same folder.
- R8.2 `itm project add git <url> [--name N]`, `itm project add dir <path> [--name N]`, `itm project rename <name> <new name>`, `itm project list [--json]`, `itm project remove <name>`. The name defaults to the repository or folder name. Commands take a project's name (ignoring case) or its id.
- R8.3 The switcher's Projects screen (Ctrl+P) lists the projects with their tasks and last use. Enter shows a project's tasks (`@name` filter); Ctrl+N adds one, Ctrl+R renames, Ctrl+D removes. Adding shows where a clone goes, a duplicate or a taken name as you type, and git's progress while cloning; Esc stops a clone and nothing of it is kept. A folder source completes as you type (ghost text; Tab or → takes it).
- R8.4 Adding a repository or folder that is already saved fails with `project already exists: <name>`. Folders compare by resolved path. Repositories compare by URL, normalized after git's `insteadOf` rewriting, so `git@github.com:acme/api.git`, `https://github.com/acme/api` and `ssh://git@github.com/acme/api` are one.
  - Any git host works: GitHub, GitLab (with subgroups), Bitbucket, Gitea, self-hosted servers.
  - Azure DevOps ssh (`v3/`), https (`_git/`) and legacy `<org>.visualstudio.com` URLs are one repository, `dev.azure.com/<org>/<project>/<repo>`; the organization ignores case.
  - Not recognized as the same repository: an ssh host alias (`git@my-alias:…`) and the real host name, and Bitbucket Server's https `scm/…` and ssh URLs. Each such form gets its own clone.
- R8.5 Project names are unique, ignoring case. When a new project's name is taken (two remotes named `api`), adding it fails with `project already exists: <name> (<its url or folder>)` until one of them gets another name: `--name` (the add dialog keeps you on the name) for the new one, or a rename of the old one. A folder inside itm's projects folder (R8.6) cannot be added as a folder project; add its repository instead.
- R8.6 Repositories are cloned to `$XDG_DATA_HOME/inbox-task-manager/projects/<host>/<path>` (`ITM_PROJECTS_DIR` overrides the root). A checkout of the same repository already there is reused; any other folder or a symlink there is an error.
- R8.7 Removing a project is refused while any task uses it, archived ones included. Removing a folder only forgets it. For a git project, `itm project remove` asks only about a clean clone itm may delete, defaulting to keep it (`--delete-clone`, `--keep-clone` skip the question; without a terminal the clone is kept); a clone with local work (uncommitted or untracked files, commits on no remote including tags and a detached HEAD, stashes) is kept unless `--delete-clone` is given. The switcher's remove dialog follows the same rules and only ever offers to keep such a clone; cancelling changes nothing. Only a real checkout of the repository inside the projects folder, holding no other project, is ever deleted. Deleting a task never touches its project.
- R8.8 A clone started from the switcher runs as a child `itm project add` that never prompts. A repository that needs a password or a host-key confirmation fails with a message to add it from a terminal.

### R9 Notes
- R9.1 Every task has notes, a markdown file `notes.md` in `$XDG_DATA_HOME/inbox-task-manager/tasks/<session>/`, for whatever context the task needs. They stay through snooze, archive and reopen, so a reopened task still has its context; deleting the task deletes them.
- R9.2 A new task's notes start with its title as a heading, a line with when it was created, its project and its session, and its description.
- R9.3 Changing a task's title, description or project (R5.4, CLI or form) appends a `## <date time> · Title changed` (`Description changed`/`cleared`, `Project changed`/`removed`) entry with the new value. Only real changes are logged. Tasks without notes yet (made before this, or adopted sessions) get them when first opened or edited, starting from the task as it was.
- R9.4 `itm notes [task]` opens them in `$VISUAL`, else `$EDITOR`, else nvim, vim, vi or nano; without a task, the current one. `prefix+shift+e` opens the current task's notes in a Herdr popup; Ctrl+O in the switcher opens the highlighted task's (any view) and returns to the switcher. `itm notes --path` prints the file and `itm notes --append TEXT` (`-` reads stdin) adds a dated `Note` entry without an editor, for example from an agent working in the task; `itm list --json` rows carry the path as `notes`.
- R9.5 The switcher's preview shows when the notes were edited and their last lines.

### Non-functional
- N1 Task and project metadata lives in `$XDG_STATE_HOME/inbox-task-manager/tasks.json`; clones (R8.6) and notes (R9.1) live under `$XDG_DATA_HOME`; Herdr remains the source of truth for layouts and agents.
- N2 Runs on Linux and macOS (the BSDs are untested) with Python 3.11+ and Textual. Installs with pip, pipx or uv, or with Nix (a flake with a package, an overlay and a home-manager module that runs the daemon as a user service). Herdr's key bindings stay in the user's Herdr config.
- N3 The switcher first draws in about 250 ms; it reads agent state from the daemon's status cache instead of querying every session, and refreshes every 2 s in the background, keeping the highlighted task. The CLI and the daemon do not load Textual.
- N4 Never touches sessions without the task prefix, except attaching to them when asked.

### Out of scope
Git worktrees per task (tasks sharing a project share its folder), moving a running session to another folder, fetching or updating clones, syncing between machines, non-agent notifications.
