//! The `itm` command line: arguments, and what each command prints.

use std::io::IsTerminal;
use std::sync::Arc;

use clap::{ArgGroup, Parser, Subcommand, ValueEnum};
use serde::Serialize;

use crate::error::{Context, Error, Result, bail};
use crate::herdr::Sessions;
use crate::itm::Itm;
use crate::projects::{CloneChoice, CloneState, RemovalNote, project_rows};
use crate::snooze::Until;
use crate::status::Status;
use crate::store::{Data, ProjectKind, State, python_json};
use crate::system::which;
use crate::tasks::{self, Edit, counts, inbox_order, is_task_session};
use crate::{times, tui};

const STATUS_FORMAT: &str = "⚑ {waiting} ● {active} ◔ {snoozed}";

#[derive(Parser)]
#[command(
    name = "itm",
    version,
    about = "Inbox-style tasks on Herdr sessions. Without a command, opens the task switcher."
)]
pub struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// The task switcher (the default command)
    Ui {
        #[arg(long, value_enum, default_value_t = tui::View::Inbox)]
        view: tui::View,
    },
    /// Attach (outside Herdr) or switch (inside) to a task or session
    Attach {
        #[arg(default_value = tasks::DEFAULT_SESSION)]
        target: String,
    },
    /// Create a task and switch to it
    New {
        title: Vec<String>,
        #[arg(short, long, allow_hyphen_values = true)]
        description: Option<String>,
        /// A saved project; repeat for several, the first is where the session starts
        #[arg(short = 'p', long = "project", value_name = "PROJECT")]
        projects: Vec<String>,
        /// Create the task snoozed, without switching
        #[arg(long, value_name = "WHEN")]
        snooze: Option<String>,
        #[arg(long)]
        no_switch: bool,
    },
    /// Hide a task until a time: snooze [task] [when]; without a time, a dialog
    Snooze {
        /// The task to snooze, when the first word should not be read as one
        #[arg(long)]
        task: Option<String>,
        /// No timer: wake when an agent needs you
        #[arg(long)]
        until_agent: bool,
        words: Vec<String>,
    },
    /// Wake a snoozed task
    #[command(alias = "unsnooze")]
    Wake { task: Option<String> },
    /// Stop a task's session, keep the task
    Archive { task: Option<String> },
    /// Make an archived task active again and switch to it
    Reopen { task: String },
    /// Delete a task, its session and its notes (asks first)
    Delete {
        task: String,
        /// Do not ask
        #[arg(long)]
        yes: bool,
    },
    /// Change a task's title, description or projects (a running session gets a workspace)
    Edit {
        task: String,
        #[arg(long, allow_hyphen_values = true)]
        title: Option<String>,
        /// An empty value clears it
        #[arg(long, allow_hyphen_values = true)]
        description: Option<String>,
        /// Set the projects; repeat for several, the first is where it starts
        #[arg(long = "project", value_name = "PROJECT", conflicts_with = "no_project")]
        projects: Vec<String>,
        #[arg(long)]
        no_project: bool,
        #[arg(long = "add-project", value_name = "PROJECT")]
        add: Vec<String>,
        #[arg(long = "remove-project", value_name = "PROJECT")]
        remove: Vec<String>,
    },
    /// Task counts for a window title or bar
    Status {
        /// Fields: waiting, active, snoozed, archived, daemon
        #[arg(long, default_value = STATUS_FORMAT)]
        format: String,
        #[arg(long)]
        json: bool,
    },
    /// Tasks for scripts: session, state, title, projects, number of waiting agents
    List {
        #[arg(long, value_enum, default_value_t = ListView::All)]
        view: ListView,
        #[arg(long)]
        json: bool,
    },
    /// Open the task whose agent has waited longest
    Jump,
    /// Go back to the previous task
    Last,
    /// Open a task's notes (notes.md) in your editor; the current task by default
    Notes {
        task: Option<String>,
        /// Print the notes file instead of opening it
        #[arg(long, conflicts_with = "append")]
        path: bool,
        /// Add a dated note without an editor ("-" reads stdin)
        #[arg(long, value_name = "TEXT", allow_hyphen_values = true)]
        append: Option<String>,
    },
    /// Saved git repositories and folders tasks can start in
    #[command(subcommand)]
    Project(ProjectCommand),
    /// Print the current session
    Current,
    /// Poll agents, notify and wake snoozed tasks
    Daemon,
    /// Leave this terminal's session, then run PROGRAM (first starts of headless servers)
    #[command(hide = true)]
    Setsid {
        program: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

#[derive(Subcommand)]
enum ProjectCommand {
    /// Clone and save a git repository, save a folder, or save a name only
    Add {
        #[arg(value_enum)]
        kind: ProjectKind,
        /// Git URL, folder path, or (name) the project's name
        source: String,
        /// Defaults to the repository or folder name
        #[arg(long)]
        name: Option<String>,
    },
    /// Give a project without a repository a folder, move it, or make it a name only
    #[command(group(ArgGroup::new("folder").required(true).args(["dir", "no_dir"])))]
    Set {
        name: String,
        #[arg(long, value_name = "PATH")]
        dir: Option<String>,
        #[arg(long)]
        no_dir: bool,
    },
    /// Saved projects with their tasks and last use
    List {
        #[arg(long)]
        json: bool,
    },
    /// Rename a project; its tasks keep it
    Rename { name: String, new_name: String },
    /// Forget a project no task uses
    Remove {
        name: String,
        /// Delete a git project's clone without asking
        #[arg(long, conflicts_with = "keep_clone")]
        delete_clone: bool,
        #[arg(long)]
        keep_clone: bool,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum ListView {
    Active,
    Snoozed,
    Archived,
    All,
}

pub fn run(cli: Cli, itm: Itm) -> Result<()> {
    let now = times::now();
    match cli.command.unwrap_or(Command::Ui { view: tui::View::Inbox }) {
        Command::Ui { view } => {
            // Switching happens after the switcher gave the terminal back.
            let itm = Arc::new(itm);
            match tui::run(&itm, view)? {
                Some(target) => itm.switch_to(&target),
                None => Ok(()),
            }
        }
        Command::Attach { mut target } => {
            if !is_task_session(&target) && !itm.herdr.sessions()?.contains_key(&target) {
                target = tasks::resolve_task(&itm.store().load()?, &target).unwrap_or(target);
            }
            itm.switch_to(&target)
        }
        Command::New { title, description, projects, snooze, no_switch } => {
            let until = snooze.map(|text| Until::parse(&text, now, &itm.tz)).transpose()?;
            if until == Some(Until::Agent) {
                bail!("a new task has no agents yet, so none could wake it; snooze it until a time");
            }
            let title = title.join(" ");
            if title.trim().is_empty() {
                // Without a title the New task form opens (R1.1), which needs a terminal.
                needs_terminal()?;
                bail!("give the task a title: itm new \"<title>\" (the New task form is not built yet)");
            }
            // A snoozed task is not switched to (R1.7). Check that a switch can
            // work before saving, so a failed one leaves no task.
            let switch = !no_switch && until.is_none();
            if switch && itm.inside_herdr() {
                itm.switchable_session(&itm.herdr.sessions()?)?;
            } else if switch && which("herdr").is_none() {
                return Err(Error::Missing("herdr"));
            }
            let name = itm.create_task(&title, description.as_deref(), &projects, until)?;
            println!("{name}");
            if switch { itm.switch_to(&name) } else { Ok(()) }
        }
        Command::Snooze { task, until_agent, words } => {
            let data = itm.store().load()?;
            let (name, when) = split_snooze_args(&itm, &data, task.as_deref(), &words)?;
            let until = match (until_agent, when) {
                (true, _) => Until::Agent,
                (false, Some(when)) => Until::parse(&when, now, &itm.tz)?,
                // R4.1: without a time, the snooze dialog.
                (false, None) => {
                    needs_terminal()?;
                    let agent = Itm::agent_snooze_problem(&name, &itm.herdr.sessions()?, &itm.status()).is_none();
                    let title = data.tasks.get(&name).map_or(name.as_str(), |t| t.title.as_str());
                    tui::snooze_dialog(&name, title, agent, &itm.tz)?.ok_or(Error::Cancelled)?
                }
            };
            itm.snooze_task(&name, until)?;
            println!("{name} snoozed {}", until.describe(now, &itm.tz));
            Ok(())
        }
        Command::Wake { task } => {
            let name = itm.target_or_current(&itm.store().load()?, task.as_deref())?;
            itm.wake_task(&name)?;
            println!("{name} is active");
            Ok(())
        }
        Command::Archive { task } => {
            let name = itm.target_or_current(&itm.store().load()?, task.as_deref())?;
            itm.archive_task(&name)?;
            println!("{name} archived");
            Ok(())
        }
        Command::Reopen { task } => {
            let name = itm.reopen_task(&task)?;
            itm.switch_to(&name)
        }
        Command::Delete { task, yes } => {
            let data = itm.store().load()?;
            let name = tasks::resolve_task(&data, &task)?;
            // R5.3: after confirmation. No is said by the exit status alone.
            if !yes && !(needs_terminal().and_then(|()| tui::confirm_delete(&name, &data.tasks[&name].title))?) {
                return Err(Error::Exit(1));
            }
            itm.delete_task(&name)?;
            println!("{name} deleted");
            Ok(())
        }
        Command::Edit { task, title, description, projects, no_project, add, remove } => {
            let name = tasks::resolve_task(&itm.store().load()?, &task)?;
            let projects = (no_project || !projects.is_empty()).then_some(projects);
            if projects.is_some() && !(add.is_empty() && remove.is_empty()) {
                bail!("--project and --no-project set the projects; --add-project and --remove-project change them");
            }
            if title.is_none() && description.is_none() && projects.is_none() && add.is_empty() && remove.is_empty() {
                // Without options the edit form opens (R5.4), which needs a terminal.
                needs_terminal()?;
                bail!(
                    "say what to change: --title, --description, --project, --add-project, --remove-project (the edit form is not built yet)"
                );
            }
            let said = itm.edit_task(&name, &Edit { title, description, projects, add, remove, ..Edit::default() })?;
            println!("{name} updated{}", said.iter().map(|s| format!("; {s}")).collect::<String>());
            Ok(())
        }
        Command::Status { format, json } => {
            let status = itm.status();
            let counts = counts(&itm.store().load()?, &status);
            if json {
                #[derive(Serialize)]
                struct Json {
                    #[serde(flatten)]
                    counts: tasks::Counts,
                    daemon: bool,
                }
                print!("{}", python_json(&Json { counts, daemon: status.fresh }, false));
            } else {
                let waiting = if status.fresh { counts.waiting.to_string() } else { "?".into() };
                let fields = [
                    ("waiting", waiting),
                    ("active", counts.active.to_string()),
                    ("snoozed", counts.snoozed.to_string()),
                    ("archived", counts.archived.to_string()),
                    ("daemon", if status.fresh { "True" } else { "False" }.into()),
                ];
                println!("{}", format_fields(&format, &fields)?);
            }
            Ok(())
        }
        Command::List { view, json } => {
            let sessions = itm.herdr.sessions()?;
            let names = sessions.keys().map(String::as_str);
            let mut data = itm.store().load()?;
            if tasks::needs_adoption(&data, names.clone()) {
                data = itm.store().edit(|data| {
                    tasks::adopt_sessions(data, names, now);
                    Ok(data.clone())
                })?;
            }
            let rows = task_rows(&itm, &data, view, &itm.status(), &sessions, now);
            if json {
                print!("{}", python_json(&rows, true));
                return Ok(());
            }
            for row in rows {
                let projects: Vec<&str> = row.projects.iter().map(|p| p.name).collect();
                let (session, state, title, waiting) = (row.session, row.state.name(), row.title, row.waiting);
                println!("{session}\t{state}\t{title}\t{}\t{waiting}", projects.join(","));
            }
            Ok(())
        }
        Command::Jump => {
            let status = itm.status();
            if !status.fresh {
                bail!("the daemon is not running, so waiting agents are unknown (start `itm daemon`; README, Setup)");
            }
            let target = tasks::longest_waiting(&itm.store().load()?, &status, now);
            itm.switch_to(&target.ok_or_else(|| Error::Other("no agent is waiting for you".into()))?)
        }
        Command::Last => {
            let current = if itm.inside_herdr() { itm.current_session(&itm.herdr.sessions()?) } else { None };
            let target = tasks::previous_task(&itm.store().load()?, current.as_deref());
            itm.switch_to(&target.ok_or_else(|| Error::Other("there is no other active task".into()))?)
        }
        Command::Notes { task, path, append } => {
            let data = itm.store().load()?;
            let name = match task {
                Some(task) => tasks::resolve_task(&data, &task)?,
                // A Herdr failure here is said as such; only "not in a task" gets the hint.
                None => match itm.inside_herdr() {
                    true => itm.current_session(&itm.herdr.sessions()?),
                    false => None,
                }
                .filter(|name| data.tasks.contains_key(name))
                .ok_or_else(|| {
                    Error::Other("not inside a task; name the task, or press ctrl+o on it in the switcher".into())
                })?,
            };
            itm.notes_path(&name)?; // a name that cannot have notes says so as it is
            let notes = (itm.ensure_notes(&name, &data.tasks[&name], &data.projects))
                .map_err(|e| Error::Other(format!("cannot create its notes: {e}")))?;
            if path {
                println!("{}", notes.display());
            } else if let Some(text) = append {
                let text = match text.as_str() {
                    "-" => std::io::read_to_string(std::io::stdin()).context(|| "cannot read stdin".into())?,
                    _ => text,
                };
                itm.add_note(&name, &text, now)?;
                println!("note added to {name}");
            } else if terminal() {
                itm.edit_notes(&notes)?;
            } else {
                bail!("the editor needs a terminal; --path prints where the notes are");
            }
            Ok(())
        }
        Command::Project(ProjectCommand::Add { kind, source, name }) => {
            let id = itm.add_project(kind, &source, name.as_deref())?;
            println!("{}", itm.store().load()?.projects[&id].name);
            Ok(())
        }
        Command::Project(ProjectCommand::Set { name, dir, .. }) => {
            println!("{}", itm.set_project_folder(&name, dir.as_deref())?.join("; "));
            Ok(())
        }
        Command::Project(ProjectCommand::Rename { name, new_name }) => {
            let (old, new, note) = itm.rename_project(&name, &new_name)?;
            println!("project {old} renamed to {new}{}", note.map(|n| format!("; {n}")).unwrap_or_default());
            Ok(())
        }
        Command::Project(ProjectCommand::Remove { name, delete_clone, keep_clone }) => {
            let (choice, notes) = match (delete_clone, keep_clone) {
                (true, _) => (CloneChoice::Delete, vec![]),
                (_, true) => (CloneChoice::Keep, vec![]),
                _ => remove_choice(&itm, &name)?,
            };
            for note in itm.remove_project(&name, choice)?.into_iter().chain(notes) {
                match note {
                    RemovalNote::Done(line) => println!("{line}"),
                    RemovalNote::Kept(line) => eprintln!("itm: {line}"),
                }
            }
            Ok(())
        }
        Command::Project(ProjectCommand::List { json }) => {
            let data = itm.store().load()?;
            let rows = project_rows(&data);
            if json {
                print!("{}", python_json(&rows, true));
                return Ok(());
            }
            for row in rows {
                let place = itm.paths.place(&data.projects[row.id]);
                let plural = if row.tasks == 1 { "" } else { "s" };
                println!("{}\t{}\t{place}\t{} task{plural}", row.name, row.kind.name(), row.tasks);
            }
            Ok(())
        }
        Command::Current => {
            println!("{}", itm.current_session(&itm.herdr.sessions()?).unwrap_or_default());
            Ok(())
        }
        Command::Daemon => Arc::new(itm).run_daemon(),
        Command::Setsid { program, args } => Err(crate::system::setsid_exec(&program, &args)),
    }
}

/// Whether a person can answer: a dialog or an editor needs a terminal both ways.
fn terminal() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

/// For a dialog: the error that says to pass options instead, without a terminal.
fn needs_terminal() -> Result<()> {
    match terminal() {
        true => Ok(()),
        false => bail!("this needs a terminal; pass the missing values as options"),
    }
}

/// `project remove` without `--delete-clone` or `--keep-clone` (R8.7): only
/// a clean clone itm may delete is asked about, keeping it by default;
/// nothing is deleted without a yes. What else is kept is said.
fn remove_choice(itm: &Itm, query: &str) -> Result<(CloneChoice, Vec<RemovalNote>)> {
    let plan = itm.removal_plan(query)?;
    let place = plan.project.folder().map(|f| itm.paths.display(f)).unwrap_or_default();
    let kept = |why: &str| Ok((CloneChoice::Keep, vec![RemovalNote::Kept(format!("kept {place}: {why}"))]));
    match &plan.clone {
        Some(CloneState::Foreign) => kept("not a clone itm can delete"),
        Some(CloneState::Deletable { local_work: Some(work) }) => {
            kept(&format!("it has {work}; --delete-clone deletes it anyway"))
        }
        Some(CloneState::Deletable { local_work: None }) if plan.users.is_empty() => match terminal() {
            true => Ok((tui::remove_project_dialog(&plan, &itm.paths)?.ok_or(Error::Cancelled)?, vec![])),
            false => kept("no terminal to ask; --delete-clone deletes it"),
        },
        _ => Ok((CloneChoice::Keep, vec![])),
    }
}

/// `snooze [task] [when…]`: the first word is the task when it names one.
fn split_snooze_args(itm: &Itm, data: &Data, task: Option<&str>, words: &[String]) -> Result<(String, Option<String>)> {
    let joined = |words: &[String]| Some(words.join(" ")).filter(|w| !w.is_empty());
    if task.is_none()
        && let Some(first) = words.first()
        && let Ok(name) = tasks::resolve_task(data, first)
    {
        return Ok((name, joined(&words[1..])));
    }
    Ok((itm.target_or_current(data, task)?, joined(words)))
}

/// One task as `itm list --json` prints it, in the Python itm's key order.
#[derive(Serialize)]
struct Row<'a> {
    session: &'a str,
    title: &'a str,
    description: Option<&'a str>,
    /// The first project, as before tasks had several.
    project: Option<&'a str>,
    project_id: Option<&'a str>,
    projects: Vec<ProjectRef<'a>>,
    state: State,
    waiting: usize,
    waiting_since: Option<i64>,
    running: bool,
    snoozed_until: Option<i64>,
    archived_at: Option<i64>,
    last_used_at: i64,
    notes: Option<String>,
}

#[derive(Serialize)]
struct ProjectRef<'a> {
    id: &'a str,
    name: &'a str,
}

fn task_rows<'a>(
    itm: &Itm,
    data: &'a Data,
    view: ListView,
    status: &Status,
    sessions: &Sessions,
    now: i64,
) -> Vec<Row<'a>> {
    let in_view = |state: State| match view {
        ListView::All => true,
        ListView::Active => state == State::Active,
        ListView::Snoozed => state == State::Snoozed,
        ListView::Archived => state == State::Archived,
    };
    let mut tasks: Vec<_> = data.tasks.iter().filter(|(_, task)| in_view(task.state)).collect();
    tasks.sort_by_key(|(name, task)| inbox_order(status, name, task));
    tasks
        .into_iter()
        .map(|(name, task)| {
            let projects: Vec<ProjectRef> = (task.projects.iter())
                .filter_map(|id| data.projects.get(id).map(|p| ProjectRef { id, name: &p.name }))
                .collect();
            Row {
                session: name,
                title: &task.title,
                description: task.description.as_deref(),
                project: projects.first().map(|p| p.name),
                project_id: projects.first().map(|p| p.id),
                projects,
                state: task.state,
                waiting: status.waiting(name),
                waiting_since: status.waiting_since(name, now),
                running: sessions.get(name.as_str()).is_some_and(|s| s.running),
                snoozed_until: task.snoozed_until,
                archived_at: task.archived_at,
                last_used_at: task.last_used_at,
                notes: itm.notes_path(name).ok().map(|path| path.display().to_string()),
            }
        })
        .collect()
}

/// Python's `str.format` for named fields: `{name}`, with `{{` and `}}` for braces.
fn format_fields(format: &str, fields: &[(&str, String)]) -> Result<String> {
    let bad = |what: &str| -> Result<String> {
        bail!("bad --format: {what}; fields: waiting, active, snoozed, archived, daemon")
    };
    let mut out = String::new();
    let mut rest = format;
    while let Some(at) = rest.find(['{', '}']) {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        if let Some(after) = rest.strip_prefix("{{").or_else(|| rest.strip_prefix("}}")) {
            out.push_str(&rest[..1]);
            rest = after;
        } else if rest.starts_with('}') {
            return bad("single '}'");
        } else {
            let Some(end) = rest.find('}') else { return bad("unclosed '{'") };
            let name = &rest[1..end];
            let Some((_, value)) = fields.iter().find(|(field, _)| *field == name) else {
                return bad(&format!("'{name}'"));
            };
            out.push_str(value);
            rest = &rest[end + 1..];
        }
    }
    out.push_str(rest);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::itm::tests::{itm, write_status};
    use crate::store::Project;

    #[test]
    fn status_formats() {
        let fields = [("waiting", "1".to_string()), ("active", "2".to_string())];
        assert_eq!(format_fields(STATUS_FORMAT.split(" ◔").next().unwrap(), &fields).unwrap(), "⚑ 1 ● 2");
        assert_eq!(format_fields("{waiting}/{active} {{x}}", &fields).unwrap(), "1/2 {x}");
        let err = format_fields("{nope}", &fields).unwrap_err();
        assert_eq!(err.to_string(), "bad --format: 'nope'; fields: waiting, active, snoozed, archived, daemon");
        assert!(format_fields("{waiting", &fields).is_err() && format_fields("}", &fields).is_err());
    }

    #[test]
    fn list_rows_put_waiting_tasks_first_and_name_every_project() {
        let (_tmp, itm) = itm();
        itm.create_task("Old", None, &[], None).unwrap();
        itm.create_task("Fix", Some("d"), &[], None).unwrap();
        itm.create_task("Later", None, &[], Some(Until::Time(times::now() + 600))).unwrap();
        itm.store()
            .edit(|data| {
                let project = |name: &str| Project {
                    name: name.into(),
                    path: Some(format!("/src/{name}")),
                    url: None,
                    added_at: 0,
                    last_used_at: 0,
                    extra: Default::default(),
                };
                data.projects.insert("p1".into(), project("api"));
                data.projects.insert("p2".into(), project("web"));
                data.tasks.get_mut("task-fix").unwrap().projects = vec!["p1".into(), "p2".into(), "gone".into()];
                Ok(())
            })
            .unwrap();
        write_status(&itm, &[("task-old", 2)]);
        let data = itm.store().load().unwrap();
        let rows = task_rows(&itm, &data, ListView::All, &itm.status(), &Sessions::new(), times::now());
        let order: Vec<&str> = rows.iter().map(|r| r.session).collect();
        assert_eq!(order[0], "task-old");
        assert_eq!(order[2], "task-later");
        let fix = &rows[1];
        assert_eq!((fix.project, fix.project_id, fix.projects.len()), (Some("api"), Some("p1"), 2));
        let json = python_json(&rows[1..2], true);
        let keys: Vec<&str> = json.lines().filter_map(|l| l.strip_prefix("    \"")?.split('"').next()).collect();
        assert_eq!(keys[..6], ["session", "title", "description", "project", "project_id", "projects"]);
        assert!(json.contains("\"notes\": \"") && json.ends_with("]\n"));
        let active = task_rows(&itm, &data, ListView::Snoozed, &itm.status(), &Sessions::new(), times::now());
        assert_eq!(active.iter().map(|r| r.session).collect::<Vec<_>>(), ["task-later"]);
    }

    #[test]
    fn the_first_snooze_word_is_the_task_when_it_names_one() {
        let (_tmp, itm) = itm();
        itm.create_task("Old", None, &[], None).unwrap();
        let data = itm.store().load().unwrap();
        let words = |w: &[&str]| w.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let split = |task, w: &[&str]| split_snooze_args(&itm, &data, task, &words(w));
        assert_eq!(split(None, &["old", "30m"]).unwrap(), ("task-old".into(), Some("30m".into())));
        assert_eq!(split(None, &["old"]).unwrap(), ("task-old".into(), None));
        assert_eq!(split(Some("old"), &["2h"]).unwrap(), ("task-old".into(), Some("2h".into())));
        assert_eq!(split(None, &["1h", "30m"]).unwrap_err().to_string(), "not inside a task; name the task");
    }
}
