"""The interactive screens of itm: task switcher, task form and dialogs.

Built with Textual and imported only by commands that need it. Rules this
module keeps (DESIGN.md):

- keys acting on the task list are inactive while a dialog or another screen
  is on top (``check_action``);
- calls that can block (Herdr, the store lock, hand-offs) run in thread
  workers; switching a client happens after the app gave the terminal back;
- a clone runs as a child process (``itm project add``) in its own session,
  never prompting, and is stopped with SIGTERM on cancel and on exit;
- user text goes into markup only through ``$variables``.
"""

from __future__ import annotations

import asyncio
import atexit
import contextlib
import os
import re
import signal
import subprocess
import threading
import time
from dataclasses import dataclass, field
from datetime import datetime, timedelta
from pathlib import Path
from typing import Sequence

from textual import on
from textual.app import App, ComposeResult, SuspendNotSupported
from textual.binding import Binding
from textual.containers import Horizontal, Vertical, VerticalScroll
from textual.content import Content
from textual.message import Message
from textual.reactive import reactive
from textual.screen import ModalScreen, Screen
from textual.suggester import Suggester, SuggestionReady
from textual.widgets import Input, OptionList, Select, Static, Tab, Tabs, TextArea
from textual.widgets.option_list import Option

import inbox_task_manager as core

THEME = "catppuccin-mocha"
VIEWS = [("inbox", "Inbox"), ("snoozed", "Snoozed"), ("archived", "Archived"), ("all", "All")]
REFRESH_SECONDS = 2.0
NO_PROJECT = "-"
NEW_ROW = "__new__"

# Clone processes started by this app; stopped when it exits for any reason.
_children: set[asyncio.subprocess.Process] = set()


def terminate_children() -> None:
    for proc in list(_children):
        if proc.returncode is None:
            try:
                proc.terminate()
            except ProcessLookupError:
                pass


atexit.register(terminate_children)


def markup(template: str, **values: str) -> Content:
    """Markup with user text passed only as ``$values`` (never parsed as markup)."""
    return Content.from_markup(template, **{k: str(v) for k, v in values.items()})


def join_lines(parts: list[Content]) -> Content:
    result = Content("")
    for i, part in enumerate(parts):
        result = result + (Content("\n") if i else Content("")) + part
    return result


# --------------------------------------------------------------------- time
def rel(ts: float, now: float | None = None) -> str:
    now = time.time() if now is None else now
    delta = ts - now
    s = abs(delta)
    if s < 60:
        return "now"
    if s < 3600:
        txt = f"{int(s // 60)}m"
    elif s < 86400:
        h, m = int(s // 3600), int((s % 3600) // 60)
        txt = f"{h}h {m}m" if m and h < 3 else f"{h}h"
    else:
        txt = f"{int(s // 86400)}d"
    return f"in {txt}" if delta > 0 else f"{txt} ago"


def short(ts: float, now: float | None = None) -> str:
    """Relative age without "ago" ("4m", "2h")."""
    return rel(ts, now).removesuffix(" ago")


def when(ts: float, now: float | None = None) -> str:
    d = datetime.fromtimestamp(ts)
    today = datetime.fromtimestamp(time.time() if now is None else now).date()
    if d.date() == today:
        return d.strftime("%H:%M")
    if d.date() == today + timedelta(days=1):
        return d.strftime("tomorrow %H:%M")
    return d.strftime("%a %d %b %H:%M")


def snooze_presets(now: float | None = None, agent: bool = True) -> list[tuple[str, int | str]]:
    """(label, until) in time order, past ones dropped, then (``agent``) "until an agent needs me"."""
    now = time.time() if now is None else now
    base = datetime.fromtimestamp(now)

    def at(days: int, hour: int) -> int:
        return int((base + timedelta(days=days)).replace(hour=hour, minute=0, second=0, microsecond=0).timestamp())

    monday = (7 - base.weekday()) or 7
    presets = [
        ("30 minutes", int(now) + 1800),
        ("1 hour", int(now) + 3600),
        ("This evening", at(0, 20)),
        ("Tomorrow morning", at(1, 9)),
        ("Next Monday", at(monday, 9)),
    ]
    timed = sorted((p for p in presets if p[1] > now), key=lambda p: p[1])
    return timed + ([("Until an agent needs me", core.UNTIL_AGENT)] if agent else [])


# --------------------------------------------------------------------- data
@dataclass
class Snapshot:
    data: dict = field(default_factory=lambda: {"tasks": {}, "projects": {}, "deleted": {}})
    sessions: dict = field(default_factory=dict)
    status: dict = field(default_factory=dict)
    fresh: bool = False
    current: str | None = None
    can_switch: bool = True
    switch_problem: str | None = None
    folders: dict = field(default_factory=dict)  # project id -> folder exists
    loaded: bool = False
    listed: bool = True  # False when Herdr did not answer: sessions are unknown, not absent


def load_snapshot() -> Snapshot:
    """Everything the screens show, read in one go (blocking; run in a worker)."""
    try:
        sessions, listed = core.list_sessions(), True
    except core.ItmError:
        sessions, listed = {}, False
    store = core.Store()
    data = store.load()
    deleted = data.get("deleted", {})
    # Adopt and expire tombstones only with a real session list: with none,
    # every tombstone would look expired and a session being deleted would
    # come back. Write only when there is something to change.
    if listed and (any(core.is_task_session(n) and n not in data["tasks"] and n not in deleted for n in sessions)
                   or any(n not in sessions for n in deleted)):
        with store.edit() as data:
            core.adopt_sessions(data, sessions)
    status, fresh = core.read_status()
    snap = Snapshot(data=data, sessions=sessions, status=status, fresh=fresh, loaded=True, listed=listed)
    if core.inside_herdr():
        snap.current = core.current_session(sessions)
        try:
            core.switchable_session()
        except core.ItmError as exc:
            snap.can_switch, snap.switch_problem = False, str(exc)
    snap.folders = {pid: Path(p["path"]).is_dir() for pid, p in data["projects"].items()}
    return snap


def project_name(snap: Snapshot, project_id: str | None) -> str | None:
    project = snap.data["projects"].get(project_id) if project_id else None
    return project["name"] if project else None


def blocked_agents(snap: Snapshot, name: str) -> list[dict]:
    return [a for a in snap.status.get(name, {}).get("agents", []) if a.get("status") == "blocked"]


def in_view(task: dict, view: str) -> bool:
    if view == "all":
        return True
    if view == "inbox":
        return task["state"] == "active"
    return task["state"] == view


_FILTER = re.compile(r'@"([^"]*)"?|@(\S+)')


def parse_query(text: str) -> tuple[list[str], str]:
    """(project tokens from @name / @"name with spaces", the remaining title text)."""
    tokens = [m.group(1) if m.group(1) is not None else m.group(2) for m in _FILTER.finditer(text)]
    rest = " ".join(_FILTER.sub(" ", text).split())
    return [t for t in tokens if t.strip()], rest


def filter_token(name: str) -> str:
    return f'@"{name}"' if " " in name else f"@{name}"


def badge(name: str | None, width: int = 10) -> Content:
    if not name:
        return Content(" " * width)
    text = markup("[$secondary]◆ $n[/]", n=name if len(name) <= width - 3 else name[: width - 4] + "…")
    return text + Content(" " * max(1, width - text.cell_length))


def task_row(snap: Snapshot, name: str, task: dict, now: float) -> Content:
    waits = blocked_agents(snap, name)
    if task["state"] == "snoozed":
        until = task.get("snoozed_until")
        icon = "[$warning]◔[/]"
        detail = markup("[$warning]wakes $w[/]", w=rel(until, now)) if until else markup("[$warning]until an agent asks[/]")
    elif task["state"] == "archived":
        icon = "[$text-muted]▣[/]"
        detail = markup("[$text-muted]archived $w[/]", w=rel(task.get("archived_at") or now, now))
    elif waits:
        first = min(waits, key=lambda a: a.get("since") or now)
        icon = "[$error]⚑[/]"
        detail = markup("[$error]$a waiting · $s[/]", a=first.get("agent", "agent"), s=short(first.get("since") or now, now))
    elif not snap.listed:
        icon = "[$text-muted]?[/]"
        detail = markup("[$text-muted]session unknown[/]")
    elif not snap.sessions.get(name, {}).get("running"):
        icon = "[$text-muted]○[/]"
        label = "not started" if name not in snap.sessions else f"stopped · {rel(task.get('last_used_at') or now, now)}"
        detail = markup("[$text-muted]$l[/]", l=label)
    else:
        agents = snap.status.get(name, {}).get("agents", [])
        icon = "[$success]●[/]"
        working = any(a.get("status") == "working" for a in agents)
        detail = markup("[$success]working[/]") if working else markup("[$text-muted]idle[/]")
    if name == snap.current:
        detail = detail + markup(" [$accent]· here[/]")
    title_text = task["title"] if len(task["title"]) <= 28 else task["title"][:27] + "…"
    title = Content.from_markup(f"{icon} $t", t=title_text)
    pad = max(2, 31 - title.cell_length)
    return title + Content(" " * pad) + badge(project_name(snap, task.get("project"))) + detail


_NOTES_META = re.compile(r"\*Created .*\*")


def notes_preview(name: str, task: dict, now: float) -> list[Content]:
    """The preview's notes lines: when they were edited and their last lines.

    The heading, the "Created" line and the description itm starts them with
    are left out (the preview shows those already).
    """
    tail = core.notes_tail(name, lines=12)
    if tail is None:
        return [markup("[$text-muted]Notes · none yet · ctrl+o writes some[/]")]
    mtime, lines = tail
    known = {line.strip() for line in (task.get("description") or "").splitlines()}
    lines = [l for l in lines if not l.startswith("# ") and not _NOTES_META.fullmatch(l) and l.strip() not in known]
    if not lines:
        return [markup("[$text-muted]Notes · nothing written yet · ctrl+o[/]")]
    head = markup("[$text-muted]Notes · edited $w[/]", w=rel(mtime, now))
    return [head, *(markup("[$text-muted]  $l[/]", l=l if len(l) <= 80 else l[:79] + "…") for l in lines[-3:])]


# ------------------------------------------------------------------ dialogs
class Dialog(ModalScreen):
    """Base for dialogs: Escape cancels."""

    BINDINGS = [Binding("escape", "cancel", "Cancel")]

    def action_cancel(self) -> None:
        self.dismiss(None)


class SnoozeScreen(Dialog):
    """Returns a timestamp, "agent", or None.

    "Until an agent needs me" is offered only when it could ever wake the task
    (``agent_problem`` is None).
    """

    def __init__(self, title: str, agent_problem: str | None = None) -> None:
        super().__init__()
        self.task_title = title
        self.agent_problem = agent_problem
        self.presets = snooze_presets(agent=agent_problem is None)

    def compose(self) -> ComposeResult:
        with Vertical(id="dialog"):
            yield Static(markup("Snooze [b]$t[/b]", t=self.task_title), classes="dtitle")
            yield Input(placeholder="Type 2h, 14:30, fri 9:00… or pick below", id="custom", compact=True)
            opts = []
            for i, (label, until) in enumerate(self.presets):
                right = "no timer" if until == core.UNTIL_AGENT else when(until)
                style = "$warning" if until == core.UNTIL_AGENT else "$foreground"
                opts.append(Option(markup(f"[{style}]$l[/][$text-muted]$r[/]", l=f"{label:<26}", r=right), id=str(i)))
            yield OptionList(*opts, id="presets")
            yield Static("", id="error")
            yield Static(markup("[$accent b]enter[/] snooze   [$accent b]esc[/] cancel"), classes="dhint")

    def on_mount(self) -> None:
        self.query_one("#presets", OptionList).highlighted = 0
        self.query_one("#custom", Input).focus()

    def on_key(self, event) -> None:
        # Arrows move the presets while typing; Ctrl+J/K (vim) move them from anywhere in the dialog.
        if event.key in ("ctrl+j", "ctrl+k") or (
                event.key in ("up", "down") and self.focused is self.query_one("#custom")):
            presets = self.query_one("#presets", OptionList)
            presets.action_cursor_up() if event.key in ("up", "ctrl+k") else presets.action_cursor_down()
            event.stop()
            event.prevent_default()

    @on(OptionList.OptionSelected, "#presets")
    def picked(self, event: OptionList.OptionSelected) -> None:
        self.dismiss(self.presets[int(event.option.id)][1])

    @on(Input.Submitted, "#custom")
    def submit(self, event: Input.Submitted) -> None:
        text = event.value.strip()
        if not text:
            presets = self.query_one("#presets", OptionList)
            self.dismiss(self.presets[presets.highlighted or 0][1])
            return
        try:
            self.dismiss(core.snooze_until(text))
        except core.ItmError as exc:
            self.query_one("#error", Static).update(markup("[$error]$e[/]", e=exc))


class DiscardScreen(Dialog):
    BINDINGS = [Binding("y", "dismiss(True)", "Discard"), Binding("n", "dismiss(False)", "Keep editing")]

    def compose(self) -> ComposeResult:
        with Vertical(id="dialog"):
            yield Static("Discard what you typed?", classes="dtitle")
            yield Static(markup("[$error b]y[/] discard   [$accent b]n[/] keep editing"), classes="dhint")

    def action_cancel(self) -> None:
        self.dismiss(False)


class FolderLister:
    """Folder listings for completion, read in daemon threads and kept while a dialog is open.

    A scan can block on a slow filesystem (NFS, autofs, a cold /nix/store). A
    daemon thread never holds up the event loop, asyncio's executor (store
    loads and core calls run there) or the app's exit. Each folder is listed
    once; typing filters the listing in memory.
    """

    def __init__(self, app: App, ready) -> None:
        self.app = app
        self.ready = ready  # called on the UI thread when a listing arrives
        self.listings: dict[str, tuple[str, list[str]] | None] = {}
        self.reading: set[str] = set()
        self.arrived: dict[str, asyncio.Event] = {}

    def get(self, parent: str) -> tuple[str, list[str]] | None | bool:
        """The listing of ``parent`` (as typed), None if unreadable, or False while it is read."""
        if parent in self.listings:
            return self.listings[parent]
        if parent not in self.reading:
            self.reading.add(parent)
            threading.Thread(target=self._read, args=(parent,), daemon=True).start()
        return False

    def _read(self, parent: str) -> None:
        listing = core.list_folders(parent)
        try:
            self.app.call_from_thread(self._done, parent, listing)
        except RuntimeError:  # the app has gone
            pass

    def _done(self, parent: str, listing) -> None:
        self.reading.discard(parent)
        self.listings[parent] = listing
        if event := self.arrived.pop(parent, None):
            event.set()
        self.ready()

    async def complete_when_read(self, text: str) -> list[str]:
        """``complete``, waiting (without blocking anything) for the folder to be read."""
        while (found := self.complete(text)) is None:
            split = core.folder_parent(text)
            await self.arrived.setdefault(split[0], asyncio.Event()).wait()
        return found

    def complete(self, text: str) -> list[str] | None:
        """Folders completing ``text``, or None while the folder is read."""
        if text in ("~", ".", ".."):
            return [text + "/"]
        if not (split := core.folder_parent(text)):
            return []
        listing = self.get(split[0])
        if listing is False:
            return None
        return core.match_folders(*split, listing[1]) if listing else []


class FolderSuggester(Suggester):
    """Ghost text completing a folder being typed (``~/co`` → ``~/code/``), exact prefix only.

    Input draws ``suggestion[len(value):]`` and → replaces the value with it, so
    a suggestion must start with exactly what was typed. ``last`` lets Tab accept
    it too.
    """

    def __init__(self, lister: FolderLister, enabled=lambda: True) -> None:
        # Paths are case sensitive, and folders change: no casefolding, no cache.
        super().__init__(use_cache=False, case_sensitive=True)
        self.lister = lister
        self.enabled = enabled
        self.last: tuple[str, str] | None = None

    async def get_suggestion(self, value: str) -> str | None:
        self.last = None
        # Nothing after a "/": every subfolder would match, and Tab would keep going down into them.
        if not (self.enabled() and core.looks_like_path(value)) or value.endswith("/"):
            return None
        found = await self.lister.complete_when_read(value)
        if not self.enabled():  # switched to a repository while the folder was read
            return None
        suggestion = next((c for c in found if c.startswith(value) and c != value), None)
        if suggestion:
            self.last = (value, suggestion)
        return suggestion


def name_badge(name: str, width: int, matched: Sequence[int] = ()) -> Content:
    """``badge`` with the letters a fuzzy search matched underlined (user text never parsed as markup)."""
    cut = len(name) > width - 3
    shown = name[: width - 4] + "…" if cut else name
    text = Content(shown)
    for offset in matched:
        if offset < len(shown) - cut:
            text = text.stylize("bold underline", offset, offset + 1)
    text = markup("[$secondary]◆ [/]") + text.stylize("$secondary")
    return text + Content(" " * max(1, width - text.cell_length))


def project_line(project: dict, matched: Sequence[int] = (), missing: bool = False) -> Content:
    where = core.project_where(project) + (" (folder missing)" if missing else "")
    return name_badge(project["name"], 14, matched) + markup("[$text-muted]$k $w[/]", k=f"{project['kind']:<4}", w=where)


def looks_like_git_url(text: str) -> bool:
    """A git URL being typed (``scheme://…`` or ``user@host:path``), not a project name."""
    return bool(re.match(r"[a-z][a-z0-9+.-]*://\S", text, re.IGNORECASE)
                or re.fullmatch(r"[^@\s/:]+@[^@\s/:]+:\S+", text))


def saved_repository(projects: dict, url: str) -> str | None:
    """The saved git project for ``url``, compared as ``normalize_git_url`` sees it (no git call)."""
    try:
        key = core.normalize_git_url(url)
    except core.ItmError:
        return None
    for pid, project in projects.items():
        if project["kind"] == "git":
            with contextlib.suppress(core.ItmError):
                if core.normalize_git_url(project["url"]) == key:
                    return pid
    return None


class ProjectField(Static, can_focus=True):
    """The task form's project. Enter, Space, typing, a paste or a click opens ``ProjectPicker``.

    ``value`` is a project id or ``NO_PROJECT``; a change posts ``Changed``.
    Keys that arrive before the picker is up go into that same picker, never
    into a second one.
    """

    value = reactive(NO_PROJECT, init=False)

    class Changed(Message):
        def __init__(self, field: ProjectField, value: str) -> None:
            super().__init__()
            self.field, self.value = field, value

        @property
        def control(self) -> ProjectField:
            return self.field

    def __init__(self, projects: dict, value: str, **kwargs) -> None:
        super().__init__("", **kwargs)
        self.projects = projects
        self.picker: ProjectPicker | None = None
        self.set_reactive(ProjectField.value, value)

    def on_mount(self) -> None:
        self.show()

    def watch_value(self) -> None:
        self.show()
        self.post_message(self.Changed(self, self.value))

    def on_focus(self) -> None:
        self.show()

    def on_blur(self) -> None:
        self.show()

    def show(self) -> None:
        project = self.projects.get(self.value)
        text = project_line(project) if project else markup("[$text-muted]No project[/]")
        if self.has_focus:
            text = text + markup("  [$accent]enter or type to search[/]")
        self.update(text)

    def on_key(self, event) -> None:
        if event.key in ("enter", "space") and self.picker is None:
            text = ""
        elif event.is_printable and event.character:
            text = event.character
        else:
            return
        event.stop()
        event.prevent_default()
        self.open(text)

    def on_paste(self, event) -> None:
        event.stop()
        self.open(" ".join(event.text.split()))

    def on_click(self) -> None:
        self.open("")

    def open(self, text: str) -> None:
        if self.picker is not None:
            self.picker.feed(text)
            return

        def picked(value: str | None) -> None:
            self.picker = None
            if value is not None:
                self.projects = core.Store().load()["projects"]  # the picker may have added one
                if value == self.value:
                    self.show()
                self.value = value

        self.picker = ProjectPicker(self.value, text)
        self.app.push_screen(self.picker, picked)


class ProjectPicker(Dialog):
    """Fuzzy finder over the saved projects; a typed path lists folders to add, a URL offers a clone.

    Returns a project id, ``NO_PROJECT``, or None (Esc: the form keeps its choice).
    Rows are (label, action): ("project", id), ("none",), ("add", kind, source)
    or ("folder", path, saved project id or None).
    """

    BINDINGS = [
        # Priority: the Input would take Ctrl+K (delete to the end) and Tab (focus the list).
        Binding("up,ctrl+k", "move(-1)", show=False, priority=True),
        Binding("down,ctrl+j", "move(1)", show=False, priority=True),
        Binding("tab,shift+tab", "complete", show=False, priority=True),
    ]

    def __init__(self, current: str = NO_PROJECT, query: str = "") -> None:
        super().__init__()
        self.current = current
        self.query_text = query
        self.data = core.Store().load()
        self.folders = {pid: Path(p["path"]).is_dir() for pid, p in self.data["projects"].items()}
        self.saved = {os.path.realpath(p["path"]): pid for pid, p in self.data["projects"].items()}
        self.rows: list[tuple] = []
        self.lister: FolderLister | None = None

    def compose(self) -> ComposeResult:
        with Vertical(id="dialog", classes="wide"):
            yield Static("Project", classes="dtitle")
            # No select_on_focus: the key that opened the picker must not be replaced by the next one.
            yield Input(self.query_text, placeholder="Search projects, or type ~/folder, /folder or a git URL",
                        id="pquery", compact=True, select_on_focus=False)
            yield OptionList(id="pchoices")
            yield Static("", id="pstatus")
            yield Static(markup("[$accent b]enter[/] choose   [$accent b]tab[/] complete a folder   "
                                "[$accent b]↑↓[/] move   [$accent b]esc[/] back"), classes="dhint")

    def on_mount(self) -> None:
        self.lister = FolderLister(self.app, self.rebuild)
        self.query_one("#pchoices", OptionList).can_focus = False  # typing always reaches the search
        box = self.query_one("#pquery", Input)
        box.focus()
        box.cursor_position = len(box.value)
        self.rebuild()

    def feed(self, text: str) -> None:
        """Keys typed before the search box had the focus: on the form's field, or on this screen."""
        boxes = self.query("#pquery")
        if not boxes:  # not composed yet
            self.query_text += text
            return
        boxes.first(Input).insert_text_at_cursor(text)

    def on_key(self, event) -> None:
        # Keys that arrive after this screen is up but before the search box has
        # the focus reach the screen itself; they belong in the search.
        if self.focused is None and event.is_printable and event.character:
            event.stop()
            event.prevent_default()
            self.feed(event.character)

    def on_paste(self, event) -> None:
        if self.focused is None:
            event.stop()
            self.feed(" ".join(event.text.split()))

    def text(self) -> str:
        return self.query_one("#pquery", Input).value.strip()

    @on(Input.Changed, "#pquery")
    def rebuild(self) -> None:
        if not self.is_attached:
            return  # a folder listing that arrived after the picker closed
        text = self.text()
        if core.looks_like_path(text):
            rows = self.folder_rows(text)
        else:
            rows = self.project_rows(text)
        self.show_rows(text, rows)

    def project_rows(self, text: str) -> list[tuple]:
        projects = core.projects_by_recent(self.data["projects"])
        if not text:
            rows = [(markup("[$text-muted]No project[/]"), ("none",))]
            rows += [(project_line(p, missing=not self.folders.get(pid, True)), ("project", pid)) for pid, p in projects]
        else:
            scored = []
            for rank, (pid, p) in enumerate(projects):
                name = core.fuzzy_match(text, p["name"])
                # Git projects by URL, folders by where they are: a clone's path is mostly itm's own folder.
                where = core.fuzzy_match(text, core.project_where(p))
                score = max(name[0] if name else 0, int(where[0] * 0.8) if where else 0)
                if score:
                    matched = name[1] if name and name[0] >= score else ()
                    scored.append((-score, rank, (project_line(p, matched, not self.folders.get(pid, True)),
                                                  ("project", pid))))
            rows = [row for *_, row in sorted(scored, key=lambda r: r[:2])]
            if core.fuzzy_match(text, "No project"):
                rows.append((markup("[$text-muted]No project[/]"), ("none",)))
            if looks_like_git_url(text):
                saved = saved_repository(self.data["projects"], text)
                if saved:
                    rows.insert(0, (project_line(self.data["projects"][saved]) + markup("  [$text-muted]saved[/]"),
                                    ("project", saved)))
                else:
                    rows.insert(0, (markup("[$accent]+ Clone[/] $u", u=text), ("add", "git", text)))
        rows += [(markup("[$accent]+ Add git repository…[/]"), ("add", "git", "")),
                 (markup("[$accent]+ Add local folder…[/]  [$text-muted]or type ~/ or /[/]"), ("add", "dir", ""))]
        return rows

    def folder_rows(self, text: str) -> list[tuple] | None:
        """Rows for a folder being typed, or None while its folder is read."""
        if text in ("~", ".", ".."):
            text += "/"  # the folder itself, then what is in it
        completions = self.lister.complete(text)
        if completions is None:
            return None
        split = core.folder_parent(text)
        listing = self.lister.get(split[0]) if split else None
        if not listing:
            return []
        real, names, links = listing

        def resolved(name: str) -> str:  # a symlink resolves to its target, as saved projects are
            return links.get(name) or os.path.normpath(os.path.join(real, name))

        def label(path: str, pid: str | None, prefix: str) -> Content:
            project = self.data["projects"].get(pid) if pid else None
            line = markup(f"{prefix}$p", p=path)
            if project:
                line = line + markup("  [$secondary]◆ $n[/] [$text-muted]saved[/]", n=project["name"])
            return line

        # The folder the text names, when it exists (the parent's listing says so).
        named = os.path.normpath(real) if split[1] == "" else resolved(split[1]) if split[1] in names else None
        rows = []
        if named:
            rows.append((label(text, self.saved.get(named), "[$accent]Use[/] "), ("folder", text, self.saved.get(named))))
        for path in completions:
            target = resolved(path[len(split[0]):].rstrip("/"))
            if target != named:
                rows.append((label(path, self.saved.get(target), "  "), ("folder", path, self.saved.get(target))))
        return rows

    def show_rows(self, text: str, rows: list[tuple] | None) -> None:
        status = self.query_one("#pstatus", Static)
        choices = self.query_one("#pchoices", OptionList)
        if rows is None:
            # Nothing to act on until the listing arrives (it calls rebuild): Enter
            # or Tab on rows left from what was typed before would pick the wrong thing.
            self.rows = []
            choices.clear_options()
            status.update(markup("[$text-muted]Reading the folder…[/]"))
            return
        self.rows = rows
        choices.clear_options()
        choices.add_options([Option(prompt, id=str(i)) for i, (prompt, _) in enumerate(rows)])
        if core.looks_like_path(text):
            status.update(markup("[$text-muted]No folder there matches.[/]") if not rows else
                          markup("[$text-muted]Enter adds the folder (or picks it when saved); tab goes into it.[/]"))
        elif text and not any(a[0] == "project" or (a[0] == "add" and a[2]) for _, a in rows):
            status.update(markup("[$text-muted]No project matches “$q”. Type a path (~/…, /…) to add a folder.[/]",
                                 q=text))
        else:
            status.update("")
        if not rows:
            choices.highlighted = None
        elif text:
            choices.highlighted = 0
        else:
            choices.highlighted = next((i for i, (_, action) in enumerate(rows) if action == ("project", self.current)
                                        or (action == ("none",) and self.current == NO_PROJECT)), 0)

    def action_move(self, step: int) -> None:
        choices = self.query_one("#pchoices", OptionList)
        choices.action_cursor_down() if step > 0 else choices.action_cursor_up()

    def highlighted(self) -> tuple | None:
        choices = self.query_one("#pchoices", OptionList)
        if choices.highlighted is None or choices.highlighted >= len(self.rows):
            return None
        return self.rows[choices.highlighted][1]

    def action_complete(self) -> None:
        """Tab: go into the highlighted folder, whose subfolders are listed next. Nothing for projects."""
        action = self.highlighted()
        if action and action[0] == "folder":
            box = self.query_one("#pquery", Input)
            box.value = action[1] if action[1].endswith("/") else action[1] + "/"
            box.cursor_position = len(box.value)  # setting the value does not move the cursor

    @on(Input.Submitted, "#pquery")
    def submitted(self) -> None:
        if action := self.highlighted():
            self.pick(action)

    @on(OptionList.OptionSelected, "#pchoices")
    def clicked(self, event: OptionList.OptionSelected) -> None:
        self.pick(self.rows[int(event.option.id)][1])

    def pick(self, action: tuple) -> None:
        kind = action[0]
        if kind == "project":
            self.dismiss(action[1])
        elif kind == "none":
            self.dismiss(NO_PROJECT)
        elif kind == "folder" and action[2]:
            self.dismiss(action[2])  # already saved: that project
        else:
            add_kind, source = ("dir", action[1]) if kind == "folder" else action[1:]
            screen = AddProjectScreen(add_kind, source=source, focus="name" if source else "source")

            def added(pid: str | None) -> None:  # returns nothing: Textual awaits what a callback returns
                if pid:
                    self.dismiss(pid)

            self.app.push_screen(screen, added)


class TaskForm(Dialog):
    """New task (title, description, project, snooze) or edit task.

    Returns {"title", "description", "project", "until"} for a new task, and
    {"title", "description"} plus "project" when it changed for an edit.
    ``session_effect`` (edit only) is ``core.project_change_effect`` for the task.
    """

    BINDINGS = [Binding("ctrl+s", "save", "Save", priority=True)]

    def __init__(
        self,
        title: str = "",
        description: str | None = None,
        project: str | None = None,
        until: int | str | None = None,
        task: dict | None = None,
        task_name: str | None = None,
        session_effect: str | None = "starts",
        cannot_switch: str | None = None,
    ) -> None:
        super().__init__()
        self.editing = task is not None
        self.initial_title = (task or {}).get("title", title or "")
        self.initial_desc = (task or {}).get("description", description or "")
        self.project = (task or {}).get("project") or project or NO_PROJECT
        self.initial_project = self.project
        self.until = until
        self.task_name = task_name
        self.session_effect = session_effect
        self.cannot_switch = cannot_switch
        self.data = core.Store().load()
        # Fixed while the form is open, so indexes stay valid (core rejects a
        # time that has passed meanwhile). No "until an agent": a new task has
        # no agents yet.
        self.presets = snooze_presets(agent=False)

    def compose(self) -> ComposeResult:
        with Vertical(id="dialog", classes="wide"):
            yield Static("Edit task" if self.editing else "New task", classes="dtitle")
            with Horizontal(classes="field"):
                yield Static("Title", classes="label")
                yield Input(value=self.initial_title, placeholder="What is this task about?", id="title", compact=True)
            with Horizontal(classes="field tall"):
                yield Static("Description", classes="label")
                yield TextArea(self.initial_desc, id="desc", compact=True, soft_wrap=True, show_line_numbers=False,
                               tab_behavior="focus", placeholder="Optional. Enter adds a line.")
            with Horizontal(classes="field"):
                yield Static("Project", classes="label")
                value = self.project if self.project in self.data["projects"] else NO_PROJECT
                self.project = self.initial_project = value
                yield ProjectField(self.data["projects"], value, id="project")
            if not self.editing:
                with Horizontal(classes="field"):
                    yield Static("Snooze", classes="label")
                    snoozes = [(Content("Don't snooze"), "none")]
                    for i, (label, until) in enumerate(self.presets):
                        right = "no timer" if until == core.UNTIL_AGENT else when(until)
                        snoozes.append((markup("$l  [$text-muted]$r[/]", l=label, r=right), f"p{i}"))
                    snoozes.append((markup("[$accent]Other time…[/]"), "other"))
                    yield Select(snoozes, value="none", allow_blank=False, id="snooze", compact=True)
                with Horizontal(classes="field", id="other-row"):
                    yield Static("", classes="label")
                    yield Input(placeholder="2h, 14:30, tomorrow 9:00, fri 9:00", id="snooze-text", compact=True)
            yield Static("", id="where")
            yield Static("", id="error")
            hint = ("[$accent b]^s[/] save   [$accent b]esc[/] cancel" if self.editing else
                    "[$accent b]enter[/] create   [$accent b]tab[/] next field   [$accent b]^s[/] create from any field   "
                    "[$accent b]esc[/] cancel")
            yield Static(markup(hint), classes="dhint")

    def on_mount(self) -> None:
        if not self.editing:
            self.query_one("#other-row").display = False
            if self.until is not None:
                self.preset_until(self.until)
        self.query_one("#title", Input).focus()
        self.update_where()

    def preset_until(self, until: int | str) -> None:
        """Show a snooze time given on the command line (never "agent": a new task has no agents)."""
        self.query_one("#snooze", Select).value = "other"
        self.query_one("#other-row").display = True
        self.query_one("#snooze-text", Input).value = datetime.fromtimestamp(until).strftime("%Y-%m-%d %H:%M")

    def typed(self) -> bool:
        """Whether Esc would lose something: text, or (when editing) a changed project."""
        title = self.query_one("#title", Input).value.strip()
        desc = self.query_one("#desc", TextArea).text.strip()
        changed_project = self.editing and self.project != self.initial_project
        return changed_project or (title, desc) != (self.initial_title.strip(), (self.initial_desc or "").strip())

    def action_cancel(self) -> None:
        if not self.typed():
            self.dismiss(None)
            return
        def answered(discard: bool) -> None:  # returns nothing: Textual awaits what a callback returns
            if discard:
                self.dismiss(None)

        self.app.push_screen(DiscardScreen(), answered)

    @on(Input.Changed, "#title")
    @on(Input.Changed, "#snooze-text")
    def changed(self) -> None:
        self.update_where()

    @on(Select.Changed, "#snooze")
    def snooze_changed(self, event: Select.Changed) -> None:
        other = self.query_one("#other-row")
        other.display = event.value == "other"
        if other.display:
            self.query_one("#snooze-text", Input).focus()
        self.update_where()

    @on(ProjectField.Changed, "#project")
    def project_changed(self, event: ProjectField.Changed) -> None:
        self.data = core.Store().load()  # the picker may have added a project
        self.project = event.value
        self.update_where()

    def snooze_value(self) -> int | str | None:
        """The chosen snooze; raises ItmError for a time that does not parse."""
        value = self.query_one("#snooze", Select).value
        if value == "none":
            return None
        if value == "other":
            text = self.query_one("#snooze-text", Input).value.strip()
            until = core.snooze_until(text) if text else None
            if until == core.UNTIL_AGENT:  # refuse here, while what was typed is still on screen
                raise core.ItmError("a new task has no agents yet, so none could wake it; pick a time")
            return until
        return self.presets[int(value[1:])][1]

    def update_where(self) -> None:
        where = self.query_one("#where", Static)
        if self.editing:
            lines = [markup("[$text-muted]Session[/]   $s", s=self.task_name or "")]
            project = self.data["projects"].get(self.project)
            if project and self.project != self.initial_project:
                folder = core.display_path(project["path"])
                if not Path(project["path"]).is_dir():
                    effect = "[$error]Its folder $f is missing; the session would start in $d instead[/]"
                else:
                    effect = "[$text-muted]" + {
                        "starts": "Its session will start in $f",
                        "tab": "Its session keeps running where it is; a new tab opens in $f",
                        "stopped": "Its stopped session keeps its folders; open a tab in $f when you need it",
                        None: "Herdr did not answer, so it is unknown whether its session runs",
                    }[self.session_effect] + "[/]"
                lines.append(markup(effect, f=folder, d=core.display_path(core.task_dir())))
            where.update(join_lines(lines))
            return
        project = self.data["projects"].get(self.project)
        folder = core.display_path(project["path"]) if project else core.display_path(core.task_dir())
        title = self.query_one("#title", Input).value
        try:
            snoozed = self.snooze_value() is not None
        except core.ItmError:
            snoozed = True
        if snoozed:
            then = "Saved snoozed; the session starts when you open it"
        elif self.cannot_switch:
            then = f"Saved without switching: {self.cannot_switch}"
        else:
            then = "Then switches you to it"
        where.update(markup("[$text-muted]Starts in[/] $f\n[$text-muted]Session[/]   task-$s\n[$text-muted]$t[/]",
                            f=folder, s=core.slugify(title) if title.strip() else "…", t=then))

    @on(Input.Submitted, "#title")
    @on(Input.Submitted, "#snooze-text")
    def action_save(self) -> None:
        error = self.query_one("#error", Static)
        title = self.query_one("#title", Input).value.strip()
        if not title:
            error.update(markup("[$error]The task needs a title.[/]"))
            self.query_one("#title", Input).focus()
            return
        desc = self.query_one("#desc", TextArea).text.strip()
        project = None if self.project == NO_PROJECT else self.project
        if self.editing:
            result = {"title": title, "description": desc}
            if self.project != self.initial_project:
                result["project"] = project
            self.dismiss(result)
            return
        try:
            until = self.snooze_value()
        except core.ItmError as exc:
            error.update(markup("[$error]$e[/]", e=exc))
            return
        self.dismiss({"title": title, "description": desc or None, "project": project, "until": until})


class AddProjectScreen(Dialog):
    """Save a folder, or clone a repository in a child process; returns the project id."""

    BINDINGS = [
        Binding("ctrl+left", "kind('git')", show=False),
        Binding("ctrl+right", "kind('dir')", show=False),
        # Priority: Tab would otherwise just move to the next field.
        Binding("tab", "complete_or_next", show=False, priority=True),
    ]

    def __init__(self, kind: str, source: str = "", focus: str = "source") -> None:
        super().__init__()
        self.kind = kind
        self.source = source
        self.start_focus = focus
        self.proc: asyncio.subprocess.Process | None = None
        self.stopping = False
        self.timer = None
        self.outcome_shown = False
        self.busy = False

    def compose(self) -> ComposeResult:
        with Vertical(id="dialog", classes="wide"):
            yield Static("Add project", classes="dtitle")
            yield Tabs(Tab("Git repository", id="git"), Tab("Local folder", id="dir"), active=self.kind, id="kind")
            with Horizontal(classes="field"):
                yield Static("Source", classes="label")
                self.suggester = FolderSuggester(FolderLister(self.app, lambda: None), enabled=lambda: self.kind == "dir")
                yield Input(self.source, id="source", compact=True, suggester=self.suggester)
            with Horizontal(classes="field"):
                yield Static("Name", classes="label")
                yield Input(id="pname", compact=True)
            yield Static("", id="status")
            yield Static(markup("[$accent b]enter[/] add   [$accent b]tab[/] complete   "
                                "[$accent b]←→[/] git / folder (empty source)   [$accent b]esc[/] back"), classes="dhint")

    def on_mount(self) -> None:
        source = self.query_one("#source", Input)
        source.cursor_position = len(source.value)
        self.query_one("#pname" if self.start_focus == "name" else "#source", Input).focus()
        self.update()

    def action_kind(self, kind: str) -> None:
        self.query_one("#kind", Tabs).active = kind

    @on(Tabs.TabActivated, "#kind")
    def kind_changed(self, event: Tabs.TabActivated) -> None:
        self.kind = event.tab.id
        source = self.query_one("#source", Input)
        source.post_message(SuggestionReady(source.value, ""))  # no folder ghost text for a repository
        self.suggester.last = None
        self.update()

    def action_complete_or_next(self) -> None:
        """Tab in the folder source takes the suggested completion; otherwise it moves on."""
        source = self.query_one("#source", Input)
        last = self.suggester.last
        if self.focused is source and self.kind == "dir" and last and last[0] == source.value:
            source.value = last[1]
            source.cursor_position = len(source.value)
            return
        self.focus_next()

    def on_key(self, event) -> None:
        if event.key in ("left", "right") and not self.query_one("#source", Input).value and self.proc is None:
            self.action_kind("git" if event.key == "left" else "dir")
            event.stop()

    def check(self) -> tuple[dict | None, str | None, str | None]:
        """(spec, name, problem); problem is "same", "name" or "source" with a message in self.message."""
        source = self.query_one("#source", Input).value.strip()
        self.message = ""
        if not source:
            return None, None, "empty"
        try:
            spec = core.project_spec(self.kind, source)
        except core.ItmError as exc:
            self.message = str(exc)
            return None, None, "source"
        data = core.Store().load()
        try:
            core.check_project_free(data, spec)
        except core.ItmError as exc:
            self.message = str(exc)
            return spec, None, "same"
        name = self.query_one("#pname", Input).value.strip() or core.default_project_name(spec)
        try:
            core.check_name_free(data, core.check_project_name(name))
        except core.ItmError as exc:
            self.message = str(exc)
            return spec, name, "name"
        return spec, name, None

    @on(Input.Changed, "#source")
    @on(Input.Changed, "#pname")
    def changed(self) -> None:
        # Validation reads the store and may run `git ls-remote --get-url`:
        # once typing pauses, not on every key.
        self.outcome_shown = False
        self.stop_timer()
        self.timer = self.set_timer(0.25, self.update)

    def stop_timer(self) -> None:
        if self.timer is not None:
            self.timer.stop()
            self.timer = None

    def update(self) -> None:
        self.timer = None
        # Keep a clone's progress, error or "stopped" until a field changes.
        if self.proc is not None or self.outcome_shown:
            return
        self.query_one("#source", Input).placeholder = (
            "git@host:group/repo.git or https://host/group/repo" if self.kind == "git" else "~/code/project")
        spec, name, problem = self.check()
        default = core.default_project_name(spec) if spec else ""
        self.query_one("#pname", Input).placeholder = default or "defaults to the repository or folder name"
        status = self.query_one("#status", Static)
        if problem == "empty":
            status.update("")
        elif problem in ("source", "same", "name"):
            status.update(markup("[$error]$m[/]", m=self.message))
        elif self.kind == "git":
            status.update(markup("[$text-muted]Clones to[/] $p\n[$text-muted]Esc stops the clone and removes what it "
                                 "copied.[/]", p=core.display_path(spec["path"])))
        else:
            status.update(markup("[$text-muted]Saves the folder; nothing is copied.[/]"))

    @on(Input.Submitted)
    def submit(self) -> None:
        # busy covers the gap before the clone's process exists (a second Enter there).
        if self.proc is not None or self.busy:
            return
        spec, name, problem = self.check()
        self.stop_timer()
        self.outcome_shown = False
        self.update()  # show the current problem right away, without waiting for the debounce
        if problem == "name":
            self.query_one("#pname", Input).focus()  # ask for another name
            return
        if problem is not None:
            return
        source = self.query_one("#source", Input).value.strip()
        self.busy = True
        if self.kind == "dir":
            self.run_worker(lambda: self.add_folder(source, name), thread=True, exit_on_error=False)
        else:
            self.run_worker(self.clone(source, name), exclusive=True, exit_on_error=False)

    def add_folder(self, source: str, name: str) -> None:
        try:
            project_id = core.add_project("dir", source, name)
        except core.ItmError as exc:
            self.outcome_shown = True
            self.app.call_from_thread(self.query_one("#status", Static).update, markup("[$error]$m[/]", m=exc))
            return
        finally:
            self.busy = False
        self.app.call_from_thread(self.dismiss, project_id)

    async def clone(self, source: str, name: str) -> None:
        try:
            await self._clone(source, name)
        finally:
            self.busy = False

    async def _clone(self, source: str, name: str) -> None:
        status = self.query_one("#status", Static)
        status.update(markup("[$text-muted]Starting the clone…[/]"))
        # --name=…: a name starting with "-" must not be read as an option.
        argv = [*core.self_command(), "project", "add", "git", f"--name={name}", "--", source]
        try:
            proc = await asyncio.create_subprocess_exec(
                *argv, stdin=subprocess.DEVNULL, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.STDOUT,
                env=core.clone_env(), start_new_session=True,
            )
        except OSError as exc:
            self.outcome_shown = True
            status.update(markup("[$error]Could not start the clone: $e[/]", e=exc))
            return
        self.proc = proc
        _children.add(proc)
        output = []
        try:
            while chunk := await proc.stdout.read(512):
                text = chunk.decode(errors="replace")
                output.append(text)
                parts = [p.strip() for p in re.split(r"[\r\n]", text) if p.strip()]
                if parts and not self.stopping:
                    status.update(markup("[$text-muted]Cloning…[/] $l", l=parts[-1][-70:]))
            code = await proc.wait()
        finally:
            # Also when the app exits mid-clone (the worker is cancelled): the
            # child runs in its own session and would finish on its own.
            if proc.returncode is None:
                proc.terminate()
            _children.discard(proc)
            self.proc = None
        full = "".join(output)
        stopped, self.stopping = self.stopping, False
        if code == 0:  # finished even if Esc came too late to stop it
            added = [l for l in full.splitlines() if l.strip()]
            try:
                project_id = core.resolve_project(core.Store().load(), added[-1].strip() if added else name)
            except core.ItmError as exc:
                self.outcome_shown = True
                status.update(markup("[$error]The clone finished, but $e[/]", e=exc))
                return
            self.dismiss(project_id)
            return
        if stopped:
            self.outcome_shown = True
            status.update(markup("[$warning]Stopped. Nothing of the clone was kept.[/]"))
            return
        errors = [l.removeprefix("itm: ") for l in full.splitlines() if l.startswith("itm: ")]
        message = errors[-1] if errors else (full.strip().splitlines() or ["the clone failed"])[-1]
        hint = core.clone_hint(full)
        self.outcome_shown = True
        status.update(markup("[$error]$m[/]" + ("\n[$text-muted]$h[/]" if hint else ""), m=message, h=hint or ""))

    def action_cancel(self) -> None:
        if self.proc is not None and self.proc.returncode is None:
            self.stopping = True
            self.query_one("#status", Static).update(markup("[$warning]Stopping the clone…[/]"))
            self.proc.terminate()
            return
        self.dismiss(None)


class ConfirmDeleteScreen(Dialog):
    BINDINGS = [Binding("y", "dismiss(True)", "Delete"), Binding("n", "dismiss(False)", "Keep")]

    def __init__(self, title: str) -> None:
        super().__init__()
        self.task_title = title

    def compose(self) -> ComposeResult:
        with Vertical(id="dialog"):
            yield Static(markup("Delete [b]$t[/b]?", t=self.task_title), classes="dtitle")
            yield Static(markup("[$text-muted]Stops its session and removes its layout, agents and notes. Its project stays. "
                                "This cannot be undone.[/]"))
            yield Static(markup("[$error b]y[/] delete   [$accent b]n[/] keep"), classes="dhint")

    def action_cancel(self) -> None:
        self.dismiss(False)


class ConfirmArchiveScreen(Dialog):
    """Archiving stops a running session; Ctrl+A is also a common Herdr prefix, so ask first."""

    BINDINGS = [Binding("y", "dismiss(True)", "Archive"), Binding("n", "dismiss(False)", "Keep")]

    def __init__(self, title: str) -> None:
        super().__init__()
        self.task_title = title

    def compose(self) -> ComposeResult:
        with Vertical(id="dialog"):
            yield Static(markup("Archive [b]$t[/b]?", t=self.task_title), classes="dtitle")
            yield Static(markup("[$text-muted]Stops its session and the programs running in it. Reopening restores the "
                                "layout and resumes agents that support it.[/]"))
            yield Static(markup("[$warning b]y[/] archive   [$accent b]n[/] keep"), classes="dhint")

    def action_cancel(self) -> None:
        self.dismiss(False)


class RemoveProjectScreen(Dialog):
    """Returns "keep", "if-clean" (delete a clean clone) or None.

    Follows `itm project remove`: a clone with local work, a missing clone or
    one itm may not delete is only ever kept.
    """

    def __init__(self, info: dict) -> None:
        super().__init__()
        self.info = info

    def compose(self) -> ComposeResult:
        info = self.info
        with Vertical(id="dialog", classes="wide"):
            yield Static(markup("Remove project [b]$n[/b]?", n=info["name"]), classes="dtitle")
            if info["users"]:
                yield Static(markup("[$error]Used by $c task(s), archived ones included:[/]\n$t\n\n"
                                    "[$text-muted]Delete those tasks first. Removing a project never touches a task.[/]",
                                    c=len(info["users"]), t="\n".join(f"  · {u}" for u in info["users"])))
                yield Static(markup("[$accent b]esc[/] back"), classes="dhint")
                return
            forget = Option(Content("Forget the project"), id="keep")
            if info["kind"] == "dir":
                yield Static(markup("[$text-muted]Forgets the folder $p. Nothing is deleted.[/]",
                                    p=core.display_path(info["path"])))
                options = [forget]
            elif info["clone"] == "missing":
                yield Static(markup("[$text-muted]Its clone is already gone.[/]"))
                options = [forget]
            elif info["clone"] == "foreign":
                yield Static(markup("[$text-muted]Its folder $p is kept: it is not a clone itm may delete.[/]",
                                    p=core.display_path(info["path"])))
                options = [forget]
            elif info["local_work"]:
                yield Static(markup("[$text-muted]Clone:[/] $p", p=core.display_path(info["path"])))
                yield Static(markup("[$warning]It has $w, so the clone is kept.[/]\n[$text-muted]To delete it anyway: "
                                    "itm project remove $n --delete-clone[/]", w=info["local_work"], n=info["name"]))
                options = [Option(Content("Forget the project, keep the clone"), id="keep")]
            else:
                yield Static(markup("[$text-muted]Clone:[/] $p", p=core.display_path(info["path"])))
                options = [Option(markup("Keep the clone  [$text-muted](default)[/]"), id="keep"),
                           Option(markup("[$error]Delete the clone[/]"), id="if-clean")]
            yield OptionList(*options, id="clone")
            yield Static(markup("[$accent b]enter[/] remove   [$accent b]esc[/] cancel, nothing changes"), classes="dhint")

    @on(OptionList.OptionSelected, "#clone")
    def chosen(self, event: OptionList.OptionSelected) -> None:
        self.dismiss(event.option.id)


class RenameProjectScreen(Dialog):
    """Returns the new name, or None."""

    def __init__(self, project_id: str, name: str) -> None:
        super().__init__()
        self.project_id = project_id
        self.old = name
        self.data = core.Store().load()

    def compose(self) -> ComposeResult:
        with Vertical(id="dialog", classes="wide"):
            yield Static(markup("Rename project [b]$n[/b]", n=self.old), classes="dtitle")
            with Horizontal(classes="field"):
                yield Static("New name", classes="label")
                yield Input(value=self.old, id="pname", compact=True)
            yield Static(markup("[$text-muted]Its tasks keep it: they refer to the project's id, not its name.[/]"),
                         id="status")
            yield Static(markup("[$accent b]enter[/] rename   [$accent b]esc[/] cancel"), classes="dhint")

    def problem(self, name: str) -> str | None:
        try:
            core.check_name_free(self.data, core.check_project_name(name), exclude=self.project_id)
        except core.ItmError as exc:
            return str(exc)
        return None

    @on(Input.Changed, "#pname")
    def check(self, event: Input.Changed) -> None:
        problem = self.problem(event.value.strip())
        self.query_one("#status", Static).update(
            markup("[$error]$m[/]", m=problem) if problem else
            markup("[$text-muted]Its tasks keep it: they refer to the project's id, not its name.[/]"))

    @on(Input.Submitted, "#pname")
    def submit(self, event: Input.Submitted) -> None:
        new = event.value.strip()
        if new and new != self.old and self.problem(new) is None:
            self.dismiss(new)


HELP = [
    ("Task list", [
        ("typing", "search titles; @name or @\"a name\" shows one project's tasks"),
        ("↑ ↓ / ctrl+k ctrl+j", "move"), ("enter", "open · wake & open · reopen · create"),
        ("tab / shift+tab", "next / previous view"), ("alt+1…9", "open row 1–9"),
        ("ctrl+n", "new task form"), ("ctrl+e", "edit title, description, project"),
        ("ctrl+z", "snooze"), ("ctrl+u", "wake a snoozed task"), ("ctrl+a", "archive (asks when running)"),
        ("ctrl+d", "delete"), ("ctrl+o", "the task's notes, in your editor"), ("ctrl+p", "projects"),
        ("? / F1", "this help (? while the search is empty)"),
        ("esc", "close"),
    ]),
    ("Task form", [("enter", "create (in Title)"), ("tab", "next field"), ("ctrl+s", "save from any field"),
                   ("enter / typing", "on Project: open the project finder"),
                   ("esc", "cancel; asks when you typed something")]),
    ("Project finder", [("typing", "find a saved project (letters in order, any case)"),
                        ("~/… or /…", "list folders; enter adds one as a project"),
                        ("tab", "go into the highlighted folder"), ("a git URL", "clone it, or pick it when saved"),
                        ("↑ ↓ / ctrl+k ctrl+j", "move"), ("enter", "choose"), ("esc", "back, choice unchanged")]),
    ("Add project", [("tab", "complete a folder path"), ("←→", "git / folder (empty source)"),
                     ("enter", "add (clone)"), ("esc", "back; stops a clone")]),
    ("Projects", [("enter", "show its tasks"), ("ctrl+k ctrl+j", "move"), ("ctrl+n", "add"), ("ctrl+r", "rename"),
                  ("ctrl+d", "remove"), ("esc", "back")]),
]


class HelpScreen(Dialog):
    """All keys; scrolls when the popup is shorter than the list (the close hint stays visible)."""

    BINDINGS = [
        Binding("f1,q,question_mark", "cancel", "Close"),
        Binding("ctrl+j", "scroll(1)", show=False),
        Binding("ctrl+k", "scroll(-1)", show=False),
    ]

    def compose(self) -> ComposeResult:
        parts = []
        for section, keys in HELP:
            parts.append(markup("[b]$s[/b]", s=section))
            parts += [markup("  [$accent b]$k[/] $d", k=f"{key:<20}", d=desc) for key, desc in keys]
            parts.append(Content(""))
        with Vertical(id="dialog", classes="wide"):
            with VerticalScroll(id="helpbody"):
                yield Static(join_lines(parts[:-1]))
            yield Static(markup("[$accent b]esc[/] close   [$accent b]↑ ↓[/] scroll"), classes="dhint")

    def on_mount(self) -> None:
        self.query_one("#helpbody").focus()

    def action_scroll(self, step: int) -> None:
        body = self.query_one("#helpbody", VerticalScroll)
        body.scroll_down() if step > 0 else body.scroll_up()


# ------------------------------------------------------------------ projects
class ProjectsScreen(Screen):
    BINDINGS = [
        Binding("escape", "app.pop_screen", "back"),
        Binding("ctrl+n", "add", "add", priority=True),
        Binding("ctrl+r", "rename", "rename", priority=True),
        Binding("ctrl+d", "remove", "remove", priority=True),
        Binding("ctrl+j", "move(1)", show=False, priority=True),
        Binding("ctrl+k", "move(-1)", show=False, priority=True),
    ]

    def action_move(self, step: int) -> None:
        plist = self.query_one("#plist", OptionList)
        plist.action_cursor_down() if step > 0 else plist.action_cursor_up()

    def compose(self) -> ComposeResult:
        yield Static(markup("[b]Projects[/]  [$text-muted]repositories and folders tasks start in[/]"), id="ptitle")
        yield OptionList(id="plist")
        yield Static("", id="pmsg")
        yield Static(markup("[$accent b]enter[/] show its tasks  [$accent b]^n[/] add  [$accent b]^r[/] rename  "
                            "[$accent b]^d[/] remove  [$accent b]esc[/] back"), id="keys")

    def on_mount(self) -> None:
        self.rebuild()

    def rebuild(self) -> None:
        snap: Snapshot = self.app.snapshot
        plist = self.query_one("#plist", OptionList)
        keep = plist.get_option_at_index(plist.highlighted).id if plist.highlighted is not None else None
        plist.clear_options()
        now = time.time()
        for pid, p in core.projects_by_recent(snap.data["projects"]):
            tasks = [t for t in snap.data["tasks"].values() if t.get("project") == pid]
            open_ = sum(t["state"] != "archived" for t in tasks)
            count = f"{len(tasks)} task{'s' if len(tasks) != 1 else ''}" + (f" · {open_} open" if tasks else "")
            where = core.project_where(p)
            where = where if len(where) <= 44 else "…" + where[-43:]
            missing = "" if snap.folders.get(pid, True) else " · folder missing"
            used = rel(p.get("last_used_at") or now, now)
            plist.add_option(Option(badge(p["name"], 14) + markup(
                "[$text-muted]$k[/] $w [$text-muted]$c used $u$m[/]",
                k=f"{p['kind']:<4}", w=f"{where:<45}", c=f"{count:<16}", u=used, m=missing), id=pid))
        if plist.option_count == 0:
            plist.add_option(Option(markup("[$text-muted]No projects yet. Ctrl+N adds a repository or folder.[/]"),
                                    disabled=True))
            return
        ids = [plist.get_option_at_index(i).id for i in range(plist.option_count)]
        plist.highlighted = ids.index(keep) if keep in ids else 0

    def selected(self) -> tuple[str, dict] | None:
        plist = self.query_one("#plist", OptionList)
        if plist.highlighted is None:
            return None
        pid = plist.get_option_at_index(plist.highlighted).id
        project = self.app.snapshot.data["projects"].get(pid)
        return (pid, project) if project else None

    def message(self, text: str, error: bool = False) -> None:
        self.query_one("#pmsg", Static).update(markup("[$error]$m[/]" if error else "[$text-muted]$m[/]", m=text))

    def action_add(self) -> None:
        def added(project_id: str | None) -> None:
            if project_id:
                self.message("Project added.")
                self.app.refresh_data()

        self.app.push_screen(AddProjectScreen("git"), added)

    def action_rename(self) -> None:
        if not (picked := self.selected()):
            return
        pid, project = picked

        def renamed(new: str | None) -> None:
            if new:
                self.app.run_core(core.rename_project, pid, new, done=lambda r: self.message(f"Renamed {r[0]} to {r[1]}."),
                                  on_error=lambda e: self.message(e, True))

        self.app.push_screen(RenameProjectScreen(pid, project["name"]), renamed)

    def action_remove(self) -> None:
        if not (picked := self.selected()):
            return
        pid, _ = picked

        def ask(info: dict) -> None:
            def chosen(mode: str | None) -> None:
                if mode:
                    self.app.run_core(core.remove_project, pid, mode, done=lambda lines: self.message(" · ".join(lines)),
                                      on_error=lambda e: self.message(e, True))

            self.app.push_screen(RemoveProjectScreen(info), chosen)

        self.app.run_core(core.removal_info, pid, done=ask, on_error=lambda e: self.message(e, True), refresh=False)

    @on(OptionList.OptionSelected, "#plist")
    def show(self, event: OptionList.OptionSelected) -> None:
        project = self.app.snapshot.data["projects"].get(event.option.id)
        if project:
            self.app.pop_screen()
            self.app.query_one("#search", Input).value = filter_token(project["name"]) + " "


# ------------------------------------------------------------------- apps
class ItmApp(App):
    ENABLE_COMMAND_PALETTE = False
    CSS = """
    Screen { background: $background; }
    #top { height: 1; margin: 0 1; }
    #top Tabs { width: 1fr; height: 1; }
    Tabs Tab { padding: 0 1; height: 1; }
    Tabs #tabs-list { min-height: 1; }
    Tabs Underline { display: none; }
    #needs { width: auto; padding: 0 1; }
    #search { margin: 1 1 1 1; border: none; height: 1; padding: 0 1; background: $surface; }
    #search:focus { border: none; background: $surface-lighten-1; }
    #keys { height: 1; margin: 0 1; color: $text-muted; }
    #msg, #pmsg { height: auto; margin: 0 1; }
    #body { height: 1fr; margin: 0 1; }
    #list { width: 63%; border: none; background: $background; padding: 0; }
    #list > .option-list--option-highlighted { background: $surface-lighten-1; text-style: bold; }
    #list:focus > .option-list--option-highlighted { background: $surface-lighten-1; }
    #side { width: 1fr; border-left: vkey $surface-lighten-1; padding: 0 2; }
    #preview { height: 1fr; }
    #pkeys { height: auto; }
    ModalScreen { align: center middle; background: $background 60%; }
    #dialog { width: 64; max-width: 100%; height: auto; max-height: 100%; border: round $accent; background: $surface;
              padding: 0 1; }
    #dialog.wide { width: 84; }
    .dtitle { text-style: bold; padding: 0 0 1 0; }
    .dhint { padding: 1 0 0 0; }
    #presets, #clone { height: auto; max-height: 8; border: none; background: $surface; margin-top: 1; }
    #pchoices { height: auto; max-height: 12; border: none; background: $surface; margin-top: 1; }
    #pquery { background: $surface-lighten-1; padding: 0 1; }
    #project { width: 1fr; height: 1; background: $surface-lighten-1; padding: 0 1; }
    #project:focus { background: $surface-lighten-2; }
    #custom { background: $surface-lighten-1; }
    .field { height: auto; margin-bottom: 1; }
    .field.tall { height: 3; }
    .label { width: 13; color: $text-muted; }
    #title, #source, #pname, #snooze-text { width: 1fr; background: $surface-lighten-1; padding: 0 1; }
    #desc { width: 1fr; height: 3; background: $surface-lighten-1; }
    #snooze { width: 1fr; }
    #kind { margin-bottom: 1; height: 1; }
    #helpbody { height: auto; max-height: 75vh; background: $surface; }
    #ptitle { margin: 1 1; }
    #plist { height: 1fr; margin: 0 1; border: none; background: $background; }
    """

    def on_mount(self) -> None:
        self.theme = os.environ.get("ITM_THEME") or THEME
        try:
            asyncio.get_running_loop().add_signal_handler(signal.SIGHUP, self.hang_up)
        except (NotImplementedError, RuntimeError, ValueError):
            pass

    def hang_up(self) -> None:
        """The popup closed: stop clones (they no longer get its SIGHUP) and leave."""
        terminate_children()
        self.exit()

    def on_unmount(self) -> None:
        terminate_children()


class DialogApp(ItmApp):
    """Shows one dialog and exits with its result (for the CLI)."""

    def __init__(self, dialog: ModalScreen) -> None:
        super().__init__()
        self.dialog = dialog

    def on_mount(self) -> None:
        super().on_mount()
        self.push_screen(self.dialog, self.exit)


class SearchInput(Input):
    """The search box. While empty it leaves "?" to the help binding.

    Textual drops any binding for a key the focused Input would type, so the
    Input itself has to decline the key.
    """

    def check_consume_key(self, key: str, character: str | None) -> bool:
        if character == "?" and not self.value:
            return False
        return super().check_consume_key(key, character)


class Switcher(ItmApp):
    """The task switcher. Exits with ("switch", session) or None."""

    BINDINGS = [
        Binding("enter", "open", "open", priority=True),
        Binding("tab", "view(1)", "view", priority=True),
        Binding("shift+tab", "view(-1)", "view", show=False, priority=True),
        Binding("ctrl+n", "new", "new", priority=True),
        Binding("ctrl+z", "snooze", "snooze", priority=True),
        Binding("ctrl+u", "wake", "wake", priority=True),
        Binding("ctrl+a", "archive", "archive", priority=True),
        Binding("ctrl+d", "delete", "delete", priority=True),
        Binding("ctrl+e", "edit", "edit", priority=True),
        Binding("ctrl+p", "projects", "projects", priority=True),
        Binding("ctrl+o", "notes", "notes", priority=True),
        Binding("f1", "help", "all keys", priority=True),
        # "?" opens help only while the search box is empty (SearchInput); otherwise it is typed.
        Binding("question_mark", "help_if_empty", show=False, priority=True),
        Binding("escape", "quit", "close"),
        Binding("up,ctrl+k", "move(-1)", show=False, priority=True),
        Binding("down,ctrl+j", "move(1)", show=False, priority=True),
    ] + [Binding(f"alt+{n}", f"jump({n})", show=False, priority=True) for n in range(1, 10)]

    MAIN_ACTIONS = {"open", "view", "new", "snooze", "wake", "archive", "delete", "edit", "projects", "notes", "move",
                    "jump", "help", "help_if_empty", "quit"}

    def __init__(self, view: str = "inbox") -> None:
        super().__init__()
        self.view = view if view in dict(VIEWS) else "inbox"
        self.snapshot = Snapshot()
        self.message_until = 0.0
        self.loading = False
        self.load_requested = 0

    def compose(self) -> ComposeResult:
        with Horizontal(id="top"):
            yield Tabs(*[Tab(label, id=key) for key, label in VIEWS], active=self.view)
            yield Static("", id="needs")
        yield SearchInput(placeholder="Search tasks · @project filters · or type a new title", id="search", compact=True)
        with Horizontal(id="body"):
            yield OptionList(id="list")
            with Vertical(id="side"):
                yield Static(markup("[$text-muted]Loading…[/]"), id="preview")
                yield Static("", id="pkeys")  # pinned: a long preview never hides the keys
        yield Static("", id="msg")
        yield Static(markup(
            "[$accent b]enter[/] open  [$accent b]tab[/] view  [$accent b]^n[/] new  [$accent b]^e[/] edit  "
            "[$accent b]^z[/] snooze  [$accent b]^p[/] projects  [$accent b]?[/] all keys"), id="keys")

    def on_mount(self) -> None:
        super().on_mount()
        self.query_one("#search", Input).focus()
        self.refresh_data()
        self.set_interval(REFRESH_SECONDS, self.refresh_data)
        self.set_interval(1.0, self.expire_message)

    def check_action(self, action: str, parameters: tuple) -> bool | None:
        # Main-list keys only act on the main screen, never behind a dialog or another screen.
        if action in self.MAIN_ACTIONS and len(self.screen_stack) > 1:
            return False
        if action == "help_if_empty" and self.search_text():
            return False  # the key goes on to the search box
        return True

    # ---------------------------------------------------------------- data
    def refresh_data(self) -> None:
        """Load a snapshot in a thread; at most one at a time, newest wins."""
        self.load_requested += 1
        if self.loading:
            return  # the running load is followed by another one (see _loaded)
        self.loading = True
        self.run_worker(lambda seq=self.load_requested: self._load(seq), thread=True, group="load",
                        exit_on_error=False)

    def _load(self, seq: int) -> None:
        # Any failure must reach _loaded, or `loading` stays set and refreshing stops for good.
        try:
            snap = load_snapshot()
        except Exception as exc:  # noqa: BLE001 - Herdr missing, odd JSON, OSError from the store
            self.call_from_thread(self._loaded, seq, None, str(exc) or type(exc).__name__)
            return
        self.call_from_thread(self._loaded, seq, snap, None)

    def _loaded(self, seq: int, snap: Snapshot | None, error: str | None) -> None:
        self.loading = False
        if error:
            self.flash(error, True)
        elif snap is not None:
            self.apply_snapshot(snap)
        if self.load_requested > seq:  # an action asked for fresher data meanwhile
            self.refresh_data()

    def apply_snapshot(self, snap: Snapshot) -> None:
        self.snapshot = snap
        self.refresh_tabs()
        self.reload()
        if isinstance(self.screen, ProjectsScreen):
            self.screen.rebuild()

    def run_core(self, fn, *args, done=None, on_error=None, refresh=True) -> None:
        """Run a blocking core call in a worker; ``done`` gets its result on the UI thread."""
        def job() -> None:
            try:
                result = fn(*args)
            except core.ItmError as exc:
                self.call_from_thread(on_error or (lambda e: self.flash(e, True)), str(exc))
                return
            if done:
                self.call_from_thread(done, result)
            if refresh:
                self.call_from_thread(self.refresh_data)

        self.run_worker(job, thread=True, group="core", exit_on_error=False)

    def flash(self, text: str, error: bool = False) -> None:
        self.query_one("#msg", Static).update(markup("[$error]$m[/]" if error else "[$success]$m[/]", m=text))
        self.message_until = time.time() + (8 if error else 4)

    def expire_message(self) -> None:
        if self.message_until and time.time() > self.message_until:
            self.query_one("#msg", Static).update("")
            self.message_until = 0.0

    def refresh_tabs(self) -> None:
        tabs = self.query_one("#top Tabs", Tabs)
        tasks = self.snapshot.data["tasks"]
        for key, label in VIEWS:
            n = sum(in_view(t, key) for t in tasks.values())
            tabs.query_one(f"#{key}", Tab).label = f"{label} {n}"
        needs = self.query_one("#needs", Static)
        if self.snapshot.loaded and not self.snapshot.listed:
            daemon = "" if self.snapshot.fresh else " · daemon not running"
            needs.update(markup("[$warning]Herdr not answering · sessions unknown$d[/]", d=daemon))
            return
        if self.snapshot.loaded and not self.snapshot.fresh:
            needs.update(markup("[$warning]daemon not running · waits unknown[/]"))
            return
        count = sum(1 for name, t in tasks.items() if t["state"] != "archived" and blocked_agents(self.snapshot, name))
        needs.update(markup("[$error b]⚑ $c needs you[/]", c=count) if count else "")

    # --------------------------------------------------------------- query
    def search_text(self) -> str:
        return self.query_one("#search", Input).value

    def filter_project(self) -> tuple[str | None, str | None]:
        """(project id named by @token, the token when it names no project)."""
        tokens, _ = parse_query(self.search_text())
        if not tokens:
            return None, None
        try:
            return core.resolve_project(self.snapshot.data, tokens[0]), None
        except core.ItmError:
            return None, tokens[0]

    def default_project(self) -> str | None:
        project, unknown = self.filter_project()
        if project or unknown:
            return project
        current = self.snapshot.data["tasks"].get(self.snapshot.current or "")
        return current.get("project") if current else None

    def visible(self) -> list[tuple[str, dict]]:
        _, text = parse_query(self.search_text())
        project, unknown = self.filter_project()
        if unknown:
            return []
        snap = self.snapshot
        rows = [
            (name, task) for name, task in snap.data["tasks"].items()
            if in_view(task, self.view) and text.lower() in task["title"].lower()
            and (project is None or task.get("project") == project)
        ]
        order = {"active": 0, "snoozed": 1, "archived": 2}
        return sorted(rows, key=lambda r: (not blocked_agents(snap, r[0]), order[r[1]["state"]],
                                           -r[1].get("last_used_at", 0)))

    def reload(self) -> None:
        olist = self.query_one("#list", OptionList)
        keep = self.highlighted_id()
        olist.clear_options()
        if not self.snapshot.loaded:
            return
        now = time.time()
        rows = self.visible()
        snap = self.snapshot
        if self.view == "inbox" and rows:
            needs = [r for r in rows if blocked_agents(snap, r[0])]
            rest = [r for r in rows if not blocked_agents(snap, r[0])]
            if needs:
                olist.add_option(Option(markup("[$error b]NEEDS YOU[/]"), disabled=True))
                olist.add_options([Option(task_row(snap, n, t, now), id=n) for n, t in needs])
            if rest:
                olist.add_option(Option(markup("[$text-muted b]ACTIVE[/]"), disabled=True))
                olist.add_options([Option(task_row(snap, n, t, now), id=n) for n, t in rest])
        else:
            olist.add_options([Option(task_row(snap, n, t, now), id=n) for n, t in rows])
        _, text = parse_query(self.search_text())
        project, unknown = self.filter_project()
        if unknown:
            olist.add_option(Option(markup("[$warning]No project named “$u”[/]", u=unknown), disabled=True))
        if not rows and text:
            into = project_name(snap, self.default_project())
            label = markup("[$accent]+[/] Create task “$q”", q=text)
            if into:
                label = label + markup(" in [$secondary]◆ $p[/]", p=into)
            olist.add_option(Option(label, id=NEW_ROW))
        ids = [olist.get_option_at_index(i).id for i in range(olist.option_count)]
        enabled = [i for i in range(olist.option_count) if not olist.get_option_at_index(i).disabled]
        if keep in ids and not olist.get_option_at_index(ids.index(keep)).disabled:
            olist.highlighted = ids.index(keep)
        else:
            olist.highlighted = enabled[0] if enabled else None
        self.show_preview()

    def highlighted_id(self) -> str | None:
        olist = self.query_one("#list", OptionList)
        if olist.highlighted is None or olist.highlighted >= olist.option_count:
            return None
        return olist.get_option_at_index(olist.highlighted).id

    def current_task(self) -> tuple[str, dict] | None:
        name = self.highlighted_id()
        task = self.snapshot.data["tasks"].get(name or "")
        return (name, task) if task else None

    def show_preview(self) -> None:
        if (preview := self.query_one_optional("#preview", Static)) is None:
            return  # a late highlight event while the app is closing
        picked = self.current_task()
        snap = self.snapshot
        self.query_one("#pkeys", Static).update("")
        if not picked:
            _, text = parse_query(self.search_text())
            if text:
                preview.update(markup("[$text-muted]No task matches.\n\nPress [b]enter[/b] to create “$q”.[/]", q=text))
            elif not snap.data["tasks"]:
                preview.update(markup("[$text-muted]No tasks yet. Type a title and press enter, or ctrl+n.[/]"))
            else:
                preview.update("")
            return
        name, task = picked
        now = time.time()
        parts = [markup("[b]$t[/b]", t=task["title"])]
        project = snap.data["projects"].get(task.get("project") or "")
        if project:
            parts.append(markup("[$secondary]◆ $n[/]  [$text-muted]$k[/]", n=project["name"], k=project["kind"]))
            parts.append(markup("[$text-muted]$w[/]", w=core.project_where(project)))
            if not snap.folders.get(task["project"], True):
                parts.append(markup("[$error]Its folder is missing; the session starts in $d instead.[/]",
                                    d=core.display_path(core.task_dir())))
        if task.get("description"):
            parts.append(Content(""))
            lines = task["description"].splitlines()
            parts += [markup("[$text-muted]$l[/]", l=line) for line in lines[:5]]
            if len(lines) > 5:
                parts.append(markup("[$text-muted]…[/]"))
        parts.append(Content(""))
        if task["state"] == "snoozed":
            until = task.get("snoozed_until")
            parts.append(markup("[$warning]◔ snoozed until $w[/] [$text-muted]($r)[/]", w=when(until), r=rel(until, now))
                         if until else markup("[$warning]◔ snoozed until an agent needs you[/]"))
            parts.append(Content(""))
        elif task["state"] == "archived":
            parts.append(markup("[$text-muted]▣ archived $w · layout and agents saved[/]",
                                w=rel(task.get("archived_at") or now, now)))
            parts.append(Content(""))
        agents = snap.status.get(name, {}).get("agents", [])
        for agent in agents:
            state = agent.get("status", "unknown")
            if state == "blocked":
                parts.append(markup("[$error]⚑[/] $a [$error]waiting $s[/]", a=f"{agent.get('agent', '?'):<7}",
                                    s=short(agent.get("since") or now, now)))
            else:
                icon = {"working": "[$success]●[/]"}.get(state, "[$text-muted]○[/]")
                parts.append(markup(f"{icon} $a [$text-muted]$s[/]", a=f"{agent.get('agent', '?'):<7}", s=state))
            if agent.get("title"):
                parts.append(markup("  [$text-muted]$t[/]", t=agent["title"]))
        if not agents:
            if not snap.listed:
                parts.append(markup("[$warning]Session unknown: Herdr is not answering[/]"))
            elif name not in snap.sessions:
                folder = core.display_path(project["path"]) if project else core.display_path(core.task_dir())
                parts.append(markup("[$text-muted]Not started yet · starts in $f[/]", f=folder))
            elif not snap.sessions[name].get("running"):
                parts.append(markup("[$text-muted]Session stopped · opens where you left it[/]"))
            elif snap.fresh:
                parts.append(markup("[$text-muted]No agents running[/]"))
            else:
                parts.append(markup("[$warning]Agents unknown: the daemon is not running[/]"))
        parts += [Content(""), *notes_preview(name, task, now)]
        parts += [Content(""), markup("[$text-muted]used $u[/]", u=rel(task.get("last_used_at") or now, now))]
        preview.update(join_lines(parts))
        action = {"snoozed": "wake & open", "archived": "reopen"}.get(task["state"], "open")
        self.query_one("#pkeys", Static).update(markup("[$accent b]enter[/] $a   [$accent b]ctrl+o[/] notes", a=action))

    # -------------------------------------------------------------- events
    @on(Input.Changed, "#search")
    def search_changed(self) -> None:
        self.reload()

    @on(OptionList.OptionHighlighted, "#list")
    def row_changed(self) -> None:
        self.show_preview()

    @on(Tabs.TabActivated, "#top Tabs")
    def tab_changed(self, event: Tabs.TabActivated) -> None:
        self.view = event.tab.id
        self.reload()

    # ------------------------------------------------------------- actions
    def action_view(self, step: int) -> None:
        keys = [k for k, _ in VIEWS]
        self.query_one("#top Tabs", Tabs).active = keys[(keys.index(self.view) + step) % len(keys)]

    def action_move(self, step: int) -> None:
        olist = self.query_one("#list", OptionList)
        olist.action_cursor_down() if step > 0 else olist.action_cursor_up()

    def action_jump(self, n: int) -> None:
        olist = self.query_one("#list", OptionList)
        tasks = [i for i in range(olist.option_count)
                 if not olist.get_option_at_index(i).disabled and olist.get_option_at_index(i).id != NEW_ROW]
        if n <= len(tasks):
            olist.highlighted = tasks[n - 1]
            self.action_open()

    def switch(self, name: str) -> None:
        """Leave the TUI and switch to ``name`` (after the terminal is restored)."""
        if not self.snapshot.can_switch:
            self.flash(self.snapshot.switch_problem or "this client cannot be switched", True)
            return
        self.exit(("switch", name))

    def action_open(self) -> None:
        if self.highlighted_id() == NEW_ROW:
            _, text = parse_query(self.search_text())
            self.create({"title": text, "description": None, "project": self.default_project(), "until": None})
            return
        if picked := self.current_task():
            self.switch(picked[0])

    def create(self, answers: dict | None) -> None:
        if not answers:
            return

        def created(name: str) -> None:
            if answers["until"] is not None:
                self.query_one("#search", Input).value = ""
                self.flash(f"Saved “{answers['title']}”, snoozed {core.describe_snooze(answers['until'])}.")
            elif self.snapshot.can_switch:
                self.exit(("switch", name))
            else:
                self.query_one("#search", Input).value = ""
                self.flash(f"Saved “{answers['title']}” without switching: {self.snapshot.switch_problem}", True)

        self.run_core(core.create_task, answers["title"], answers["description"], answers["project"], answers["until"],
                      done=created)

    def action_new(self) -> None:
        _, text = parse_query(self.search_text())
        cannot = None if self.snapshot.can_switch else self.snapshot.switch_problem
        self.push_screen(TaskForm(text, project=self.default_project(), cannot_switch=cannot), self.create)

    def leave_if_current(self, name: str) -> None:
        """After moving this client off its own task, the popup must not stay open in that session."""
        if name == self.snapshot.current:
            self.exit()

    def action_snooze(self) -> None:
        if not (picked := self.current_task()):
            return
        name, task = picked

        def chosen(until) -> None:
            if until is not None:
                self.run_core(core.snooze_task, name, until,
                              done=lambda _: (self.flash(f"Snoozed “{task['title']}” {core.describe_snooze(until)}."),
                                              self.leave_if_current(name)))

        problem = core.agent_snooze_problem(name, self.snapshot.sessions, self.snapshot.status)
        self.push_screen(SnoozeScreen(task["title"], agent_problem=problem), chosen)

    def action_wake(self) -> None:
        if (picked := self.current_task()) and picked[1]["state"] == "snoozed":
            self.run_core(core.wake_task, picked[0], done=lambda _: self.flash(f"“{picked[1]['title']}” is active."))

    def action_archive(self) -> None:
        if not (picked := self.current_task()) or picked[1]["state"] == "archived":
            return
        name, task = picked

        def archive(yes: bool = True) -> None:
            if yes:
                self.run_core(core.archive_task, name,
                              done=lambda _: (self.flash(f"Archived “{task['title']}”."), self.leave_if_current(name)))

        if self.snapshot.sessions.get(name, {}).get("running"):
            self.push_screen(ConfirmArchiveScreen(task["title"]), archive)
        else:
            archive()

    def action_delete(self) -> None:
        if not (picked := self.current_task()):
            return
        name, task = picked

        def confirmed(yes: bool) -> None:
            if yes:
                self.run_core(core.delete_task, name,
                              done=lambda _: (self.flash(f"Deleted “{task['title']}”."), self.leave_if_current(name)))

        self.push_screen(ConfirmDeleteScreen(task["title"]), confirmed)

    def action_edit(self) -> None:
        if not (picked := self.current_task()):
            return
        name, task = picked
        effect = core.project_change_effect(name, self.snapshot.sessions) if self.snapshot.listed else None

        def done(notes: list[str]) -> None:
            failed = any(note.startswith("could not") or " is missing" in note for note in notes)
            self.flash("Saved" + "".join(f"; {n}" for n in notes) + ".", error=failed)

        def saved(answers: dict | None) -> None:
            if answers:
                self.run_core(core.edit_task, name, answers["title"], answers["description"],
                              answers.get("project", core.UNSET), done=done)

        self.push_screen(TaskForm(task=task, task_name=name, session_effect=effect), saved)

    def action_projects(self) -> None:
        self.push_screen(ProjectsScreen())

    def action_notes(self) -> None:
        """Edit the highlighted task's notes; the app steps aside while the editor runs."""
        if not (picked := self.current_task()):
            return
        name = picked[0]
        try:
            data = core.Store().load()  # a task deleted meanwhile must not get its notes back
            if name not in data["tasks"]:
                raise core.ItmError("this task no longer exists")
            path = core.ensure_notes(name, data["tasks"][name], data["projects"])
            command = core.editor_command(path)
        except (core.ItmError, OSError) as exc:
            self.flash(str(exc), True)
            return
        failed = None
        try:
            # The event loop waits on purpose: nothing is drawn while the editor
            # owns the terminal. No lock is held. An exception must not leave the
            # block, or the app would stay suspended.
            with self.suspend():
                try:
                    subprocess.run(command, check=False, env=core.editor_env())
                except OSError as exc:
                    failed = f"could not run {command[0]}: {exc}"
                except KeyboardInterrupt:  # Ctrl+C with an editor that does not take the keyboard (code -w)
                    failed = "the editor was interrupted"
        except SuspendNotSupported:
            failed = f"cannot open an editor here; the notes are in {core.display_path(path)}"
        if failed:
            self.flash(failed, True)
        self.show_preview()

    def action_help(self) -> None:
        self.push_screen(HelpScreen())

    def action_help_if_empty(self) -> None:
        self.action_help()


# ----------------------------------------------------------- entry points
def run(app: App):
    # A clone child inherits ignored signals, and an ignored SIGTERM would stay
    # ignored during its clone, so Esc could not stop it.
    if signal.getsignal(signal.SIGTERM) is signal.SIG_IGN:
        signal.signal(signal.SIGTERM, signal.SIG_DFL)
    try:
        return app.run()
    finally:
        terminate_children()


def switcher(view: str = "inbox") -> tuple[str, str] | None:
    return run(Switcher(view))


def task_form(title: str = "", description: str | None = None, project: str | None = None,
              until: int | str | None = None, cannot_switch: str | None = None) -> dict | None:
    return run(DialogApp(TaskForm(title, description, project, until, cannot_switch=cannot_switch)))


def edit_form(name: str) -> dict | None:
    data = core.Store().load()
    try:
        sessions = core.list_sessions()
    except core.ItmError:
        sessions = None  # Herdr did not answer: the form says the session state is unknown
    effect = core.project_change_effect(name, sessions) if sessions is not None else None
    form = TaskForm(task=data["tasks"][name], task_name=name, session_effect=effect)
    return run(DialogApp(form))


def snooze_dialog(title: str, agent_problem: str | None = None) -> int | str | None:
    return run(DialogApp(SnoozeScreen(title, agent_problem=agent_problem)))


def confirm_delete(title: str) -> bool:
    return bool(run(DialogApp(ConfirmDeleteScreen(title))))


def remove_project_dialog(info: dict) -> str | None:
    return run(DialogApp(RemoveProjectScreen(info)))
