"""Inbox-style task manager on top of Herdr named sessions.

Every task is a Herdr session named ``task-<slug>``. See REQUIREMENTS.md
for the requirements and DESIGN.md for the rules this code must keep. The
interactive screens live in ``inbox_task_manager.tui`` and are imported
only when a command needs them, so the CLI and the daemon stay fast. What
differs between operating systems lives in ``inbox_task_manager.system``.
"""

from __future__ import annotations

import argparse
import contextlib
import fcntl
import json
import os
import re
import secrets
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
import unicodedata
import urllib.parse
from datetime import date, datetime, timedelta
from pathlib import Path

from . import system

PREFIX = "task-"
DEFAULT_SESSION = "default"
VIEWS = ("active", "snoozed", "archived", "all")
TUI_VIEWS = ("inbox", "snoozed", "archived", "all")
POLL_SECONDS = 2.0
STATUS_MAX_AGE = 15
HERDR_TIMEOUT = 10
MAX_SNOOZE = 10 * 365 * 86400
# Snooze value for "until an agent needs me": no timer, the daemon wakes the
# task when one of its agents is blocked.
UNTIL_AGENT = "agent"


class ItmError(Exception):
    """A user-facing error."""


class Cancelled(ItmError):
    """The user dismissed a prompt with Esc."""


class Aborted(Cancelled):
    """The user pressed Ctrl+C in a prompt."""


def screens():
    """The interactive screens (the ``tui`` module, Textual), imported on first use."""
    if not (sys.stdin.isatty() and sys.stdout.isatty()):
        raise ItmError("this needs a terminal; pass the missing values as options")
    import importlib

    return importlib.import_module("inbox_task_manager.tui")


@contextlib.contextmanager
def needs(program: str):
    """Turn a missing ``program`` into a user-facing error (pip installs bring no herdr or git)."""
    try:
        yield
    except FileNotFoundError as exc:
        if exc.filename != program:
            raise
        raise ItmError(f"{program} not found; install it or add it to PATH") from None


# --------------------------------------------------------------------------
# Paths and time


def state_dir() -> Path:
    base = os.environ.get("XDG_STATE_HOME") or str(Path.home() / ".local" / "state")
    return Path(base) / "inbox-task-manager"


def data_dir() -> Path:
    base = os.environ.get("XDG_DATA_HOME") or str(Path.home() / ".local" / "share")
    return Path(base) / "inbox-task-manager"


def projects_dir() -> Path:
    return Path(os.environ.get("ITM_PROJECTS_DIR") or data_dir() / "projects").expanduser().absolute()


def runtime_dir() -> Path:
    try:
        return system.user_runtime_dir("inbox-task-manager")
    except OSError as exc:
        raise ItmError(f"no private runtime folder: {exc}; set $XDG_RUNTIME_DIR") from exc


def clients_dir() -> Path:
    return runtime_dir() / "clients"


def task_dir() -> Path:
    return Path(os.environ.get("ITM_TASK_DIR") or Path.home())


def now() -> int:
    return int(time.time())


def display_path(path: str | Path) -> str:
    home = str(Path.home())
    text = str(path)
    return "~" + text[len(home):] if text == home or text.startswith(home + "/") else text


def fmt_time(ts: int | None) -> str:
    if not ts:
        return "-"
    try:
        when = datetime.fromtimestamp(ts)
    except (OverflowError, OSError, ValueError):
        return "?"
    if when.date() == datetime.now().date():
        return when.strftime("%H:%M")
    return when.strftime("%Y-%m-%d %H:%M")


@contextlib.contextmanager
def flocked(path: Path):
    path.parent.mkdir(parents=True, exist_ok=True)
    with open(path, "w") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        yield


def write_atomic(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp = tempfile.mkstemp(dir=path.parent, prefix=f".{path.name}.")
    try:
        with os.fdopen(fd, "w") as handle:
            handle.write(text)
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(tmp, path)
    except BaseException:
        with contextlib.suppress(FileNotFoundError):
            os.unlink(tmp)
        raise


# --------------------------------------------------------------------------
# Names and durations


def slugify(title: str) -> str:
    text = unicodedata.normalize("NFKD", title)
    text = "".join(ch for ch in text if not unicodedata.combining(ch))
    slug = re.sub(r"[^a-z0-9]+", "-", text.lower()).strip("-")
    return slug[:40].rstrip("-") or "task"


def unique_session_name(title: str, taken: set[str]) -> str:
    base = PREFIX + slugify(title)
    taken_lower = {name.lower() for name in taken}
    name, suffix = base, 2
    while name.lower() in taken_lower:
        name = f"{base}-{suffix}"
        suffix += 1
    return name


def is_task_session(name: str) -> bool:
    return name.startswith(PREFIX)


_DURATION = re.compile(r"(?:([0-9]+)d)?(?:([0-9]+)h)?(?:([0-9]+)m)?(?:([0-9]+)s)?")


def _when(text: str, current: int) -> int:
    spec = re.sub(r"\s+", "", text.strip().lower())
    if not spec:
        raise ItmError("empty snooze time")

    if re.fullmatch(r"[0-9]+", spec):
        return current + int(spec) * 60

    match = _DURATION.fullmatch(spec)
    if match and any(match.groups()):
        days, hours, minutes, seconds = (int(g or 0) for g in match.groups())
        return current + days * 86400 + hours * 3600 + minutes * 60 + seconds

    day = _day_and_clock(text, current)
    if day is not None:
        return day

    # Anything else ("next week", "+3 days") goes through GNU date, if there is one.
    gnu_date = system.gnu_date()
    if gnu_date is None:
        raise ItmError(f"cannot understand snooze time: {text}")
    result = subprocess.run(
        [gnu_date, "-d", text.strip(), "+%s"], capture_output=True, text=True, check=False
    )
    if result.returncode != 0 or not re.fullmatch(r"-?[0-9]+", result.stdout.strip()):
        raise ItmError(f"cannot understand snooze time: {text}")
    return int(result.stdout.strip())


_WEEKDAYS = ("monday", "tuesday", "wednesday", "thursday", "friday", "saturday", "sunday")
_DAY_AND_CLOCK = re.compile(
    r"(?:(?P<day>today|tomorrow|(?P<next>next\s+)?(?P<weekday>[a-z]{3,})|(?P<date>[0-9]{4}-[0-9]{2}-[0-9]{2}))(?:\s+|$))?"
    r"(?:(?:at\s+)?(?P<hour>[0-9]{1,2})(?::(?P<minute>[0-9]{2}))?\s*(?P<ampm>am|pm)?)?"
)


def _weekday(word: str) -> int | None:
    """0 for Monday ("mon", "mond", "monday"), …; None for any other word."""
    for index, name in enumerate(_WEEKDAYS):
        if len(word) >= 3 and name.startswith(word):
            return index
    return None


def _day_and_clock(text: str, current: int) -> int | None:
    """``14:30``, ``5pm``, ``tomorrow 9:00``, ``friday 2pm``, ``next mon``, ``2026-12-01 14:30``.

    A clock alone is the next time it shows (tomorrow once it passed today).
    A day is read like GNU ``date -d`` reads it, so none of these needs GNU
    date: a weekday is today when it matches, ``next`` skips today; a day
    without a clock starts at midnight, except ``tomorrow``, which keeps the
    time of day. None when ``text`` is not of this form.
    """
    match = _DAY_AND_CLOCK.fullmatch(text.strip().lower())
    if not match or not (match.group("day") or match.group("hour")):
        return None
    base = datetime.fromtimestamp(current)
    if match.group("date"):
        try:
            when = date.fromisoformat(match.group("date"))
        except ValueError:
            raise ItmError(f"invalid date: {text}") from None
    elif match.group("weekday"):
        weekday = _weekday(match.group("weekday"))
        if weekday is None:
            return None
        ahead = (weekday - base.weekday()) % 7
        if match.group("next") and ahead == 0:
            ahead = 7
        when = base.date() + timedelta(days=ahead)
    else:
        when = base.date() + timedelta(days=1 if match.group("day") == "tomorrow" else 0)
    if match.group("hour") is None:
        clock = base.time().replace(microsecond=0) if match.group("day") == "tomorrow" else datetime.min.time()
    else:
        hour, minute = int(match.group("hour")), int(match.group("minute") or 0)
        if match.group("ampm"):
            if not 1 <= hour <= 12:
                raise ItmError(f"invalid time: {text}")
            hour = hour % 12 + (12 if match.group("ampm") == "pm" else 0)
        if hour > 23 or minute > 59:
            raise ItmError(f"invalid time: {text}")
        clock = datetime.min.time().replace(hour=hour, minute=minute)
    try:
        target = datetime.combine(when, clock)
        if not match.group("day") and target.timestamp() <= current:
            target += timedelta(days=1)
        return int(target.timestamp())
    except (OverflowError, OSError, ValueError):
        raise ItmError(f"invalid date: {text}") from None


def parse_when(text: str, current: int | None = None) -> int:
    """Return the epoch second a snooze given as ``text`` ends."""
    current = now() if current is None else current
    when = _when(text, current)
    if when <= current:
        raise ItmError(f"snooze time must be in the future: {text}")
    if when - current > MAX_SNOOZE:
        raise ItmError(f"snooze time is too far away: {text}")
    return when


# --------------------------------------------------------------------------
# Task store


class Store:
    """``tasks.json`` guarded by an exclusive flock and written atomically.

    ``tasks`` maps session name to task metadata; ``deleted`` holds tombstones
    for deleted tasks whose session is still being stopped, so they are not
    adopted back; ``projects`` maps a project id to a saved, named git
    repository or folder (tasks refer to projects by id).
    """

    def __init__(self, path: Path | None = None) -> None:
        self.path = path or state_dir() / "tasks.json"

    def _locked(self):
        return flocked(self.path.parent / ".tasks.lock")

    def projects_lock(self):
        """Serializes adding and removing projects, which clone and delete
        outside the store lock."""
        return flocked(self.path.parent / ".projects.lock")

    def _read(self) -> dict:
        try:
            data = json.loads(self.path.read_text())
        except FileNotFoundError:
            data = {"version": 1}
        except json.JSONDecodeError as exc:
            raise ItmError(f"{self.path} is not valid JSON: {exc}") from exc
        data.setdefault("tasks", {})
        data.setdefault("deleted", {})
        data.setdefault("projects", {})
        return data

    def load(self) -> dict:
        with self._locked():
            return self._read()

    @contextlib.contextmanager
    def edit(self):
        with self._locked():
            data = self._read()
            yield data
            write_atomic(self.path, json.dumps(data, indent=2, sort_keys=True) + "\n")


def new_task(
    title: str,
    created: int | None = None,
    description: str | None = None,
    project: str | None = None,
) -> dict:
    created = now() if created is None else created
    task = {"title": title, "state": "active", "created_at": created, "last_used_at": created}
    if description:
        task["description"] = description
    if project:
        task["project"] = project
    return task


def activate(task: dict) -> None:
    task["state"] = "active"
    task.pop("snoozed_until", None)
    task.pop("archived_at", None)


def adopt_sessions(data: dict, sessions: dict[str, dict]) -> None:
    """Track prefixed Herdr sessions created outside itm; expire tombstones."""
    deleted = data.setdefault("deleted", {})
    for name in [n for n in deleted if n not in sessions]:
        del deleted[name]
    for name in sessions:
        if is_task_session(name) and name not in data["tasks"] and name not in deleted:
            data["tasks"][name] = new_task(name[len(PREFIX):].replace("-", " "))


# --------------------------------------------------------------------------
# Task notes


def notes_path(name: str) -> Path:
    """A task's notes, ``tasks/<session>/notes.md`` in itm's data folder.

    Keyed by session name, which a task keeps for good (a new title keeps it).
    """
    if not name or "/" in name or "\0" in name or name in (".", ".."):
        raise ItmError(f"cannot keep notes for a session named {name!r}")
    return data_dir() / "tasks" / name / "notes.md"


def notes_stamp(ts: int | None = None) -> str:
    return datetime.fromtimestamp(now() if ts is None else ts).strftime("%Y-%m-%d %H:%M")


def notes_header(name: str, task: dict, projects: dict) -> str:
    project = projects.get(task.get("project") or "")
    meta = [
        f"Created {notes_stamp(task.get('created_at'))}",
        f"project {project['name']}" if project else "no project",
        name,
    ]
    text = f"# {' '.join(task['title'].split())}\n\n*{' · '.join(meta)}*\n"
    if task.get("description"):
        text += f"\n{task['description']}\n"
    return text


def ensure_notes(name: str, task: dict, projects: dict) -> Path:
    """A task's notes file, created from its title and description when missing."""
    path = notes_path(name)
    if path.exists():
        return path
    path.parent.mkdir(parents=True, exist_ok=True)
    # Written in full, then linked into place: notes that exist are never
    # overwritten, and an append racing this never lands before the header.
    header = notes_header(name, task, projects)
    fd, tmp = tempfile.mkstemp(dir=path.parent, prefix=".notes.")
    try:
        with os.fdopen(fd, "w") as handle:
            handle.write(header)
        try:
            os.link(tmp, path)
        except FileExistsError:
            pass
        except OSError:  # a filesystem without hard links: an exclusive create still never overwrites
            with contextlib.suppress(FileExistsError), open(path, "x") as handle:
                handle.write(header)
    finally:
        os.unlink(tmp)
    return path


def notes_taken() -> set[str]:
    """Session names that still have a notes folder (a new task must not inherit it)."""
    try:
        return {entry.name for entry in os.scandir(data_dir() / "tasks") if entry.is_dir()}
    except OSError:
        return set()


def append_notes(name: str, changes: list[tuple[str, str | None]], ts: int | None = None) -> None:
    """Append ``(heading, text)`` entries to a task's notes, after a blank line."""
    stamp = notes_stamp(ts)
    text = "".join(f"## {stamp} · {heading}\n" + (f"\n{body}\n" if body else "") + "\n" for heading, body in changes)
    with open(notes_path(name), "ab+") as handle:
        size = handle.seek(0, os.SEEK_END)
        handle.seek(max(0, size - 2))
        tail = handle.read()
        # One blank line between what is there and the new entry.
        gap = "" if size == 0 or tail.endswith(b"\n\n") else "\n" if tail.endswith(b"\n") else "\n\n"
        handle.write((gap + text).encode())


def add_note(name: str, text: str, ts: int | None = None) -> None:
    """Append a note (``itm notes --append``, e.g. from an agent working in the task)."""
    text = text.strip()
    if not text:
        raise ItmError("the note is empty")
    append_notes(name, [("Note", text)], ts)


def remove_notes(name: str) -> None:
    """Delete a task's notes, and its folder when nothing else is in it."""
    path = notes_path(name)
    with contextlib.suppress(OSError):
        path.unlink()
    with contextlib.suppress(OSError):
        path.parent.rmdir()


def notes_tail(name: str, lines: int = 3) -> tuple[float, list[str]] | None:
    """(modified time, last non-empty lines) of a task's notes, or None without notes."""
    try:
        path = notes_path(name)
        with open(path, "rb") as handle:
            size = handle.seek(0, os.SEEK_END)
            handle.seek(max(0, size - 4096))
            text = handle.read().decode(errors="replace")
            mtime = os.fstat(handle.fileno()).st_mtime
    except (OSError, ItmError):
        return None
    return mtime, [line for line in text.splitlines() if line.strip()][-lines:]


def editor_command(path: Path) -> list[str]:
    """$VISUAL, else $EDITOR (split like a shell would), else a common editor."""
    for var in ("VISUAL", "EDITOR"):
        try:
            words = shlex.split(os.environ.get(var) or "")
        except ValueError as exc:
            raise ItmError(f"cannot read ${var}: {exc}") from exc
        if words:
            return [*words, str(path)]
    for name in ("nvim", "vim", "vi", "nano"):
        if shutil.which(name):
            return [name, str(path)]
    raise ItmError("no editor found; set $EDITOR")


def editor_env() -> dict[str, str]:
    env = os.environ.copy()
    env.pop("ITM_POPUP", None)  # itm run from the editor (:terminal) would delay every error by 2.5 s
    return env


def run_editor(path: Path) -> None:
    """Edit ``path`` in the user's editor, which gets the terminal (no timeout, like attach)."""
    command = editor_command(path)
    try:
        subprocess.run(command, check=False, env=editor_env())
    except OSError as exc:
        raise ItmError(f"could not run {command[0]}: {exc}") from exc


def resolve_task(data: dict, query: str) -> str:
    tasks = data["tasks"]
    if query in tasks:
        return query
    if PREFIX + query in tasks:
        return PREFIX + query
    lowered = query.lower()
    matches = [name for name, task in tasks.items() if task["title"].lower() == lowered]
    if len(matches) == 1:
        return matches[0]
    if len(matches) > 1:
        raise ItmError(f"several tasks are titled '{query}': {', '.join(sorted(matches))}")
    raise ItmError(f"no task named '{query}'")


# --------------------------------------------------------------------------
# Projects


_URL_SCHEME = re.compile(r"[a-z][a-z0-9+.-]*://", re.IGNORECASE)
_SCP_LIKE = re.compile(r"(?:[^@/:]+@)?([^@/:]+):(.+)")
_HOST = re.compile(r"[a-z0-9_][a-z0-9_.-]*")


def _azure_repo(host: str, segments: list[str]) -> tuple[str, list[str]]:
    """Azure DevOps names one repository with several URL shapes (ssh ``v3/``,
    https ``_git/``, legacy ``<org>.visualstudio.com``); map them all to
    ``dev.azure.com/<org>/<project>/<repo>``."""
    if host in ("ssh.dev.azure.com", "vs-ssh.visualstudio.com") and segments[0] == "v3":
        host, segments = "dev.azure.com", segments[1:]
    elif host.endswith(".visualstudio.com"):
        rest = segments[1:] if segments[0].lower() == "defaultcollection" else segments
        host, segments = "dev.azure.com", [host.removesuffix(".visualstudio.com"), *rest]
    if host != "dev.azure.com" or not segments:
        return host, segments
    if "_git" in segments[1:-1]:
        at = segments.index("_git")
        project = segments[1:at] or segments[at + 1 :]  # org/_git/repo: the project is named like the repo
        segments = [segments[0], *project, *segments[at + 1 :]]
    # Organizations are case-insensitive, and legacy URLs carry them in the host.
    return host, [segments[0].lower(), *segments[1:]]


def _valid_segment(segment: str) -> bool:
    return (
        segment not in ("", ".", "..")
        and segment.lower() != ".git"
        and "/" not in segment
        and not any(unicodedata.category(ch).startswith("C") for ch in segment)
    )


def normalize_git_url(url: str) -> str:
    """``host/owner/repo`` for a git URL, the same for every form of one repository."""
    text = url.strip()
    if text.startswith("-") or "::" in text:  # options, git's <transport>::<address>
        raise ItmError(f"not a git URL: {url}")
    if _URL_SCHEME.match(text):
        try:
            parts = urllib.parse.urlsplit(text)
            host, path = parts.hostname or "", parts.path
        except ValueError as exc:
            raise ItmError(f"not a git URL: {url}") from exc
    else:
        match = _SCP_LIKE.fullmatch(text)
        if not match:
            raise ItmError(f"not a git URL: {url}")
        host, path = match.groups()
    host = host.lower()
    segments = [urllib.parse.unquote(s) for s in path.split("/") if s]
    if segments and segments[-1].endswith(".git"):
        segments[-1] = segments[-1][: -len(".git")]
    if segments:
        host, segments = _azure_repo(host, segments)
    if not _HOST.fullmatch(host) or not segments or not all(_valid_segment(s) for s in segments):
        raise ItmError(f"not a git URL: {url}")
    return "/".join([host, *segments])


def repo_key(url: str) -> str:
    """``normalize_git_url`` after git's own rewriting (``url.<base>.insteadOf``)."""
    key = normalize_git_url(url)  # refuse anything else before git sees it
    try:
        result = subprocess.run(
            ["git", "ls-remote", "--get-url", url.strip()], capture_output=True, text=True, check=False, cwd="/"
        )
    except OSError:
        return key
    expanded = result.stdout.strip() if result.returncode == 0 else ""
    return normalize_git_url(expanded) if expanded else key


def project_spec(kind: str, source: str) -> dict:
    """The saved form of a git repository or folder, without its name."""
    source = source.strip()
    if kind == "git":
        return {"kind": "git", "url": source, "path": str(projects_dir() / repo_key(source))}
    if not source:
        raise ItmError("empty folder path")
    try:
        path = Path(source).expanduser()
    except RuntimeError as exc:  # "~someone" for a user that does not exist (or is half typed)
        raise ItmError(f"not a folder: {source}") from exc
    if not path.is_dir():
        raise ItmError(f"not a folder: {source}")
    path = path.resolve()
    root = projects_dir().resolve()
    if path == root or root in path.parents:
        raise ItmError(f"{display_path(path)} is inside itm's projects folder; add its repository with `itm project add git`")
    return {"kind": "dir", "path": str(path)}


def default_project_name(spec: dict) -> str:
    return Path(spec["path"]).name or "project"


def check_project_name(name: str) -> str:
    name = name.strip()
    if not name or len(name) > 60 or any(unicodedata.category(ch).startswith("C") for ch in name):
        raise ItmError("a project name has 1 to 60 characters, without tabs or other control characters")
    return name


def project_where(project: dict) -> str:
    return project.get("url") or display_path(project["path"])


def check_project_free(data: dict, spec: dict, name: str | None = None) -> None:
    """Refuse a repository or folder that is already saved, or a name another project has.

    A repository's clone path is derived from its normalized URL, so comparing
    paths also finds the same repository given by another URL.
    """
    target = os.path.realpath(spec["path"])
    for project in data["projects"].values():
        if os.path.realpath(project["path"]) == target:
            raise ItmError(f"project already exists: {project['name']}")
    if name is not None:
        check_name_free(data, name)


def check_name_free(data: dict, name: str, exclude: str | None = None) -> None:
    """Refuse ``name`` when a project other than ``exclude`` has it, ignoring case."""
    for project_id, project in data["projects"].items():
        if project_id != exclude and project["name"].lower() == name.lower():
            raise ItmError(
                f"project already exists: {project['name']} ({project_where(project)}); choose another name,"
                f" or rename it with `itm project rename {shlex.quote(project['name'])} <name>`"
            )


def resolve_project(data: dict, query: str) -> str:
    """The id of the project named ``query`` (ignoring case), or with that id."""
    projects = data["projects"]
    matches = [project_id for project_id, project in projects.items() if project["name"].lower() == query.lower()]
    if len(matches) == 1:
        return matches[0]
    if query in projects:
        return query
    raise ItmError(f"no project named '{query}'; add it with `itm project add`")


def new_project_id(projects: dict) -> str:
    while True:
        project_id = secrets.token_hex(4)
        if project_id not in projects:
            return project_id


def is_checkout_of(path: Path, url: str) -> bool:
    """Whether ``path`` is a git checkout of ``url``'s repository (not a symlink to one)."""
    if path.is_symlink() or not (path / ".git").exists():
        return False
    with needs("git"):
        result = subprocess.run(
            ["git", "-C", str(path), "remote", "get-url", "origin"],
            capture_output=True,
            text=True,
            check=False,
        )
    try:
        return result.returncode == 0 and normalize_git_url(result.stdout) == repo_key(url)
    except ItmError:
        return False


EXIT_SIGNALS = (signal.SIGHUP, signal.SIGTERM, signal.SIGINT)


def ignore_exit_signals() -> None:
    """Keep cleanup from being interrupted; ``exit_on_signals`` restores the handlers."""
    for sig in EXIT_SIGNALS:
        signal.signal(sig, signal.SIG_IGN)


@contextlib.contextmanager
def exit_on_signals():
    """Turn SIGHUP (a popup closed mid-clone), SIGTERM and SIGINT into SystemExit,
    so cleanup runs; further signals are ignored until the block is left.
    Signals ignored already (``nohup``) stay ignored."""
    def leave(signum, _frame):
        ignore_exit_signals()
        raise SystemExit(128 + signum)

    previous = {sig: signal.getsignal(sig) for sig in EXIT_SIGNALS}
    for sig, handler in previous.items():
        if handler is not signal.SIG_IGN:
            signal.signal(sig, leave)
    try:
        yield
    finally:
        for sig, handler in previous.items():
            signal.signal(sig, handler)


def prune_empty_parents(path: Path) -> None:
    """Remove the empty folders above ``path``, up to the projects folder."""
    root = projects_dir().resolve()
    for parent in path.resolve().parents:
        if root not in parent.parents:
            break
        try:
            parent.rmdir()
        except OSError:
            break


def git_clone(url: str, dest: Path) -> int:
    # --progress: git prints no progress when its output is not a terminal
    # (the TUI reads it from a pipe).
    with needs("git"):
        proc = subprocess.Popen(["git", "clone", "--progress", "--", url, str(dest)])
    try:
        return proc.wait()
    except BaseException:
        # SIGTERM, unlike subprocess.run's SIGKILL, lets git stop its helpers.
        proc.terminate()
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()
        raise


def clone_repo(url: str, target: Path) -> None:
    """Clone ``url`` into ``target``, or keep a checkout of it already there."""
    if target.exists() or target.is_symlink():
        if not is_checkout_of(target, url):
            raise ItmError(f"{display_path(target)} already exists and is not a checkout of {url}")
        return
    with exit_on_signals():
        # Clone into a hidden folder next to the target and rename, so a failed
        # or interrupted clone leaves nothing behind.
        tmp = None
        try:
            target.parent.mkdir(parents=True, exist_ok=True)
            tmp = Path(tempfile.mkdtemp(dir=target.parent, prefix=f".{target.name}."))
            if git_clone(url, tmp / "repo") != 0:
                raise ItmError(f"could not clone {url}")
            os.rename(tmp / "repo", target)
            tmp.rmdir()
        except BaseException:
            ignore_exit_signals()
            if tmp:
                shutil.rmtree(tmp, ignore_errors=True)
            prune_empty_parents(target)
            raise


def add_project(kind: str, source: str, name: str | None = None) -> str:
    """Save a folder, or clone and save a git repository; returns its id."""
    store = Store()
    spec = project_spec(kind, source)
    name = check_project_name((name or "").strip() or default_project_name(spec))
    # Every writer of projects holds this lock, so checking once before the
    # clone is enough; cmd_new only updates last_used_at.
    with store.projects_lock():
        check_project_free(store.load(), spec, name)
        if kind == "git":
            clone_repo(spec["url"], Path(spec["path"]))
        with store.edit() as data:
            added = now()
            project_id = new_project_id(data["projects"])
            data["projects"][project_id] = {"name": name, **spec, "added_at": added, "last_used_at": added}
    return project_id


def deletable_clone(data: dict, project: dict) -> Path | None:
    """A removed git project's clone, if itm may delete it: a real checkout of
    its repository inside the projects folder that holds no other project."""
    path = Path(project["path"])
    if project["kind"] != "git" or not is_checkout_of(path, project["url"]):
        return None
    resolved = path.resolve()
    if projects_dir().resolve() not in resolved.parents:
        return None
    for other in data["projects"].values():
        other_path = Path(other["path"]).resolve()
        if other_path == resolved or resolved in other_path.parents:
            return None
    return path


def local_work(path: Path) -> str | None:
    """What exists only in a checkout (changes, unpushed commits, stashes), if anything."""
    checks = {
        "uncommitted changes": ["status", "--porcelain", "--untracked-files=normal"],
        # HEAD and tags too: commits on a detached HEAD or only tagged are local as well.
        "unpushed commits": ["rev-list", "--ignore-missing", "HEAD", "--branches", "--tags", "--not", "--remotes", "-n", "1"],
        "stashes": ["stash", "list"],
    }
    found = []
    for label, args in checks.items():
        with needs("git"):
            result = subprocess.run(["git", "-C", str(path), *args], capture_output=True, text=True, check=False)
        if result.returncode != 0:
            found.append(f"{label} (git could not tell)")
        elif result.stdout.strip():
            found.append(label)
    return ", ".join(found) or None


def remove_clone(path: Path) -> None:
    """Delete a clone and the host/owner folders it leaves empty."""
    shutil.rmtree(path)
    prune_empty_parents(path)


def projects_by_recent(projects: dict) -> list[tuple[str, dict]]:
    """(id, project) pairs, most recently used first."""
    return sorted(projects.items(), key=lambda p: (-p[1].get("last_used_at", 0), p[1]["name"].lower()))


def self_command() -> list[str]:
    """How to run this program again (for child processes)."""
    if os.environ.get("ITM_SELF"):
        return [os.environ["ITM_SELF"]]
    return [sys.executable, "-m", "inbox_task_manager"]


def clone_env() -> dict[str, str]:
    """Environment for a clone the TUI runs: git and ssh must not prompt.

    The TUI owns the terminal, so a password or host-key prompt would draw
    over it or hang; such a clone fails instead (see ``clone_hint``).
    """
    env = os.environ.copy()
    env.pop("ITM_POPUP", None)  # would delay every error by 2.5 s
    env["GIT_TERMINAL_PROMPT"] = "0"
    env["GIT_SSH_COMMAND"] = f"{env.get('GIT_SSH_COMMAND') or 'ssh'} -o BatchMode=yes"
    return env


_PROMPT_FAILURES = (
    "terminal prompts disabled",
    "could not read username",
    "could not read password",
    "host key verification failed",
    "permission denied",
    "batch mode",
)


def clone_hint(output: str) -> str | None:
    """A next step when a clone failed because git or ssh wanted to ask something."""
    lowered = output.lower()
    if any(text in lowered for text in _PROMPT_FAILURES):
        return "this repository needs a password or a host key confirmation; run `itm project add git <url>` in a terminal"
    return None


def removal_info(query: str) -> dict:
    """What removing a project would do, for a dialog (nothing is changed)."""
    data = Store().load()
    project_id = resolve_project(data, query)
    project = data["projects"][project_id]
    info = {
        "id": project_id,
        "name": project["name"],
        "kind": project["kind"],
        "path": project["path"],
        "url": project.get("url"),
        "users": sorted(t["title"] for t in data["tasks"].values() if t.get("project") == project_id),
        "clone": None,
        "local_work": None,
    }
    if project["kind"] == "git":
        # As remove_project sees it: after this project is gone, other projects may still hold the clone.
        others = {**data, "projects": {k: v for k, v in data["projects"].items() if k != project_id}}
        if not os.path.lexists(project["path"]):
            info["clone"] = "missing"
        elif (clone := deletable_clone(others, project)) is None:
            info["clone"] = "foreign"
        else:
            info["clone"] = "deletable"
            info["local_work"] = local_work(clone)
    return info


def remove_project(query: str, clone: str = "keep") -> list[str]:
    """Forget a project no task uses; returns what happened, line by line.

    ``clone`` decides about a git project's clone: "keep", "delete" (even with
    local work: ``--delete-clone``) or "if-clean" (delete only without local
    work, checked again here, under the lock).
    """
    store = Store()
    messages = []
    with store.projects_lock():
        with store.edit() as data:
            project_id = resolve_project(data, query)
            name = data["projects"][project_id]["name"]
            users = sorted(t["title"] for t in data["tasks"].values() if t.get("project") == project_id)
            if users:
                raise ItmError(
                    f"project '{name}' is used by {len(users)} task(s), delete them first: {', '.join(users)}"
                )
            project = data["projects"].pop(project_id)
        messages.append(f"project {name} removed")
        if project["kind"] != "git" or not os.path.lexists(project["path"]) or clone == "keep":
            return messages
        path = deletable_clone(data, project)
        if path is None:
            messages.append(f"kept {display_path(project['path'])}: not a clone itm can delete")
            return messages
        if clone == "if-clean" and (work := local_work(path)):
            messages.append(f"kept {display_path(path)}: it has {work}; --delete-clone deletes it anyway")
            return messages
        remove_clone(path)
        messages.append(f"deleted {display_path(path)}")
    return messages


def rename_project(query: str, new_name: str) -> tuple[str, str]:
    """Rename a project; its tasks keep it (they store its id). Returns (old, new)."""
    store = Store()
    with store.projects_lock(), store.edit() as data:
        project_id = resolve_project(data, query)
        name = check_project_name(new_name)
        check_name_free(data, name, exclude=project_id)
        project = data["projects"][project_id]
        old, project["name"] = project["name"], name
    return old, name


def looks_like_path(text: str) -> bool:
    """Whether typed text names a folder rather than a project (``~/…``, ``/…``, ``./…``)."""
    return text in (".", "..") or text.startswith(("/", "~", "./", "../"))


def fuzzy_match(query: str, text: str) -> tuple[int, list[int]] | None:
    """(score, matched positions) of ``query``'s letters in order in ``text``, ignoring case; None if absent.

    Linear time, so long names never stall a UI. A prefix ranks first, then a
    substring at a word start, then any substring, then scattered letters
    (word starts and runs of letters help, gaps cost).
    """
    q, t = query.lower(), text.lower()
    if len(q) != len(query) or len(t) != len(text):  # a letter whose lower case is longer (İ): exact case
        q, t = query, text
    if not q:
        return 0, []
    starts = {i for i in range(len(text))
              if i == 0 or not text[i - 1].isalnum() or (text[i - 1].islower() and text[i].isupper())}
    at = t.find(q)
    if at >= 0:
        base = 3000 if at == 0 else 2000 if at in starts else 1000
        return base - min(at, 999), list(range(at, at + len(q)))
    positions, i = [], 0
    for ch in q:
        i = t.find(ch, i)
        if i < 0:
            return None
        positions.append(i)
        i += 1
    hits = sum(p in starts for p in positions)
    runs = sum(b == a + 1 for a, b in zip(positions, positions[1:]))
    gaps = positions[-1] - positions[0] + 1 - len(positions)
    return max(1, min(999, 500 + 20 * hits + 10 * runs - gaps)), positions


def folder_parent(text: str) -> tuple[str, str] | None:
    """(parent as typed, ending in "/", last segment) of a folder path being typed."""
    head, sep, segment = text.rpartition("/")
    return (head + "/", segment) if sep else None


def list_folders(parent: str, scan_limit: int = 5000) -> tuple[str, list[str], dict[str, str]] | None:
    """(resolved folder, names of its subfolders, resolved targets of those that are symlinks)
    for a folder as typed; None when it cannot be read.

    Reads at most ``scan_limit`` entries, so a huge folder (/nix/store) answers
    fast, partially. Can still block on a slow filesystem: the TUI calls it
    from a daemon thread.
    """
    try:
        folder = Path(parent).expanduser()
    except RuntimeError:  # "~someone" for a user that does not exist (or is half typed)
        return None
    names, links = [], {}
    try:
        real = os.path.realpath(folder)
        with os.scandir(folder) as entries:
            for count, entry in enumerate(entries):
                if count >= scan_limit:
                    break
                try:
                    if entry.is_dir():  # follows symlinks
                        names.append(entry.name)
                        if entry.is_symlink():
                            links[entry.name] = os.path.realpath(entry.path)
                except OSError:
                    continue
    except OSError:
        return None
    return real, names, links


def match_folders(parent: str, segment: str, names: list[str], limit: int = 30) -> list[str]:
    """The subfolders matching ``segment``, best first, spelled ``parent`` + name + "/".

    Hidden folders only when the segment starts with ".".
    """
    found = []
    for name in names:
        if name.startswith(".") and not segment.startswith("."):
            continue
        if match := fuzzy_match(segment, name):
            found.append((-match[0], name.casefold(), name))
    found.sort()
    return [f"{parent}{name}/" for *_, name in found[:limit]]


def complete_folders(text: str, limit: int = 30, scan_limit: int = 5000) -> list[str]:
    """Folders completing a path being typed, spelled as typed (``~`` kept), each ending in "/"."""
    if text in ("~", ".", ".."):
        return [text + "/"]
    if not (split := folder_parent(text)):
        return []
    listing = list_folders(split[0], scan_limit)
    return match_folders(*split, listing[1], limit) if listing else []


def task_cwd(data: dict, name: str) -> Path:
    """Where a task's session starts: its project folder, else ``task_dir()``."""
    project = data["projects"].get(data["tasks"].get(name, {}).get("project"))
    if project:
        path = Path(project["path"])
        if path.is_dir():
            return path
        print(f"itm: {display_path(path)} is missing; starting in {display_path(task_dir())}", file=sys.stderr)
    return task_dir()


# --------------------------------------------------------------------------
# Herdr


def herdr_env(socket: str | None = None) -> dict[str, str]:
    env = os.environ.copy()
    if socket:
        env["HERDR_SOCKET_PATH"] = socket
    return env


def herdr_json(args: list[str], socket: str | None = None) -> dict:
    command = f"herdr {' '.join(args)}"
    try:
        with needs("herdr"):
            result = subprocess.run(
                ["herdr", *args],
                env=herdr_env(socket),
                capture_output=True,
                text=True,
                check=False,
                timeout=HERDR_TIMEOUT,
            )
    except subprocess.TimeoutExpired as exc:
        raise ItmError(f"{command} timed out") from exc
    if result.returncode != 0:
        raise ItmError(f"{command} failed: {result.stderr.strip() or result.stdout.strip()}")
    try:
        return json.loads(result.stdout)
    except json.JSONDecodeError as exc:
        raise ItmError(f"{command} returned invalid JSON") from exc


def list_sessions() -> dict[str, dict]:
    return {s["name"]: s for s in herdr_json(["session", "list", "--json"])["sessions"]}


def list_agents(socket: str) -> list[dict] | None:
    """Agents of one session, or None when the session could not be queried."""
    try:
        return herdr_json(["agent", "list"], socket)["result"]["agents"]
    except (ItmError, KeyError, TypeError):
        return None


def inside_herdr() -> bool:
    return bool(os.environ.get("HERDR_SOCKET_PATH"))


def current_session(sessions: dict[str, dict]) -> str | None:
    socket = os.environ.get("HERDR_SOCKET_PATH")
    if not socket:
        return None
    for name, session in sessions.items():
        if session.get("socket_path") == socket:
            return name
    return None


def stop_session_detached(name: str, delete: bool = False) -> None:
    """Stop (and optionally delete) a session without waiting for it.

    Runs in its own process session so it survives when the session being
    stopped is the one hosting this process.
    """
    script = 'herdr session stop "$1" --json >/dev/null 2>&1 || true'
    if delete:
        script += '; herdr session delete "$1" --json >/dev/null 2>&1'
    subprocess.Popen(
        ["/bin/sh", "-c", script, "sh", name],
        start_new_session=True,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )


# --------------------------------------------------------------------------
# Attach loop and in-place switching


def is_itm_process(pid: int) -> bool:
    """Whether ``pid`` is alive and still an itm process (not a reused pid)."""
    argv = system.process_argv(pid)
    return argv is not None and any(
        "inbox-task-manager" in arg or "inbox_task_manager" in arg or os.path.basename(arg) == "itm"
        for arg in argv
    )


def live_clients() -> dict[int, str]:
    """Map of attach-loop pid to the session it is attached to."""
    clients: dict[int, str] = {}
    directory = clients_dir()
    if not directory.is_dir():
        return clients
    for marker in directory.iterdir():
        if not marker.name.isdigit():
            continue
        pid = int(marker.name)
        if not is_itm_process(pid):
            if system.pid_alive(pid) and system.process_argv(pid) is None:
                continue  # alive but unreadable (ps failed): neither a client nor stale
            for stale in (marker, marker.with_name(f"{pid}.next")):
                with contextlib.suppress(FileNotFoundError):
                    stale.unlink()
            continue
        with contextlib.suppress(FileNotFoundError):
            clients[pid] = marker.read_text().strip()
    return clients


def attach_children(parent: int) -> list[int]:
    """``herdr session attach`` processes whose parent is ``parent``."""
    return [
        pid
        for pid, argv in system.child_processes(parent)
        if len(argv) >= 3 and os.path.basename(argv[0]) == "herdr" and argv[1:3] == ["session", "attach"]
    ]


def _marker_mtime(pid: int) -> float:
    try:
        return (clients_dir() / str(pid)).stat().st_mtime
    except OSError:
        return 0.0


def handoff(target: str, from_session: str | None = None, newest_only: bool = False) -> bool:
    """Ask attach loops to move to ``target``; returns whether any did."""
    clients = [(pid, s) for pid, s in live_clients().items() if from_session is None or s == from_session]
    if newest_only and clients:
        clients = [max(clients, key=lambda c: _marker_mtime(c[0]))]
    switched = False
    for pid, _ in clients:
        next_file = clients_dir() / f"{pid}.next"
        write_atomic(next_file, target + "\n")
        # The loop may be between attaches; give it a moment to spawn one.
        children: list[int] = []
        for _ in range(20):
            children = attach_children(pid)
            if children:
                break
            time.sleep(0.1)
        for child in children:
            with contextlib.suppress(ProcessLookupError):
                os.kill(child, signal.SIGTERM)
        if children:
            switched = True
        else:
            with contextlib.suppress(FileNotFoundError):
                next_file.unlink()
    return switched


def touch_task(name: str) -> Path | None:
    """Mark a task used; returns the folder its session starts in."""
    if not is_task_session(name):
        return None
    with Store().edit() as data:
        data["deleted"].pop(name, None)
        task = data["tasks"].setdefault(name, new_task(name[len(PREFIX):].replace("-", " ")))
        activate(task)
        task["last_used_at"] = now()
    return task_cwd(data, name)


def attach_loop(name: str) -> int:
    if not shutil.which("herdr"):  # before touch_task saves a task for nothing
        raise ItmError("herdr not found; install it or add it to PATH")
    marker = clients_dir() / str(os.getpid())
    next_file = marker.with_name(f"{os.getpid()}.next")
    claimed = marker.with_name(f"{os.getpid()}.next.claimed")
    # A hand-off left by an earlier loop that had this pid and crashed.
    with contextlib.suppress(FileNotFoundError):
        next_file.unlink()
    try:
        while name:
            write_atomic(marker, name + "\n")
            # Only the session's first start uses the folder; Herdr restores it after.
            cwd = touch_task(name)
            with needs("herdr"):
                subprocess.run(["herdr", "session", "attach", name], cwd=cwd, check=False)
            # Claim the hand-off with a rename before reading it: one written
            # while the next attach starts must survive until that attach ends.
            try:
                os.replace(next_file, claimed)
                name = claimed.read_text().strip()
                claimed.unlink()
            except FileNotFoundError:
                name = ""
    finally:
        for path in (marker, next_file, claimed):
            with contextlib.suppress(FileNotFoundError):
                path.unlink()
    return 0


def switchable_session() -> str:
    """The current session, if this client can be switched in place."""
    current = current_session(list_sessions())
    if current is None:
        raise ItmError("cannot tell which Herdr session this is")
    if current not in live_clients().values():
        raise ItmError("this Herdr client was not started by `itm attach`; detach and use `itm attach`")
    return current


def switch_to(target: str) -> int:
    """Attach to ``target`` from outside Herdr, or switch in place from inside."""
    if not inside_herdr():
        return attach_loop(target)
    current = switchable_session()
    if target == current:
        touch_task(target)
        return 0
    if not handoff(target, from_session=current):
        raise ItmError(f"could not switch this client to {target}")
    return 0


# --------------------------------------------------------------------------
# Status cache written by the daemon


def status_path() -> Path:
    return runtime_dir() / "status.json"


def read_status() -> tuple[dict[str, dict], bool]:
    """(per-session agent status, whether the daemon updated it lately).

    A stale cache comes back empty, and a UI must say the daemon is not
    running instead of showing nothing waiting.
    """
    try:
        status = json.loads(status_path().read_text())
    except (FileNotFoundError, json.JSONDecodeError):
        return {}, False
    if not isinstance(status, dict) or status.get("updated_at", 0) < now() - STATUS_MAX_AGE:
        return {}, False
    return status.get("sessions", {}), True


def load_status() -> dict[str, dict]:
    """Per-session agent status; empty when the daemon has not updated it lately."""
    return read_status()[0]


def waiting_count(status: dict[str, dict], name: str) -> int:
    return status.get(name, {}).get("blocked", 0)


def waiting_since(status: dict[str, dict], name: str) -> int | None:
    """When the longest wait in a session started (first seen by the daemon)."""
    sinces = [a.get("since") or now() for a in status.get(name, {}).get("agents", []) if a.get("status") == "blocked"]
    return min(sinces) if sinces else None


def longest_waiting(data: dict, status: dict[str, dict]) -> str | None:
    """The task whose agent has waited longest, archived ones excluded."""
    waits = [
        (since, name)
        for name, task in data["tasks"].items()
        if task["state"] != "archived" and (since := waiting_since(status, name)) is not None
    ]
    return min(waits)[1] if waits else None


def previous_task(data: dict, current: str | None) -> str | None:
    """The most recently used active task other than ``current``."""
    candidates = [
        (task.get("last_used_at", 0), name)
        for name, task in data["tasks"].items()
        if task["state"] == "active" and name != current
    ]
    return max(candidates)[1] if candidates else None


def counts(data: dict, status: dict[str, dict]) -> dict[str, int]:
    """Tasks per state, and tasks with an agent waiting for input."""
    tasks = data["tasks"]
    result = {state: sum(t["state"] == state for t in tasks.values()) for state in ("active", "snoozed", "archived")}
    result["waiting"] = sum(1 for name, t in tasks.items() if t["state"] != "archived" and waiting_count(status, name))
    return result


def next_active(data: dict, status: dict[str, dict], exclude: str | None) -> str:
    candidates = [
        (waiting_count(status, name), task.get("last_used_at", 0), name)
        for name, task in data["tasks"].items()
        if task["state"] == "active" and name != exclude
    ]
    return max(candidates)[2] if candidates else DEFAULT_SESSION


# --------------------------------------------------------------------------
# Commands


def target_or_current(data: dict, query: str | None) -> str:
    if query:
        return resolve_task(data, query)
    current = current_session(list_sessions()) if inside_herdr() else None
    if current and current in data["tasks"]:
        return current
    raise ItmError("not inside a task; name the task")


def move_clients_off(name: str, data: dict) -> bool:
    """Switch every itm client attached to ``name`` to the next active task."""
    return handoff(next_active(data, load_status(), exclude=name), from_session=name)


def snooze_until(text: str) -> int | str:
    """A snooze given as text: a time ``parse_when`` understands, or "agent"."""
    if text.strip().lower() in (UNTIL_AGENT, "until an agent needs me"):
        return UNTIL_AGENT
    return parse_when(text)


def apply_snooze(task: dict, until: int | str) -> None:
    """Mark a task snoozed; a time must be in the future (a preset may have passed while a dialog was open)."""
    if until != UNTIL_AGENT and not now() < until <= now() + MAX_SNOOZE:
        raise ItmError(f"snooze time must be in the future: {fmt_time(until)}")
    task["state"] = "snoozed"
    task.pop("archived_at", None)
    if until == UNTIL_AGENT:
        task.pop("snoozed_until", None)  # no timer: only a blocked agent wakes it
    else:
        task["snoozed_until"] = until


def describe_snooze(until: int | str | None) -> str:
    return "until an agent needs you" if until in (None, UNTIL_AGENT) else f"until {fmt_time(until)}"


def agent_snooze_problem(name: str, sessions: dict[str, dict], status: dict[str, dict]) -> str | None:
    """Why "until an agent needs me" would hide ``name`` for good, if it would."""
    if not sessions.get(name, {}).get("running"):
        return "its session is not running, so no agent can ask; snooze it until a time"
    if waiting_count(status, name):
        return "an agent in it is already waiting for you"
    return None


def create_task(
    title: str,
    description: str | None = None,
    project: str | None = None,
    until: int | str | None = None,
) -> str:
    """Save a new task and return its session name; invalid input writes nothing."""
    title = (title or "").strip()
    if not title:
        raise ItmError("task title cannot be empty")
    if until == UNTIL_AGENT:
        raise ItmError("a new task has no agents yet, so none could wake it; snooze it until a time")
    description = (description or "").strip() or None
    sessions = list_sessions()
    with Store().edit() as data:
        if project:
            project = resolve_project(data, project)
            data["projects"][project]["last_used_at"] = now()
        taken = set(sessions) | set(data["tasks"]) | set(data["deleted"]) | notes_taken()
        name = unique_session_name(title, taken)
        task = new_task(title, description=description, project=project)
        if until is not None:
            apply_snooze(task, until)
        data["tasks"][name] = task
    # Best effort: notes missing now are started from the task when first opened or edited.
    with contextlib.suppress(OSError, ItmError):
        ensure_notes(name, task, data["projects"])
    return name


def cmd_new(
    title: str | None,
    switch: bool = True,
    description: str | None = None,
    project: str | None = None,
    snooze: str | None = None,
    interactive: bool = False,
) -> int:
    # Check what was given before asking for the rest.
    until = snooze_until(snooze) if snooze else None
    if until == UNTIL_AGENT:
        raise ItmError("a new task has no agents yet, so none could wake it; snooze it until a time")
    if project:
        project = resolve_project(Store().load(), project)
    title = (title or "").strip()
    ask = interactive or not title
    if ask and not project and inside_herdr():
        # The form starts with the project of the task you are in (R1.6).
        data = Store().load()
        current = data["tasks"].get(current_session(list_sessions()) or "")
        project = current.get("project") if current else None
    cannot_switch = None
    if switch and until is None and inside_herdr():
        try:
            switchable_session()
        except ItmError as exc:
            if not ask:
                raise
            cannot_switch = exc  # the form keeps what was typed and saves without switching
    if ask:
        answers = screens().task_form(
            title=title,
            description=description,
            project=project,
            until=until,
            cannot_switch=str(cannot_switch) if cannot_switch else None,
        )
        if answers is None:
            raise Cancelled("cancelled")
        title, description, project, until = (answers[k] for k in ("title", "description", "project", "until"))
    name = create_task(title, description, project, until)
    print(name)
    if not switch or until is not None:
        return 0
    if cannot_switch:
        raise ItmError(f"{name} saved without switching: {cannot_switch}")
    return switch_to(name)


def split_snooze_args(data: dict, task: str | None, words: list[str]) -> tuple[str, str | None]:
    """``snooze [task] [when…]``: the first word is the task when it names one."""
    if task is None and words:
        with contextlib.suppress(ItmError):
            return resolve_task(data, words[0]), " ".join(words[1:]) or None
    return target_or_current(data, task), " ".join(words) or None


def snooze_task(name: str, until: int | str) -> None:
    """Snooze a task and move every itm client on it to the next task."""
    if until == UNTIL_AGENT and (problem := agent_snooze_problem(name, list_sessions(), load_status())):
        raise ItmError(f"cannot snooze until an agent needs you: {problem}")
    with Store().edit() as data:
        if name not in data["tasks"]:
            raise ItmError(f"task {name} no longer exists")
        apply_snooze(data["tasks"][name], until)
    move_clients_off(name, data)


def cmd_snooze(task: str | None, words: list[str], until_agent: bool = False) -> int:
    data = Store().load()
    name, when_text = split_snooze_args(data, task, words)
    if until_agent:
        until = UNTIL_AGENT
    elif when_text:
        until = snooze_until(when_text)
    else:
        problem = agent_snooze_problem(name, list_sessions(), load_status())
        until = screens().snooze_dialog(data["tasks"][name]["title"], agent_problem=problem)
        if until is None:
            raise Cancelled("cancelled")
    snooze_task(name, until)
    print(f"{name} snoozed {describe_snooze(until)}")
    return 0


def wake_task(name: str) -> None:
    with Store().edit() as data:
        task = data["tasks"].get(name)
        if task is None:
            raise ItmError(f"task {name} no longer exists")
        if task["state"] != "snoozed":
            raise ItmError(f"{name} is not snoozed")
        activate(task)


def cmd_wake(query: str | None) -> int:
    name = target_or_current(Store().load(), query)
    wake_task(name)
    print(f"{name} is active")
    return 0


def archive_task(name: str) -> None:
    """Archive a task: move itm clients off it, then stop its session."""
    with Store().edit() as data:
        task = data["tasks"].get(name)
        if task is None:
            raise ItmError(f"task {name} no longer exists")
        task["state"] = "archived"
        task["archived_at"] = now()
        task.pop("snoozed_until", None)
    move_clients_off(name, data)
    stop_session_detached(name)


def cmd_archive(query: str | None) -> int:
    name = target_or_current(Store().load(), query)
    archive_task(name)
    print(f"{name} archived")
    return 0


def cmd_reopen(query: str) -> int:
    with Store().edit() as data:
        name = resolve_task(data, query)
        activate(data["tasks"][name])
    return switch_to(name)


def delete_task(name: str) -> None:
    """Delete a task for good, its notes too (a later task may get its session name); its project stays."""
    with Store().edit() as data:
        if name not in data["tasks"]:
            raise ItmError(f"task {name} no longer exists")
        data["tasks"].pop(name)
        data["deleted"][name] = now()
    with contextlib.suppress(ItmError):  # a name that cannot have notes has none
        remove_notes(name)
    move_clients_off(name, data)
    stop_session_detached(name, delete=True)


def cmd_delete(query: str, yes: bool) -> int:
    data = Store().load()
    name = resolve_task(data, query)
    if not yes and not screens().confirm_delete(data["tasks"][name]["title"]):
        return 1
    delete_task(name)
    print(f"{name} deleted")
    return 0


UNSET = object()


def project_change_effect(name: str, sessions: dict[str, dict]) -> str:
    """What giving a task another project does to its session.

    "starts": the session never started, so it will start in the project's folder.
    "tab": the session runs; it cannot move, so itm opens a tab in the folder.
    "stopped": the session is stopped and keeps its saved folders.
    """
    if name not in sessions:
        return "starts"
    return "tab" if sessions[name].get("running") else "stopped"


def open_project_tab(name: str, project: dict, sessions: dict[str, dict], focus: bool = False) -> None:
    """Open a tab in a project's folder inside the task's running session."""
    herdr_json(
        ["tab", "create", "--cwd", project["path"], "--label", project["name"], "--focus" if focus else "--no-focus"],
        sessions[name]["socket_path"],
    )


def edit_task(name: str, title: str | None = None, description: str | None = None, project=UNSET) -> list[str]:
    """Change a task's title, description (empty clears it) or project; returns what that did.

    A session starts in its project's folder only once, so a new project for
    a running session opens a tab in the project's folder instead. What really
    changed is appended to the task's notes.
    """
    added = None
    changes: list[tuple[str, str | None]] = []
    with Store().edit() as data:
        task = data["tasks"].get(name)
        if task is None:
            raise ItmError(f"task {name} no longer exists")
        before = dict(task)
        if title is not None:
            if not title.strip():
                raise ItmError("task title cannot be empty")
            if title.strip() != task["title"]:
                changes.append(("Title changed", title.strip()))
            task["title"] = title.strip()
        if description is not None:
            if description.strip() != task.get("description", ""):
                changes.append(("Description changed", description.strip()) if description.strip()
                               else ("Description cleared", None))
            if description.strip():
                task["description"] = description.strip()
            else:
                task.pop("description", None)
        # Compare ids: --project takes a name (any case) or an id.
        project_id = resolve_project(data, project) if project not in (UNSET, None, "") else None
        if project is not UNSET and project_id != task.get("project"):
            if project_id:
                task["project"] = project_id
                data["projects"][project_id]["last_used_at"] = now()
                added = dict(data["projects"][project_id])
                changes.append(("Project changed", f"{added['name']} ({display_path(added['path'])})"))
            else:
                task.pop("project", None)
                changes.append(("Project removed", None))
    # The edit is saved; what follows only reports, writes notes or opens a
    # tab, so a failure there must not fail it.
    notes_failed = []
    if changes:  # before the Herdr calls below, which can take until their timeouts
        try:
            ensure_notes(name, before, data["projects"])  # a task without notes yet: start them as it was
            append_notes(name, changes)
        except (OSError, ItmError) as exc:
            notes_failed.append(f"could not update its notes: {exc}")
    return ([project_change_message(name, added)] if added is not None else []) + notes_failed


def project_change_message(name: str, added: dict) -> str:
    """Say what a task's new project means for its session, opening a tab in a running one."""
    where = display_path(added["path"])
    if not Path(added["path"]).is_dir():
        return f"{where} is missing; the session starts in {display_path(task_dir())} instead, and no tab was opened"
    try:
        sessions = list_sessions()
    except ItmError as exc:
        return f"could not tell whether its session runs ({exc}); open a tab in {where} if it does"
    effect = project_change_effect(name, sessions)
    if effect == "starts":
        return f"its session will start in {where}"
    if effect == "stopped":
        return f"its stopped session keeps its folders; open a tab in {where} when you need it"
    focus = inside_herdr() and current_session(sessions) == name
    try:
        open_project_tab(name, added, sessions, focus=focus)
    except ItmError as exc:
        return f"could not open a tab in {where}: {exc}"
    return f"opened a tab in {where}"


def cmd_edit(query: str, title: str | None, description: str | None, project) -> int:
    name = resolve_task(Store().load(), query)
    if title is None and description is None and project is UNSET:
        answers = screens().edit_form(name)
        if answers is None:
            raise Cancelled("cancelled")
        title, description, project = answers["title"], answers["description"], answers.get("project", UNSET)
    notes = edit_task(name, title, description, project)
    print(f"{name} updated" + "".join(f"; {note}" for note in notes))
    return 0


def _notes_or_none(name: str) -> Path | None:
    try:
        return notes_path(name)
    except ItmError:
        return None


def task_rows(data: dict, view: str, status: dict[str, dict], sessions: dict[str, dict]) -> list[dict]:
    rows = []
    projects = data.get("projects", {})
    for name, task in data["tasks"].items():
        if view != "all" and task["state"] != view:
            continue
        rows.append(
            {
                "session": name,
                "title": task["title"],
                "description": task.get("description"),
                "project": projects.get(task.get("project"), {}).get("name"),
                "project_id": task.get("project"),
                "state": task["state"],
                "waiting": waiting_count(status, name),
                "waiting_since": waiting_since(status, name),
                "running": sessions.get(name, {}).get("running", False),
                "snoozed_until": task.get("snoozed_until"),
                "archived_at": task.get("archived_at"),
                "last_used_at": task.get("last_used_at", 0),
                "notes": str(path) if (path := _notes_or_none(name)) else None,
            }
        )
    order = {"active": 0, "snoozed": 1, "archived": 2}
    rows.sort(key=lambda r: (r["waiting"] == 0, order[r["state"]], -r["last_used_at"]))
    return rows


def cmd_list(view: str, as_json: bool) -> int:
    sessions = list_sessions()
    with Store().edit() as data:
        adopt_sessions(data, sessions)
    rows = task_rows(data, view, load_status(), sessions)
    if as_json:
        print(json.dumps(rows, indent=2))
    else:
        for row in rows:
            print("\t".join([row["session"], row["state"], row["title"], row["project"] or "", str(row["waiting"])]))
    return 0


def cmd_notes(query: str | None, path_only: bool = False, append: str | None = None) -> int:
    """Open a task's notes in the editor; ``--path`` prints where they are, ``--append`` adds a note."""
    data = Store().load()
    if query:
        name = resolve_task(data, query)
    else:
        # A Herdr failure here says so; only "not in a task" gets the hint.
        name = current_session(list_sessions()) if inside_herdr() else None
        if name not in data["tasks"]:
            raise ItmError("not inside a task; name the task, or press ctrl+o on it in the switcher")
    try:
        path = ensure_notes(name, data["tasks"][name], data["projects"])
    except OSError as exc:
        raise ItmError(f"cannot create its notes: {exc}") from exc
    if path_only:
        print(path)
        return 0
    if append is not None:
        try:
            add_note(name, sys.stdin.read() if append == "-" else append)
        except OSError as exc:
            raise ItmError(f"cannot write its notes: {exc}") from exc
        print(f"note added to {name}")
        return 0
    if not (sys.stdin.isatty() and sys.stdout.isatty()):
        raise ItmError("the editor needs a terminal; --path prints where the notes are")
    run_editor(path)
    return 0


def cmd_jump() -> int:
    """Open the task whose agent has waited longest."""
    status, fresh = read_status()
    if not fresh:
        raise ItmError("the daemon is not running, so waiting agents are unknown (start `itm daemon`; README, Setup)")
    target = longest_waiting(Store().load(), status)
    if target is None:
        raise ItmError("no agent is waiting for you")
    return switch_to(target)


def cmd_last() -> int:
    """Go back to the most recently used other active task."""
    current = current_session(list_sessions()) if inside_herdr() else None
    target = previous_task(Store().load(), current)
    if target is None:
        raise ItmError("there is no other active task")
    return switch_to(target)


STATUS_FORMAT = "⚑ {waiting} ● {active} ◔ {snoozed}"


def cmd_status(fmt: str, as_json: bool) -> int:
    status, fresh = read_status()
    result: dict = {**counts(Store().load(), status), "daemon": fresh}
    if as_json:
        print(json.dumps(result))
        return 0
    if not fresh:
        result["waiting"] = "?"
    try:
        print(fmt.format(**result))
    except (KeyError, IndexError, ValueError) as exc:
        raise ItmError(f"bad --format: {exc}; fields: waiting, active, snoozed, archived, daemon") from exc
    return 0


def cmd_ui(view: str) -> int:
    """The task switcher; switching happens after it gave the terminal back."""
    action = screens().switcher(view)
    if action and action[0] == "switch":
        return switch_to(action[1])
    return 0


def project_rows(data: dict) -> list[dict]:
    return [
        {
            "id": project_id,
            "name": project["name"],
            "kind": project["kind"],
            "path": project["path"],
            "url": project.get("url"),
            "tasks": sum(1 for task in data["tasks"].values() if task.get("project") == project_id),
        }
        for project_id, project in sorted(data["projects"].items(), key=lambda p: p[1]["name"].lower())
    ]


def cmd_project_add(kind: str, source: str, name: str | None) -> int:
    project_id = add_project(kind, source, name)
    print(Store().load()["projects"][project_id]["name"])
    return 0


def cmd_project_rename(query: str, new_name: str) -> int:
    old, name = rename_project(query, new_name)
    print(f"project {old} renamed to {name}")
    return 0


def cmd_project_list(as_json: bool) -> int:
    rows = project_rows(Store().load())
    if as_json:
        print(json.dumps(rows, indent=2))
    else:
        for row in rows:
            tasks = f"{row['tasks']} task" + ("" if row["tasks"] == 1 else "s")
            print(f"{row['name']}\t{row['kind']}\t{row['url'] or display_path(row['path'])}\t{tasks}")
    return 0


def cmd_project_remove(query: str, delete_clone: bool | None) -> int:
    """Forget a project no task uses; a git project's clone is deleted on request.

    Without ``--delete-clone``/``--keep-clone`` only a clean clone itm may
    delete is asked about (keep by default); a clone with local work is kept.
    """
    notes = []
    if delete_clone is not None:
        mode = "delete" if delete_clone else "keep"
    else:
        # Nothing is deleted without a question: only the dialog can choose "if-clean".
        info = removal_info(query)
        mode = "keep"
        where = display_path(info["path"])
        if info["clone"] == "foreign":
            notes.append(f"kept {where}: not a clone itm can delete")
        elif info["clone"] == "deletable" and info["local_work"]:
            notes.append(f"kept {where}: it has {info['local_work']}; --delete-clone deletes it anyway")
        elif info["clone"] == "deletable" and not info["users"]:
            if sys.stdin.isatty() and sys.stdout.isatty():
                mode = screens().remove_project_dialog(info)
                if mode is None:
                    raise Cancelled("cancelled")
            else:
                notes.append(f"kept {where}: no terminal to ask; --delete-clone deletes it")
    for line in remove_project(query, mode) + notes:
        if line.startswith("kept "):
            print(f"itm: {line}", file=sys.stderr)
        else:
            print(line)
    return 0


# --------------------------------------------------------------------------
# Daemon


def notified_path() -> Path:
    return state_dir() / "notified.json"


def load_notified() -> dict[str, int]:
    try:
        return json.loads(notified_path().read_text())
    except (FileNotFoundError, json.JSONDecodeError):
        return {}


def desktop_notify(title: str, body: str, target: str | None) -> None:
    """A desktop notification in a thread; its Open action (notify-send) switches an itm client."""
    try:
        command = system.notify_command(
            title, body, app="inbox-task-manager", with_open=target is not None and bool(live_clients())
        )
    except ValueError as exc:
        print(f"cannot read $ITM_NOTIFY_COMMAND ({exc}): {title}", file=sys.stderr, flush=True)
        return
    if command is None:
        print(f"no way to show desktop notifications; set ITM_NOTIFY_COMMAND: {title}", file=sys.stderr, flush=True)
        return
    args, offer_open = command

    def run() -> None:
        try:
            result = subprocess.run(args, capture_output=True, text=True, check=False, timeout=3600)
            if offer_open and result.stdout.strip() == "open":
                handoff(target, newest_only=True)
        except Exception as exc:  # a notification must never take the daemon down
            print(f"notification failed: {exc}", file=sys.stderr, flush=True)

    threading.Thread(target=run, daemon=True).start()


def deliver(title: str, body: str, target: str | None, sessions: dict[str, dict]) -> None:
    """Herdr toast in an attached session if possible, else a desktop notification."""
    for session in set(live_clients().values()):
        socket = sessions.get(session, {}).get("socket_path")
        if not socket:
            continue
        try:
            shown = herdr_json(
                ["notification", "show", title, "--body", body, "--sound", "request"], socket
            )["result"].get("shown")
        except (ItmError, KeyError, TypeError):
            shown = False
        if shown:
            return
    desktop_notify(title, body, target)


def poll_once(store: Store, notified: dict[str, int]) -> list[tuple[str, str, str | None]]:
    """One daemon pass; returns the (title, body, target) notifications sent.

    ``notified`` holds the blocked waits already reported. A wait is forgotten
    once its agent is no longer blocked, so the next wait is reported again.
    """
    sessions = list_sessions()
    attached = set(live_clients().values())
    status: dict[str, dict] = {}
    polled: set[str] = set()
    blocked_now: dict[str, tuple[str, dict]] = {}

    for name, session in sessions.items():
        if not is_task_session(name) or not session.get("running"):
            continue
        agents = list_agents(session["socket_path"])
        if agents is None:
            continue
        polled.add(name)
        entries = []
        for a in agents:
            entry = {
                "agent": a.get("agent", "?"),
                "status": a.get("agent_status", "unknown"),
                "title": a.get("terminal_title_stripped", ""),
                "pane_id": a.get("pane_id"),
            }
            if entry["status"] == "blocked":
                key = f"{name}|{a.get('pane_id')}|{a.get('state_change_seq')}"
                blocked_now[key] = (name, a)
                # When the wait started, as far as the daemon saw it.
                entry["since"] = notified.get(key, now())
            entries.append(entry)
        status[name] = {"blocked": sum(e["status"] == "blocked" for e in entries), "agents": entries}

    write_atomic(status_path(), json.dumps({"updated_at": now(), "sessions": status}) + "\n")
    new_waits = [event for key, event in blocked_now.items() if key not in notified]

    messages: list[tuple[str, str, str | None]] = []
    with store.edit() as data:
        adopt_sessions(data, sessions)
        tasks = data["tasks"]
        for name, agent in new_waits:
            task = tasks.get(name)
            if not task or task["state"] == "archived":
                continue
            woke = task["state"] == "snoozed"
            if woke:
                activate(task)
            # Herdr already toasts blocked agents in the attached session.
            if name in attached and not woke:
                continue
            label = agent.get("terminal_title_stripped") or agent.get("pane_id", "")
            prefix = "Woke · " if woke else ""
            messages.append(
                (f"{prefix}Needs input · {task['title']}", f"{agent.get('agent', 'agent')}: {label}", name)
            )
        waiting_sessions = {name for name, _ in blocked_now.values()}
        for name, task in tasks.items():
            if task["state"] != "snoozed":
                continue
            until = task.get("snoozed_until")
            if until is None and name in waiting_sessions:
                # Snoozed until an agent needs you: any wait wakes it, also one
                # already reported (e.g. while the daemon was down).
                activate(task)
                messages.append((f"Woke · Needs input · {task['title']}", "An agent is waiting for you", name))
            elif until is None and not sessions.get(name, {}).get("running"):
                # Its session stopped (Herdr restart, reboot): no agent can ask any more.
                activate(task)
                messages.append((f"Task back · {task['title']}", "Its session stopped, so no agent can ask", name))
            elif until is not None and until <= now():
                activate(task)
                messages.append((f"Task back · {task['title']}", "Snooze ended", name))

    # Only now that the store is updated: remember current waits, keep the
    # ones of sessions that could not be polled, forget everything else.
    kept = {
        key: ts
        for key, ts in notified.items()
        if key in blocked_now or key.split("|", 1)[0] not in polled
    }
    kept.update({key: notified.get(key, now()) for key in blocked_now})
    notified.clear()
    notified.update(kept)
    write_atomic(notified_path(), json.dumps(notified) + "\n")

    for title, body, target in messages:
        deliver(title, body, target, sessions)
    return messages


def cmd_daemon() -> int:
    store = Store()
    notified = load_notified()
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
    last_error = None
    while True:
        try:
            poll_once(store, notified)
            last_error = None
        except Exception as exc:  # keep the service alive; the service manager logs stderr
            # Once per distinct error: a missing herdr would fill the log every 2 s.
            if str(exc) != last_error:
                print(f"poll failed: {exc}", file=sys.stderr, flush=True)
            last_error = str(exc)
        time.sleep(POLL_SECONDS)


# --------------------------------------------------------------------------
# CLI


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="itm", description="Inbox-style tasks on Herdr sessions. Without a command, opens the task switcher."
    )
    sub = parser.add_subparsers(dest="command")

    p = sub.add_parser("ui", help="the task switcher (the default command)")
    p.add_argument("--view", choices=TUI_VIEWS, default="inbox")
    p = sub.add_parser("attach", help="attach (outside Herdr) or switch (inside) to a task or session")
    p.add_argument("target", nargs="?", default=DEFAULT_SESSION)
    p = sub.add_parser("new", help="create a task and switch to it; without a title, opens the form")
    p.add_argument("title", nargs="*")
    p.add_argument("-d", "--description")
    p.add_argument("-p", "--project", help="a saved project (see `itm project`)")
    p.add_argument("--snooze", metavar="WHEN", help="create the task snoozed, without switching")
    p.add_argument("-i", "--interactive", action="store_true", help="open the form with what was given filled in")
    p.add_argument("--no-switch", action="store_true")
    p = sub.add_parser("snooze", help="hide a task until a time: snooze [task] [when]")
    p.add_argument("--task", help="the task to snooze, when the first word should not be read as one")
    p.add_argument("--until-agent", action="store_true", help="no timer: wake when an agent needs you")
    p.add_argument("words", nargs="*")
    for command in ("wake", "unsnooze", "archive"):
        p = sub.add_parser(command, help="wake a snoozed task" if command != "archive" else "stop a task's session, keep it")
        p.add_argument("task", nargs="?")
    p = sub.add_parser("reopen", help="make an archived task active again and switch to it")
    p.add_argument("task")
    p = sub.add_parser("delete", help="delete a task, its session and its notes (asks first)")
    p.add_argument("task")
    p.add_argument("--yes", action="store_true", help="do not ask")
    p = sub.add_parser("edit", help="change a task's title, description or project (a running session gets a tab there)")
    p.add_argument("task")
    p.add_argument("--title")
    p.add_argument("--description", help="an empty value clears it")
    edit_project = p.add_mutually_exclusive_group()
    edit_project.add_argument("--project", default=UNSET)
    edit_project.add_argument("--no-project", dest="project", action="store_const", const=None)
    p = sub.add_parser("notes", help="open a task's notes (notes.md) in your editor; the current task by default")
    p.add_argument("task", nargs="?")
    notes_mode = p.add_mutually_exclusive_group()
    notes_mode.add_argument("--path", action="store_true", help="print the notes file instead of opening it")
    notes_mode.add_argument("--append", metavar="TEXT", help='add a dated note without an editor ("-" reads stdin)')
    sub.add_parser("jump", help="open the task whose agent has waited longest")
    sub.add_parser("last", help="go back to the previous task")
    p = sub.add_parser("status", help="task counts for a window title or bar")
    p.add_argument("--format", default=STATUS_FORMAT, help="fields: waiting, active, snoozed, archived, daemon")
    p.add_argument("--json", action="store_true")
    p = sub.add_parser("list", help="tasks for scripts: session, state, title, project, number of waiting agents")
    p.add_argument("--view", choices=VIEWS, default="all")
    p.add_argument("--json", action="store_true")
    p = sub.add_parser("project", help="saved git repositories and folders tasks can start in")
    projects = p.add_subparsers(dest="project_command", required=True)
    p = projects.add_parser("add", help="clone and save a git repository, or save a folder")
    p.add_argument("kind", choices=("git", "dir"))
    p.add_argument("source", help="git URL or folder path")
    p.add_argument("--name", help="defaults to the repository or folder name")
    p = projects.add_parser("list", help="saved projects with their tasks and last use")
    p.add_argument("--json", action="store_true")
    p = projects.add_parser("rename", help="rename a project; its tasks keep it")
    p.add_argument("name")
    p.add_argument("new_name")
    p = projects.add_parser("remove", help="forget a project no task uses")
    p.add_argument("name")
    clone = p.add_mutually_exclusive_group()
    clone.add_argument("--delete-clone", dest="delete_clone", action="store_true", help="delete a git project's clone without asking")
    clone.add_argument("--keep-clone", dest="delete_clone", action="store_false")
    p.set_defaults(delete_clone=None)
    sub.add_parser("current", help="print the current session")
    sub.add_parser("daemon", help="poll agents, notify and wake snoozed tasks")
    return parser


def run(args: argparse.Namespace) -> int:
    match args.command:
        case None:
            return cmd_ui("inbox")
        case "ui":
            return cmd_ui(args.view)
        case "attach":
            target = args.target
            if not is_task_session(target) and target not in list_sessions():
                with contextlib.suppress(ItmError):
                    target = resolve_task(Store().load(), target)
            return switch_to(target)
        case "new":
            return cmd_new(
                " ".join(args.title),
                switch=not args.no_switch,
                description=args.description,
                project=args.project,
                snooze=args.snooze,
                interactive=args.interactive,
            )
        case "snooze":
            return cmd_snooze(args.task, args.words, until_agent=args.until_agent)
        case "wake" | "unsnooze":
            return cmd_wake(args.task)
        case "archive":
            return cmd_archive(args.task)
        case "reopen":
            return cmd_reopen(args.task)
        case "delete":
            return cmd_delete(args.task, args.yes)
        case "notes":
            return cmd_notes(args.task, args.path, args.append)
        case "edit":
            return cmd_edit(args.task, args.title, args.description, args.project)
        case "jump":
            return cmd_jump()
        case "last":
            return cmd_last()
        case "status":
            return cmd_status(args.format, args.json)
        case "list":
            return cmd_list(args.view, args.json)
        case "project":
            match args.project_command:
                case "add":
                    return cmd_project_add(args.kind, args.source, args.name)
                case "list":
                    return cmd_project_list(args.json)
                case "rename":
                    return cmd_project_rename(args.name, args.new_name)
                case "remove":
                    return cmd_project_remove(args.name, args.delete_clone)
        case "current":
            print(current_session(list_sessions()) or "")
            return 0
        case "daemon":
            return cmd_daemon()
    return 0


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    try:
        return run(args)
    except Cancelled as exc:
        print(f"itm: {exc}", file=sys.stderr)
        return 1
    except KeyboardInterrupt:
        print("itm: cancelled", file=sys.stderr)
        return 130
    except ItmError as exc:
        print(f"itm: {exc}", file=sys.stderr)
        # Popups close when the command exits; keep the error readable.
        if os.environ.get("ITM_POPUP"):
            time.sleep(2.5)
        return 1


if __name__ == "__main__":
    sys.exit(main())
