# Parity: the Rust itm against the Python itm

Where the Rust version (the crate at the repository root) stands against
every requirement in `REQUIREMENTS.md` and every rule in `DESIGN.md`
section 2. The Python itm (`inbox_task_manager/`, `tests/*.py`) is the
behavior reference until the cutover; `scripts/cross-check.sh` runs both on
shared state. Updated with every change that moves a row.

Status: **done** (behaves like the Python itm, pinned by a test), **partial**
(what is missing is said), **missing**, **n/a**. Tests are named
`module::test` for unit tests in `src/`, `switching::test` for the
real-process tests in `tests/switching.rs`, and `cross-check`, `smoke` for
the scripts in `scripts/` (run in a sandbox against the Python build and a
real headless Herdr).

Last updated: milestone 1 (foundation).

## Requirements

| ID | What | Status | Where | Pinned by |
|---|---|---|---|---|
| R1.1 | `itm new` with `-d`, `-p`…, `--snooze`, `--no-switch`, `-i`; the New task form | partial | `cli.rs` (New), `itm.rs` `create_task` | `itm::a_new_task_is_active_with_a_unique_name_and_a_trimmed_description`, cross-check (`new`). Missing: the form (`itm new` without a title, `-i`, `prefix+t`); without a terminal it says what Python says |
| R1.2 | first start: one named workspace per project, first focused, else `$HOME`/`ITM_TASK_DIR`; `--no-switch` writes metadata only | done | `projects.rs` `prepare_first_start`, `herdr.rs` `start_server` | `projects::a_first_start_opens_a_named_workspace_per_project`, `projects::a_session_starts_in_its_first_project_with_a_folder`, `switching::a_first_start_runs_its_server_in_its_own_session`, smoke (two named workspaces) |
| R1.3 | `task-<slug>`, numeric suffix, title verbatim | done | `tasks.rs` `slugify`, `unique_session_name` | `tasks::slugs`, `tasks::session_names_get_a_suffix_when_taken_ignoring_case`, `itm::a_name_is_not_reused_while_its_session_or_notes_exist`, cross-check (unicode title, suffix) |
| R1.4 | active and switched to at once unless snoozed; a plain Herdr client: `new` fails before saving, the form and switcher save and say why | partial | `cli.rs` (New), `tui/app.rs` `done` | `switching::a_task_is_not_saved_when_switching_to_it_cannot_work`, `tui::app::a_task_created_where_switching_cannot_work_is_saved_and_says_why`. Missing: the form |
| R1.5 | optional description, trimmed, in the preview and `list --json` | done | `itm.rs` `create_task`, `tui/draw.rs` | `itm::a_new_task_is_active_with_a_unique_name_and_a_trimmed_description`, `tui::draw::the_inbox`, cross-check |
| R1.6 | projects in the form: fuzzy finder, paths, URLs, add dialog; ids stored | partial | `store.rs` (ids) | `projects::a_new_task_takes_projects_by_name_and_marks_them_used`. Missing: the form and its finder |
| R1.7 | `--snooze` creates it snoozed, no switch, not until an agent; invalid input writes nothing | done | `cli.rs`, `itm.rs` `create_task` | `itm::invalid_input_writes_nothing`, cross-check (`--snooze 2h`, `agent`, `0m`) |
| R2.1 | `itm attach` outside Herdr loops and follows switches | done | `switching.rs` `attach_loop` | `switching::the_attach_loop_follows_hand_offs_until_a_detach`, `switching::a_hand_off_written_during_an_attach_is_followed` (unit), smoke |
| R2.2 | the switcher; switch after it closed, in place inside Herdr | done | `tui/`, `cli.rs` (Ui), `switching.rs` `switch_to` | `tui::enter_returns_the_task_to_switch_to_and_esc_returns_none`, `switching::a_switch_inside_herdr_moves_only_this_sessions_clients`, smoke |
| R2.3 | Inbox: "Needs you" with agent and wait first, then "Active"; rows with first project and +n; preview with every project, description, agents | partial | `tui/app.rs` `visible_rows`, `tui/draw.rs` | `tui::app::the_inbox_lists_tasks_needing_you_first_under_headers`, `tui::draw::the_inbox`, `tui::draw::a_task_that_never_ran_starts_in_its_first_project_with_a_folder`. Missing: "workspace open" (needs R10) |
| R2.4 | Enter opens, wakes or reopens and says which; Alt+1…9 | done | `tui/app.rs` `open`, `tui/draw.rs` `preview` | `tui::app::enter_opens_and_says_nothing_else_happens_here`, `tui::draw::a_snoozed_task_says_when_it_wakes_and_what_enter_does` |
| R2.5 | `itm jump`, `itm last` | done | `tasks.rs` `longest_waiting`, `previous_task` | `tasks::jump_goes_to_the_longest_wait_and_last_to_the_previous_task`, smoke |
| R2.6 | other sessions attachable by name, not listed | done | `cli.rs` (Attach), `tasks.rs` | `tasks::only_prefixed_sessions_are_adopted`, `switching::the_marker_names_the_session_while_attached` (unit) |
| R3.1 | one notification when an agent becomes blocked; shells never | done | `daemon.rs` `poll_once` | `daemon::notifies_wakes_and_reports_a_wait_once`, smoke |
| R3.2 | toast in the attached session, else `$ITM_NOTIFY_COMMAND`, `notify-send` or `osascript`, else logged | done | `daemon.rs` `deliver`, `system.rs` `notify_command` | `system::each_system_notifies_with_its_own_program`, `system::a_custom_notify_command_gets_title_and_body`, smoke (command). The toast path has no test of its own |
| R3.3 | names task and agent; Open moves the newest `itm attach` client (notify-send only) | done | `daemon.rs` `desktop_notify`, `switching.rs` `handoff` | `switching::the_open_action_moves_only_the_most_recently_attached_client`, `system::each_system_notifies_with_its_own_program`. Not run end to end (the sandbox notifies through a command, which offers no Open) |
| R3.4 | a wait reported once per pane and state change | done | `daemon.rs` | `daemon::notifies_wakes_and_reports_a_wait_once`, `daemon::a_wait_is_reported_again_after_the_agent_unblocked`, smoke |
| R3.5 | a blocked agent in a snoozed task notifies and wakes it | done | `daemon.rs` | `daemon::notifies_wakes_and_reports_a_wait_once`, smoke |
| R4.1 | phrases, at most 10 years, the first word as the task, the dialog without a time (presets in time order, typed time); GNU `date` for other phrases | partial | `snooze.rs`, `cli.rs` (Snooze), `tui/screen.rs`, `tui::snooze_dialog` | `snooze::phrases_itm_reads_itself`, `snooze::durations`, `snooze::invalid_phrases`, `snooze::presets_are_in_time_order_without_past_ones`, `cli::the_first_snooze_word_is_the_task_when_it_names_one`, `tui::the_snooze_dialog_alone_returns_the_choice`, smoke (`itm snooze` opens it). Missing: the GNU `date` fallback (`next week`) |
| R4.2 | snoozed tasks leave the Inbox; the session runs on | done | `tui/app.rs` `View::shows` | `tui::app::views_change_with_tab_and_keep_the_search` |
| R4.3 | the timer wakes it, with a notification | done | `daemon.rs` | `daemon::notifies_wakes_and_reports_a_wait_once` ("Task back") |
| R4.4 | a blocked agent wakes it early, with a notification | done | `daemon.rs` | `daemon::notifies_wakes_and_reports_a_wait_once`, smoke |
| R4.5 | `--until-agent` / the dialog preset; refused when nothing could wake it; ends on any blocked agent or a stopped session | done | `itm.rs` `agent_snooze_problem`, `daemon.rs` | `itm::snoozing_until_an_agent_needs_a_way_to_wake`, `daemon::snoozed_until_an_agent_wakes_for_an_agent_or_a_stopped_session_only`, `daemon::until_an_agent_wakes_on_a_wait_already_reported`, cross-check |
| R4.6 | `wake`/`unsnooze`, Ctrl+U | done | `itm.rs` `wake_task`, `tui/app.rs` | `itm::wake_archive_and_reopen`, `tui::app::ctrl_u_wakes_only_snoozed_tasks_and_errors_stay_visible`, cross-check |
| R4.7 | snooze, archive, delete move every `itm attach` client on the task to the next one; the switcher closes when it moved its own | partial | `itm.rs` `move_clients_off`, `tasks.rs` `next_active` | `tasks::next_active_prefers_waiting_then_recent_else_default`, `switching::the_attach_loop_follows_hand_offs_until_a_detach`, `tui::app::archiving_the_task_this_switcher_runs_in_closes_it`, smoke. Missing: delete |
| R5.1 | archive stops the session, keeps its state; Ctrl+A asks when it runs | done | `itm.rs` `archive_task`, `tui/screen.rs` | `itm::wake_archive_and_reopen`, `tui::app::archiving_a_running_task_asks_first_and_the_dialog_takes_every_key`, smoke |
| R5.2 | `itm reopen`, or opening from Archived | done | `itm.rs` `reopen_task`, `touch_task` | `itm::wake_archive_and_reopen`, `tui::app::enter_opens_and_says_nothing_else_happens_here` (the attach that follows reopens). Not run end to end |
| R5.3 | `itm delete` with confirmation and tombstone; notes deleted | missing | | (tombstones are respected: `tasks::a_tombstone_blocks_adoption_until_its_session_is_gone`) |
| R5.4 | `itm edit`, Ctrl+E, the form | missing | | |
| R6.1 | four views with counts; Tab keeps the search | done | `tui/app.rs`, `tui/draw.rs` | `tui::app::views_change_with_tab_and_keep_the_search`, `tui::app::the_inbox_lists_tasks_needing_you_first_under_headers` |
| R6.2 | search titles; `@name` filters; a Create row | partial | `tui/app.rs` `visible_rows` | `tui::app::no_match_offers_to_create_the_task`. Missing: `@name` and `@"a name"` |
| R6.3 | when snoozed tasks wake, when archived ones were archived, in words | done | `tui/draw.rs`, `times.rs` | `tui::draw::a_snoozed_task_says_when_it_wakes_and_what_enter_does`, `times::relative_times_round_down_and_say_which_way` |
| R6.4 | `itm list [--view] [--json]`; JSON `projects`, `project`, `project_id` | done | `cli.rs` `task_rows` | `cli::list_rows_put_waiting_tasks_first_and_name_every_project`, cross-check (byte for byte on either's state) |
| R6.5 | `itm status [--format] [--json]`; `?` without the daemon | done | `cli.rs` (Status) | `cli::status_formats`, `store::compact_json_has_pythons_separators`, cross-check |
| R7.1 | `itm daemon` polls every running task session | done | `daemon.rs` | `daemon::*`, smoke. The service files are `contrib/` (they run whichever `itm`) |
| R7.2 | works with no client attached | done | `daemon.rs` | smoke (the daemon notifies before any client) |
| R7.3 | the cache records when each wait started; older than 15 s means no daemon | done | `status.rs`, `tui/draw.rs` | `daemon::the_status_records_when_a_wait_started`, `status::a_stale_or_missing_cache_is_unknown`, `tui::draw::unknown_waits_and_sessions_are_said_not_hidden` |
| R8.1 | a project: git repository, folder or name; kind read off it; one folder per project | partial | `store.rs` `Project` | `store::a_file_written_by_python_is_read_and_rewritten_as_python_would`, cross-check (name-only projects read). Missing: creating git projects (they are read and used) |
| R8.2 | `project add git/dir/name`, `set`, `rename`, `list [--json]`, `remove` | partial | `cli.rs` (Project), `projects.rs` | `projects::a_folder_is_saved_once_under_a_free_name`, `projects::a_folder_named_like_a_name_only_project_is_offered_to_it`, cross-check (`project list --json`). Done: `add dir`, `list`. Missing: `add git`, `add name`, `set`, `rename`, `remove` |
| R8.3 | the Projects screen | missing | | |
| R8.4 | an already saved repository or folder is refused; URLs normalized | partial | `projects.rs` `add_folder_project` | `projects::a_folder_is_saved_once_under_a_free_name`, cross-check. Missing: URL normalization (no git projects yet) |
| R8.5 | names unique ignoring case, said with where the other is; not inside the projects folder | partial | `projects.rs`, `error.rs` (`ProjectNameTaken`) | `projects::a_folder_is_saved_once_under_a_free_name`, `projects::what_cannot_be_a_project`, cross-check. Missing: the add dialog keeping the user on the name |
| R8.6 | clones under `projects/<host>/<path>`, `ITM_PROJECTS_DIR` | partial | `paths.rs` | `paths::the_projects_folder_reads_a_tilde_like_python`. Missing: cloning |
| R8.7 | removing a project and its clone | missing | | |
| R8.8 | a clone from the switcher, never prompting | missing | | |
| R9.1 | notes in `tasks/<session>/notes.md`, kept through snooze, archive and reopen; deleted with the task | partial | `notes.rs` | `notes::notes_start_with_the_task_and_are_never_overwritten`, cross-check (same file). Missing: deletion (R5.3) |
| R9.2 | notes start with title, created line, projects, session, description | done | `notes.rs` `notes_header` | `notes::notes_start_with_the_task_and_are_never_overwritten`, cross-check (identical notes) |
| R9.3 | edits are logged in the notes | missing | | (needs R5.4) |
| R9.4 | `itm notes [task]` in the editor, `--path`, `--append`; Ctrl+O; `notes` in `list --json` | partial | `notes.rs`, `cli.rs` (Notes) | `notes::notes_are_appended_after_one_blank_line`, cross-check (`--append`), smoke. Missing: Ctrl+O in the switcher |
| R9.5 | the preview shows when notes were edited and their last lines | missing | | |
| R10.1 | projects from two sources (hand, workspaces) | missing | | (`found_projects` is kept as an unknown field) |
| R10.2 | which project a workspace is in | missing | | |
| R10.3 | projects follow workspaces (the daemon) | missing | `daemon.rs` | The Rust daemon behaves as the Python one with the setting off: it drops `workspace_projects` every pass |
| R10.4 | settings in `config.toml`, the Settings screen | missing | | |
| R10.5 | `itm project add-workspace` (`prefix+shift+a`) | missing | | |
| N1 | where state lives; older files read as is; `kind` dropped | partial | `paths.rs`, `store.rs` `upgrade` | `store::a_file_written_by_python_is_read_and_rewritten_as_python_would`, `store::odd_project_lists_read_as_none`, cross-check (old format rewritten the same). Missing: `config.toml` (R10.4) |
| N2 | Linux and macOS; pip, Nix (package, overlay, home-manager module) | partial | `system.rs` | `system::every_process_table_reads_command_lines_children_and_endings`, `system::ps_output_is_read_into_command_lines`. Missing: any build or run on macOS; a Nix package for the Rust itm (the standoff's `package.nix` was not brought over) |
| N3 | first draw fast; status from the cache; 2 s refresh keeping the highlight | done | `tui/mod.rs`, `tui/app.rs` `rebuild` | `tui::the_list_refreshes_every_two_seconds`, `tui::app::moving_skips_headers_and_a_refresh_keeps_the_highlighted_task`, `scripts/measure.sh` (milestone 1, release build: first draw 1.7 ms, `list` 1.1 ms, `status` 1.3 ms, daemon 0.033% of a core and 4.2 MB; the Python itm on the same state: 55 ms, 49 ms, 0.75% and 24.7 MB) |
| N4 | never touches sessions without the prefix | done | `tasks.rs` `adopt_sessions`, `itm.rs` | `tasks::only_prefixed_sessions_are_adopted`, `tasks::next_active_prefers_waiting_then_recent_else_default` |

## DESIGN.md section 2: rules that must stay true

### Ownership

| Rule | Status | Where | Pinned by |
|---|---|---|---|
| Only `task-*` sessions are tasks: never stopped, deleted or adopted otherwise; attached only by name or as the `default` fallback | done | `tasks.rs` | `tasks::only_prefixed_sessions_are_adopted`, `tasks::next_active_prefers_waiting_then_recent_else_default` |
| itm's files and their places (tasks.json, config.toml, clones, notes, notified.json, status.json, clients/) | partial | `paths.rs` | cross-check (both find each other's files). `config.toml` and clones are not written yet |
| `<runtime>`: `$XDG_RUNTIME_DIR`, `/run/user/<uid>` when ours, else a private `/tmp` folder refused when not ours or a link | done | `paths.rs` `runtime_dir` | `paths::xdg_runtime_dir_wins`, `paths::private_folder_is_made_private_and_links_are_refused` |

### Switching protocol

| Rule | Status | Where | Pinned by |
|---|---|---|---|
| `itm attach` loops and registers `<runtime>/clients/<pid>` naming its session | done | `switching.rs` | `switching::the_marker_names_the_session_while_attached` (unit), `switching::the_attach_loop_follows_hand_offs_until_a_detach` |
| a switch writes `.next`, then SIGTERM to the attach child; no `.next` ends the loop | done | `switching.rs` `handoff`, `follow_handoffs` | `switching::the_attach_loop_follows_hand_offs_until_a_detach` |
| SIGHUP, SIGTERM, SIGINT end the loop like a detach and remove its markers (Rust) | done | `switching.rs` `attach_loop` | `switching::signals_end_the_loop_and_remove_its_markers` |
| `.next` claimed by rename, never deleted before an attach; a stale one removed once at start | done | `switching.rs` | `switching::a_hand_off_written_during_an_attach_is_followed` (unit) |
| an in-session switch moves only the current session's clients; refused when that session is unknown (Open is the exception) | done | `switching.rs` `switch_to` | `switching::a_switch_inside_herdr_moves_only_this_sessions_clients`, `switching::a_hand_off_moves_only_the_clients_on_the_session_and_signals_their_attach_child` (unit) |
| markers validated by command line; alive but unreadable is skipped; the attach child searched for briefly; `/proc` or `ps -ww` | done | `switching.rs` `live_clients`, `system.rs` | `switching::markers_are_checked_against_the_process_table` (unit), `switching::stale_markers_are_removed_and_their_processes_left_alone`, `system::every_process_table_reads_command_lines_children_and_endings` |
| snooze, archive and delete move clients to the next task | partial | `itm.rs` | see R4.7; delete is missing |
| stopping a session that may host the caller happens after the hand-off (Rust: `server.stop` over the socket instead of a detached `/bin/sh`) | done | `itm.rs` `archive_task`, `herdr.rs` `stop` | `itm::wake_archive_and_reopen`, smoke (archiving the task a client is on; the command runs outside the session) |
| a popup that moved its own client away exits | done | `tui/app.rs` `done` | `tui::app::archiving_the_task_this_switcher_runs_in_closes_it` |
| plain `herdr` clients cannot be switched, and the error says so | done | `switching.rs` `switchable_session` | `switching::switching_inside_herdr_needs_an_itm_client_on_a_known_session` (unit), `switching::a_task_is_not_saved_when_switching_to_it_cannot_work` |
| the process name is part of the protocol (`itm`, `inbox-task-manager`, `inbox_task_manager`) | done | `switching.rs` `is_itm_process`, `Cargo.toml` (`[[bin]] itm`) | `switching::process_names_that_count_as_itm` (unit) |

### Data safety

| Rule | Status | Where | Pinned by |
|---|---|---|---|
| every read-modify-write under `flock`, atomic write; the task re-resolved inside | done | `store.rs` `edit`, `itm.rs` `existing` | `store::concurrent_edits_lose_no_update`, `store::the_lock_is_the_one_python_takes`, `itm::wake_archive_and_reopen` (a task gone), cross-check (waits for the lock) |
| an error inside an edit cancels the write | done | `store.rs` | `store::an_error_inside_an_edit_cancels_the_write`, `daemon::a_failed_store_update_reports_nothing_as_notified` |
| the store lock is never held while waiting for the user | done | `cli.rs` (the snooze dialog runs before the edit) | by construction; no test |
| a deleted task leaves a tombstone until its session is gone | partial | `tasks.rs` | `tasks::a_tombstone_blocks_adoption_until_its_session_is_gone`; delete is missing |
| a wait is marked notified only after the store update; forgotten when unblocked; kept for sessions not polled | done | `daemon.rs` `poll_once` | `daemon::a_failed_store_update_reports_nothing_as_notified`, `daemon::a_wait_is_reported_again_after_the_agent_unblocked`, `daemon::a_session_that_did_not_answer_keeps_its_waits` |
| a missing `herdr` (or `git`) is a clear error; `itm attach` checks before saving; a repeated daemon error is logged once; the switcher says "Herdr not answering" | done | `herdr.rs` `missing_herdr`, `switching.rs`, `daemon.rs` `keep_polling`, `tui/draw.rs` | `switching::a_task_is_not_saved_when_switching_to_it_cannot_work`, `daemon::a_failed_or_panicking_pass_never_stops_the_daemon_and_is_logged_once`, `tui::draw::unknown_waits_and_sessions_are_said_not_hidden`. No `git` yet |
| every Herdr query and notification has a timeout (the connection included); one failed pass, even a panic, never stops the daemon | done | `herdr.rs` `connect`, `system.rs` `output_within`, `daemon.rs` | `herdr::a_session_that_never_accepts_times_out_instead_of_hanging`, `herdr::a_session_that_never_answers_times_out`, `system::a_command_runs_to_its_end_or_is_killed_at_the_timeout`, `daemon::a_failed_or_panicking_pass_never_stops_the_daemon_and_is_logged_once` |
| a status cache older than 15 s is ignored, and the UI says the daemon is not running | done | `status.rs`, `tui/draw.rs` | `status::a_stale_or_missing_cache_is_unknown`, `tui::draw::unknown_waits_and_sessions_are_said_not_hidden` |
| snooze times in the future and at most 10 years away, for every caller | done | `tasks.rs` `Task::snooze`, `snooze.rs` | `tasks::snoozing_until_an_agent_has_no_timer_and_times_must_be_ahead`, `snooze::invalid_phrases` |
| a snooze until an agent can always end | done | `itm.rs`, `daemon.rs` | see R4.5 |
| a failed UI refresh still ends the refresh | done | `tui/mod.rs` `in_worker` | `tui::a_failed_or_panicking_refresh_ends_and_the_next_one_comes` |
| without a session list nothing is adopted and no tombstone expires | done | `tui/mod.rs` (`Work::load`), `daemon.rs` (a pass needs the list) | `tui::a_refresh_without_a_session_list_adopts_nothing_and_expires_no_tombstone` |
| short store reads on the UI thread are fine; writes, Herdr and hand-offs in workers | done | `tui/mod.rs` | `tui::a_job_runs_in_a_worker_then_the_list_refreshes` (the Rust switcher loads in a worker too) |

### Projects

| Rule | Status | Where | Pinned by |
|---|---|---|---|
| projects keyed by a random id; shown and matched by name (ignoring case) or id | done | `projects.rs` `resolve_project`, `new_project_id` | `projects::a_folder_is_saved_once_under_a_free_name`, cross-check |
| a task holds a list of project ids; the old single `project` converted when read | done | `store.rs` `upgrade` | `store::a_file_written_by_python_is_read_and_rewritten_as_python_would`, cross-check |
| workspaces on the first start only, under a lock per session, checked again inside; none for a session that has some; no `HERDR_*` but `HERDR_SESSION`; a failure is said and the attach goes on; edits open workspaces in running sessions | partial | `projects.rs`, `herdr.rs` | `projects::a_first_start_opens_a_named_workspace_per_project`, `projects::a_session_that_ran_before_is_not_built_again`, `switching::a_first_start_runs_its_server_in_its_own_session`, `switching::a_failed_first_start_is_said_and_the_attach_goes_on`. Missing: edits (R5.4) |
| itm never closes a workspace | done | (no code does) | |
| two project sources (`found_projects`) | missing | | R10 |
| a project is `{name, url?, path?, added_at, last_used_at}`; `kind` never stored | done | `store.rs` `Project` | `store::a_file_written_by_python_is_read_and_rewritten_as_python_would`, cross-check (old format) |
| code never assumes a folder; the TUI keeps the missing folders; a name-only project is never missing | done | `store.rs` (`Option`), `projects.rs` `missing_folders`, `tui/draw.rs` | `tui::draw::a_task_that_never_ran_starts_in_its_first_project_with_a_folder` |
| a folder belongs to at most one project, nesting allowed; `set` only without a repository | partial | `projects.rs` | `projects::a_folder_is_saved_once_under_a_free_name`. Missing: `set` |
| a project without a folder gets no workspace; found only by a workspace's name; renames say who loses it | partial | `projects.rs` `task_folders` | `projects::a_session_starts_in_its_first_project_with_a_folder`. Missing: the workspace and rename parts |
| the workspace hotkey's rules | missing | | R10.5 |
| the finder's "+ New project" row | missing | | R1.6 |
| workspace sync (`poll_once`) and its trusted passes | missing | | R10.3 |
| `workspace_projects` only while the daemon reads the session; dropped on archive | partial | `daemon.rs`, `tasks.rs` `Task::archive` | as with sync off; no test of its own |
| a label is a name only when it is not a folder's name | missing | | R10.2 |
| a workspace in a repository's worktree or checkout | missing | | R10.2 |
| only saved projects matched; the home folder is none | missing | | R10.2 |
| a hand edit keeps a workspace's project while sync runs | missing | | R5.4, R10 |
| `add-workspace` refuses the home folder | missing | | R10.5 |
| adding and removing hold `.projects.lock`; a clone outside the store lock | partial | `projects.rs` `add_folder_project` | no test of the lock; no clones |
| names unique ignoring case; a taken name names its project; not inside the projects folder | partial | `projects.rs`, `error.rs` | `projects::a_folder_is_saved_once_under_a_free_name`, `projects::what_cannot_be_a_project`, cross-check. Missing: the add dialog |
| clones go to a hidden folder, cleaned up on failure and signals | missing | | R8.6 |
| a clone from a TUI runs as a child `itm project add` that never prompts | missing | | R8.8 |
| removing a project, and what may be deleted | missing | | R8.7 |
| duplicates by resolved path, and by normalized URL | partial | `projects.rs` | path: `projects::a_folder_is_saved_once_under_a_free_name`; URLs missing |

### Notes

| Rule | Status | Where | Pinned by |
|---|---|---|---|
| keyed by session name; deleting a task deletes them | partial | `notes.rs` | delete is missing |
| `notes_path` refuses names that are not one path segment | done | `notes.rs` | `notes::names_that_are_not_one_path_segment_have_no_notes` |
| written after the store, never under its lock; a failure never undoes the change | partial | `itm.rs` `create_task` | create: by construction, no test; edits missing |
| started by a temp file and a hard link (exclusive create without links); appends one write each | done | `notes.rs` `ensure_notes`, `append_entry` | `notes::notes_start_with_the_task_and_are_never_overwritten`, `notes::notes_are_appended_after_one_blank_line` |
| only real changes appended; the daemon never writes notes | partial | | the daemon writes none; edits missing |
| an editor saving over an append drops it (accepted) | n/a | | |

### Settings

| Rule | Status | Where | Pinned by |
|---|---|---|---|
| `config.toml` read on every use; unknown settings are errors | missing | | R10.4 |
| `set_config` changes one line and checks the result | missing | | |
| a broken file never stops the daemon | missing | | |
| `.config.lock`, writes through a symlink | missing | | |

### Notifications

| Rule | Status | Where | Pinned by |
|---|---|---|---|
| Herdr toasts the attached session; the daemon notifies only for others; toast, else desktop (command, osascript, notify-send), else a log line; only notify-send offers Open | done | `daemon.rs`, `system.rs` | `daemon::an_attached_session_is_left_to_herdr`, `system::each_system_notifies_with_its_own_program` |
| a blocked agent in a snoozed task notifies and wakes it | done | `daemon.rs` | `daemon::notifies_wakes_and_reports_a_wait_once` |

## Known differences from the Python itm

- Herdr is reached over its socket API, not the CLI, and a session is
  stopped with `server.stop` (DESIGN.md section 4, 2026-11-06).
- `itm list` writes `tasks.json` only when it adopts a session or expires
  a tombstone; Python writes on every list (the content is the same).
- Project names refuse only control characters (Unicode Cc); Python
  refuses all of category C (format characters such as zero-width spaces
  too).
- `~user` in `ITM_PROJECTS_DIR` is not expanded (Python's `expanduser`
  does).
- The switcher has no help screen (its seven keys fit the footer), no
  `@project` filter, no notes in the preview and no Ctrl+O.
