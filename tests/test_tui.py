"""The Textual screens, driven headlessly against real task data in a temporary home."""

import asyncio
import contextlib
import json
import os
import sys
import tempfile
import threading
import time
import unittest
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))

import inbox_task_manager as core  # noqa: E402
from inbox_task_manager import tui  # noqa: E402
from textual import events  # noqa: E402
from textual.widgets import Input, OptionList, Select, Static  # noqa: E402


def text_of(widget) -> str:
    return str(widget.render())


class TuiTest(unittest.IsolatedAsyncioTestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name).resolve()  # macOS: /private symlink, see test_inbox_task_manager
        env = {
            "XDG_STATE_HOME": str(self.root / "state"),
            "XDG_DATA_HOME": str(self.root / "data"),
            "XDG_RUNTIME_DIR": str(self.root / "run"),
            "XDG_CONFIG_HOME": str(self.root / "config"),
            "ITM_TASK_DIR": str(self.root / "home"),
        }
        self.env = mock.patch.dict(os.environ, env)
        self.env.start()
        for name in ("HERDR_SOCKET_PATH", "ITM_PROJECTS_DIR", "ITM_POPUP", "ITM_SELF"):
            os.environ.pop(name, None)
        self.sessions = {}
        self.patches = [
            mock.patch.object(core, "list_sessions", lambda: self.sessions),
            mock.patch.object(core, "handoff", return_value=False),
            mock.patch.object(core, "stop_session_detached"),
        ]
        for patch in self.patches:
            patch.start()
        self.stopped = self.patches[2].new if hasattr(self.patches[2], "new") else None
        self.store = core.Store()
        now = core.now()
        with self.store.edit() as data:
            data["tasks"]["task-a"] = {**core.new_task("Fix login"), "last_used_at": now - 60}
            data["tasks"]["task-b"] = {**core.new_task("Write docs"), "last_used_at": now - 30}
            data["tasks"]["task-c"] = {**core.new_task("Later thing"), "state": "snoozed", "snoozed_until": now + 3600}
            data["tasks"]["task-d"] = {**core.new_task("Old thing"), "state": "archived", "archived_at": now - 86400}
        self.write_status({"task-a": {"blocked": 1, "agents": [
            {"agent": "claude", "status": "blocked", "title": "Investigate", "pane_id": "p1", "since": now - 240}]}})

    def tearDown(self):
        for patch in self.patches:
            patch.stop()
        self.env.stop()
        self.tmp.cleanup()

    def write_status(self, sessions):
        core.write_atomic(core.status_path(), json.dumps({"updated_at": core.now(), "sessions": sessions}))

    def folder(self, name):
        path = self.root / name
        path.mkdir(parents=True, exist_ok=True)
        return path

    def labels(self, app):
        olist = app.query_one("#list", OptionList)
        return [(o.id, str(o.prompt)) for o in (olist.get_option_at_index(i) for i in range(olist.option_count))]

    async def settle(self, pilot, app):
        for _ in range(3):
            await app.workers.wait_for_complete()
            await pilot.pause()

    async def typed(self, pilot, text):
        for ch in text:
            await pilot.press({" ": "space", "@": "at", "/": "slash", ":": "colon", ".": "full_stop"}.get(ch, ch))

    def task(self, name):
        return self.store.load()["tasks"][name]

    def choices(self, app):
        """The picker's rows as text."""
        olist = app.screen.query_one("#pchoices", OptionList)
        return [str(olist.get_option_at_index(i).prompt) for i in range(olist.option_count)]

    # ---------------------------------------------------------------- list
    async def test_inbox_sections_counts_and_preview(self):
        app = tui.Switcher()
        async with app.run_test(size=(110, 30)) as pilot:
            await self.settle(pilot, app)
            labels = self.labels(app)
            self.assertEqual([i for i, _ in labels], [None, "task-a", None, "task-b"])
            self.assertIn("NEEDS YOU", labels[0][1])
            self.assertIn("claude waiting · 4m", labels[1][1])
            self.assertIn("1 needs you", text_of(app.query_one("#needs")))
            tabs = [str(t.label) for t in app.query("#top Tab")]
            self.assertEqual(tabs, ["Inbox 2", "Snoozed 1", "Archived 1", "All 4"])
            preview = text_of(app.query_one("#preview"))
            self.assertIn("waiting 4m", preview)
            self.assertIn("enter open   ctrl+o notes", text_of(app.query_one("#pkeys")))

    async def test_stale_daemon_is_said(self):
        core.status_path().unlink()
        app = tui.Switcher()
        async with app.run_test(size=(110, 30)) as pilot:
            await self.settle(pilot, app)
            self.assertIn("daemon not running", text_of(app.query_one("#needs")))
            self.assertNotIn("NEEDS YOU", str(self.labels(app)))

    async def test_herdr_not_answering_is_said(self):
        with mock.patch.object(core, "list_sessions", side_effect=core.ItmError("herdr not found")):
            app = tui.Switcher()
            async with app.run_test(size=(110, 30)) as pilot:
                await self.settle(pilot, app)
                self.assertIn("Herdr not answering", text_of(app.query_one("#needs")))
                self.assertNotIn("daemon not running", text_of(app.query_one("#needs")))  # the status cache is fresh
                self.assertIn("session unknown", str(self.labels(app)))

    async def test_herdr_and_daemon_both_down_are_both_said(self):
        core.status_path().unlink()
        with mock.patch.object(core, "list_sessions", side_effect=core.ItmError("herdr not found")):
            app = tui.Switcher()
            async with app.run_test(size=(110, 30)) as pilot:
                await self.settle(pilot, app)
                needs = text_of(app.query_one("#needs"))
                self.assertIn("Herdr not answering", needs)
                self.assertIn("daemon not running", needs)

    async def test_enter_opens_after_closing(self):
        app = tui.Switcher()
        async with app.run_test(size=(110, 30)) as pilot:
            await self.settle(pilot, app)
            await pilot.press("enter")
        self.assertEqual(app.return_value, ("switch", "task-a"))

    async def test_views_keep_the_search(self):
        app = tui.Switcher()
        async with app.run_test(size=(110, 30)) as pilot:
            await self.settle(pilot, app)
            app.query_one("#search", Input).value = "thing"
            await pilot.press("tab")
            await pilot.pause()
            self.assertEqual(app.view, "snoozed")
            self.assertEqual([i for i, _ in self.labels(app)], ["task-c"])
            self.assertIn("wake & open", text_of(app.query_one("#pkeys")))

    # ------------------------------------------------------------- create
    async def test_create_row_uses_the_filtered_project(self):
        api = core.add_project("dir", str(self.folder("code/api")), "api")
        app = tui.Switcher()
        async with app.run_test(size=(110, 30)) as pilot:
            await self.settle(pilot, app)
            app.query_one("#search", Input).value = "@API Rate limits"
            await pilot.pause()
            self.assertIn("in ◆ api", self.labels(app)[-1][1])
            await pilot.press("enter")
            await self.settle(pilot, app)
        self.assertEqual(app.return_value, ("switch", "task-rate-limits"))
        self.assertEqual(self.task("task-rate-limits")["projects"], [api])

    async def test_unknown_project_filter(self):
        self.assertEqual(tui.parse_query('@"my app" fix @api'), (["my app", "api"], "fix"))
        app = tui.Switcher()
        async with app.run_test(size=(110, 30)) as pilot:
            await self.settle(pilot, app)
            app.query_one("#search", Input).value = "@nope Fix"
            await pilot.pause()
            labels = [l for _, l in self.labels(app)]
            self.assertIn("No project named “nope”", labels[0])
            self.assertTrue(labels[-1].startswith("+ Create task “Fix”") and " in " not in labels[-1])
            await pilot.press("ctrl+n")
            await pilot.pause()
            self.assertEqual(app.screen.query_one("#project", tui.ProjectField).value, ())

    async def test_form_creates_with_description_and_snooze(self):
        app = tui.Switcher()
        async with app.run_test(size=(110, 34)) as pilot:
            await self.settle(pilot, app)
            await pilot.press("ctrl+n")
            await self.typed(pilot, "Plan launch")
            await pilot.press("ctrl+a", "tab")  # ctrl+a stays in the form
            await self.typed(pilot, "one")
            await pilot.press("enter")
            await self.typed(pilot, "two")
            # By label: presets are in time order, so "This evening" comes before "1 hour" after 19:00.
            hour = [label for label, _ in app.screen.presets].index("1 hour")
            app.screen.query_one("#snooze", Select).value = f"p{hour}"
            await pilot.press("ctrl+s")
            await self.settle(pilot, app)
            self.assertEqual(len(app.screen_stack), 1)  # snoozed: no switch
        task = self.task("task-plan-launch")
        self.assertEqual((task["description"], task["state"]), ("one\ntwo", "snoozed"))
        self.assertGreater(task["snoozed_until"], core.now() + 3000)
        self.assertEqual(self.task("task-a")["state"], "active")

    async def test_form_esc_asks_only_after_typing(self):
        app = tui.Switcher()
        async with app.run_test(size=(110, 34)) as pilot:
            await self.settle(pilot, app)
            await pilot.press("ctrl+n", "escape")
            await pilot.pause()
            self.assertEqual(len(app.screen_stack), 1)
            await pilot.press("ctrl+n", "x", "escape")
            await pilot.pause()
            self.assertEqual(type(app.screen).__name__, "DiscardScreen")
            await pilot.press("n")
            await pilot.pause()
            self.assertEqual(type(app.screen).__name__, "TaskForm")
            await pilot.press("escape", "y")
            await pilot.pause()
            self.assertEqual(len(app.screen_stack), 1)

    async def test_form_rejects_a_bad_time(self):
        app = tui.Switcher()
        async with app.run_test(size=(110, 34)) as pilot:
            await self.settle(pilot, app)
            await pilot.press("ctrl+n")
            await self.typed(pilot, "Soon")
            app.screen.query_one("#snooze", Select).value = "other"
            await pilot.pause()
            await self.typed(pilot, "soonish")
            await pilot.press("enter")
            await pilot.pause()
            self.assertEqual(type(app.screen).__name__, "TaskForm")
            self.assertIn("cannot understand", text_of(app.screen.query_one("#error")))

    async def test_plain_herdr_client_saves_without_switching(self):
        with mock.patch.dict(os.environ, {"HERDR_SOCKET_PATH": "/sock"}), \
                mock.patch.object(core, "switchable_session", side_effect=core.ItmError("not itm")):
            app = tui.Switcher()
            async with app.run_test(size=(110, 30)) as pilot:
                await self.settle(pilot, app)
                app.query_one("#search", Input).value = "Kept"
                await pilot.pause()
                await pilot.press("enter")
                await self.settle(pilot, app)
                self.assertIn("without switching: not itm", text_of(app.query_one("#msg")))
                await pilot.press("up", "enter")  # opening needs a switch too
                await pilot.pause()
                self.assertIn("not itm", text_of(app.query_one("#msg")))
        self.assertIsNone(app.return_value)
        self.assertEqual(self.task("task-kept")["title"], "Kept")

    # ------------------------------------------------------------ dialogs
    async def test_dialog_keys_stay_in_the_dialog(self):
        app = tui.Switcher()
        async with app.run_test(size=(110, 30)) as pilot:
            await self.settle(pilot, app)
            await pilot.press("ctrl+z", "ctrl+a", "tab")
            await self.settle(pilot, app)
            self.assertEqual(type(app.screen).__name__, "SnoozeScreen")
            self.assertEqual((self.task("task-a")["state"], app.view), ("active", "inbox"))
            await pilot.press("escape")
            await pilot.press("ctrl+z")
            await self.typed(pilot, "2h")
            await pilot.press("enter")
            await self.settle(pilot, app)
        task = self.task("task-a")
        self.assertEqual(task["state"], "snoozed")
        self.assertAlmostEqual(task["snoozed_until"], core.now() + 7200, delta=5)

    async def test_snooze_until_an_agent(self):
        self.sessions["task-b"] = {"running": True}  # running, no agent waiting
        app = tui.Switcher()
        async with app.run_test(size=(110, 30)) as pilot:
            await self.settle(pilot, app)
            await pilot.press("down", "ctrl+z")
            presets = app.screen.query_one("#presets", OptionList)
            presets.highlighted = presets.option_count - 1
            await pilot.press("enter")
            await self.settle(pilot, app)
        task = self.task("task-b")
        self.assertEqual(task["state"], "snoozed")
        self.assertNotIn("snoozed_until", task)

    async def test_archive_delete_and_wake(self):
        app = tui.Switcher()
        async with app.run_test(size=(110, 30)) as pilot:
            await self.settle(pilot, app)
            await pilot.press("down", "ctrl+a")  # task-b (the header is skipped)
            await self.settle(pilot, app)
            self.assertEqual(self.task("task-b")["state"], "archived")
            core.stop_session_detached.assert_called_with("task-b")
            await pilot.press("tab")  # snoozed
            await pilot.pause()
            await pilot.press("ctrl+u")
            await self.settle(pilot, app)
            self.assertEqual(self.task("task-c")["state"], "active")
            await pilot.press("shift+tab")
            await pilot.pause()
            doomed = app.highlighted_id()
            await pilot.press("ctrl+d", "y")
            await self.settle(pilot, app)
            core.stop_session_detached.assert_called_with(doomed, delete=True)
        self.assertNotIn(doomed, self.store.load()["tasks"])

    async def test_archiving_a_running_session_asks_first(self):
        self.sessions["task-b"] = {"running": True}
        app = tui.Switcher()
        async with app.run_test(size=(110, 30)) as pilot:
            await self.settle(pilot, app)
            await pilot.press("down", "ctrl+a")
            await pilot.pause()
            self.assertEqual(type(app.screen).__name__, "ConfirmArchiveScreen")
            await pilot.press("d", "n")  # a stray key after the Herdr prefix does nothing
            await self.settle(pilot, app)
            self.assertEqual(self.task("task-b")["state"], "active")
            await pilot.press("ctrl+a", "y")
            await self.settle(pilot, app)
        self.assertEqual(self.task("task-b")["state"], "archived")

    async def test_leaving_the_current_task_closes_the_switcher(self):
        with mock.patch.dict(os.environ, {"HERDR_SOCKET_PATH": "/sock"}), \
                mock.patch.object(core, "current_session", return_value="task-a"), \
                mock.patch.object(core, "switchable_session", return_value="task-a"):
            app = tui.Switcher()
            async with app.run_test(size=(110, 30)) as pilot:
                await self.settle(pilot, app)
                self.assertIn("here", self.labels(app)[1][1])
                await pilot.press("ctrl+a")
                await self.settle(pilot, app)
        self.assertEqual(self.task("task-a")["state"], "archived")
        self.assertIsNone(app.return_value)

    async def test_edit_renames_and_adds_projects_to_a_running_task(self):
        api = core.add_project("dir", str(self.folder("code/api")), "api")
        web = core.add_project("dir", str(self.folder("code/web")), "web")
        old = core.add_project("dir", str(self.folder("code/old")), "old")
        with self.store.edit() as data:
            core.set_task_projects(data["tasks"]["task-a"], [old])
        self.sessions["task-a"] = {"running": True, "socket_path": "/sock/a"}
        with mock.patch.object(core, "open_project_workspace") as open_workspace:
            app = tui.Switcher()
            async with app.run_test(size=(110, 34)) as pilot:
                await self.settle(pilot, app)
                await pilot.press("ctrl+e")
                await pilot.pause()
                app.screen.query_one("#title", Input).value = "Fix login fast"
                app.screen.query_one("#project", tui.ProjectField).value = (api, web)
                await pilot.pause()
                self.assertIn("A workspace opens in", text_of(app.screen.query_one("#where")))
                await pilot.press("ctrl+s")
                await self.settle(pilot, app)
                self.assertIn("opened a workspace in", text_of(app.query_one("#msg")))
        task = self.task("task-a")
        self.assertEqual((task["title"], task["projects"]), ("Fix login fast", [api, web]))
        self.assertEqual([c.args[0] for c in open_workspace.call_args_list], ["/sock/a", "/sock/a"])
        self.assertEqual([c.args[1]["name"] for c in open_workspace.call_args_list], ["api", "web"])

    async def test_edit_form_says_what_a_new_project_does(self):
        api = core.add_project("dir", str(self.folder("code/api")), "api")
        cases = {"starts": "will start with a workspace in", "stopped": "keeps its workspaces", None: "did not answer"}
        for effect, text in cases.items():
            with self.subTest(effect=effect):
                app = tui.DialogApp(tui.TaskForm(task=self.task("task-a"), task_name="task-a", session_effect=effect))
                async with app.run_test(size=(110, 34)) as pilot:
                    await pilot.pause()
                    app.screen.query_one("#project", tui.ProjectField).value = (api,)
                    await pilot.pause()
                    self.assertIn(text, text_of(app.screen.query_one("#where")))
                    await pilot.press("escape")  # only the project changed: still asks
                    await pilot.pause()
                    self.assertEqual(type(app.screen).__name__, "DiscardScreen")

    async def test_failed_edit_notes_are_shown_as_errors(self):
        api = core.add_project("dir", str(self.folder("code/api")), "api")
        self.sessions["task-a"] = {"running": True, "socket_path": "/sock/a"}
        with mock.patch.object(core, "open_project_workspace", side_effect=core.ItmError("no answer")):
            app = tui.Switcher()
            async with app.run_test(size=(110, 34)) as pilot:
                await self.settle(pilot, app)
                with mock.patch.object(app, "flash") as flash:
                    await pilot.press("ctrl+e")
                    await pilot.pause()
                    app.screen.query_one("#project", tui.ProjectField).value = (api,)
                    await pilot.pause()
                    await pilot.press("ctrl+s")
                    await self.settle(pilot, app)
        self.assertIn("could not open a workspace", flash.call_args.args[0])
        self.assertTrue(flash.call_args.kwargs.get("error"))

    async def test_vim_keys_stay_in_dialogs_and_lists(self):
        core.add_project("dir", str(self.folder("a")), "a")
        core.add_project("dir", str(self.folder("b")), "b")
        app = tui.Switcher()
        async with app.run_test(size=(110, 30)) as pilot:
            await self.settle(pilot, app)
            await pilot.press("ctrl+z", "ctrl+j")  # the snooze presets move, not the task list
            self.assertEqual(app.screen.query_one("#presets", OptionList).highlighted, 1)
            await pilot.press("escape")
            await pilot.pause()
            self.assertEqual(app.highlighted_id(), "task-a")
            await pilot.press("ctrl+n")
            await self.typed(pilot, "abc")
            await pilot.press("home", "ctrl+k")  # Ctrl+K still edits inside the form
            self.assertEqual(app.screen.query_one("#title", Input).value, "")
            await pilot.press("escape", "y", "ctrl+p")
            await pilot.pause()
            plist = app.screen.query_one("#plist", OptionList)
            self.assertEqual(plist.highlighted, 0)
            await pilot.press("ctrl+j")
            self.assertEqual(plist.highlighted, 1)
            await pilot.press("ctrl+k")
            self.assertEqual(plist.highlighted, 0)

    async def test_help_scrolls_when_the_popup_is_short(self):
        app = tui.Switcher()
        async with app.run_test(size=(110, 24)) as pilot:
            await self.settle(pilot, app)
            await pilot.press("question_mark")
            await pilot.pause()
            body = app.screen.query_one("#helpbody")
            self.assertGreater(body.max_scroll_y, 0)
            await pilot.press("ctrl+j")
            self.assertGreater(body.scroll_y, 0)
            hint, dialog = app.screen.query_one(".dhint"), app.screen.query_one("#dialog")
            self.assertLessEqual(hint.region.bottom, dialog.region.bottom)

    async def test_vim_keys_and_help(self):
        app = tui.Switcher()
        async with app.run_test(size=(110, 30)) as pilot:
            await self.settle(pilot, app)
            self.assertEqual(app.highlighted_id(), "task-a")
            await pilot.press("ctrl+j")
            self.assertEqual(app.highlighted_id(), "task-b")
            await pilot.press("ctrl+k")
            self.assertEqual(app.highlighted_id(), "task-a")
            await pilot.press("question_mark")  # empty search: help
            await pilot.pause()
            self.assertEqual(type(app.screen).__name__, "HelpScreen")
            await pilot.press("question_mark")  # closes it again
            await pilot.pause()
            await pilot.press("w", "h", "y", "question_mark")  # typed when there is text
            await pilot.pause()
            self.assertEqual((type(app.screen).__name__, app.search_text()), ("Screen", "why?"))
            await pilot.press("f1")
            await pilot.pause()
            self.assertEqual(type(app.screen).__name__, "HelpScreen")
            await pilot.press("escape", "ctrl+p")
            await pilot.pause()
            await pilot.press("ctrl+j")  # the Projects list too (and not the task list behind it)
            await pilot.press("ctrl+z")  # snooze is not a Projects key
            self.assertEqual(type(app.screen).__name__, "ProjectsScreen")

    # ----------------------------------------------------------- projects
    async def test_projects_rename_remove_and_filter(self):
        api = core.add_project("dir", str(self.folder("code/api")), "api")
        notes = core.add_project("dir", str(self.folder("notes")), "notes")
        with self.store.edit() as data:
            data["tasks"]["task-a"]["project"] = api  # as tasks.json had it before projects became a list
        app = tui.Switcher()
        async with app.run_test(size=(110, 30)) as pilot:
            await self.settle(pilot, app)
            await pilot.press("ctrl+p")
            await self.settle(pilot, app)
            plist = app.screen.query_one("#plist", OptionList)
            ids = [plist.get_option_at_index(i).id for i in range(plist.option_count)]
            plist.highlighted = ids.index(api)
            await pilot.press("ctrl+d")
            await self.settle(pilot, app)
            self.assertIn("Used by 1 task", " ".join(text_of(s) for s in app.screen.query(Static)))
            self.assertFalse(app.screen.query("#clone"))  # nothing to choose
            await pilot.press("escape")
            await pilot.press("ctrl+r")
            await pilot.pause()
            box = app.screen.query_one("#pname", Input)
            box.value = "NOTES"
            await pilot.press("enter")
            await pilot.pause()
            self.assertEqual(type(app.screen).__name__, "RenameProjectScreen")  # taken, any case
            box.value = "backend"
            await pilot.press("enter")
            await self.settle(pilot, app)
            self.assertEqual(self.store.load()["projects"][api]["name"], "backend")
            plist = app.screen.query_one("#plist", OptionList)
            ids = [plist.get_option_at_index(i).id for i in range(plist.option_count)]
            plist.highlighted = ids.index(notes)
            await pilot.press("ctrl+d")
            await self.settle(pilot, app)
            await pilot.press("enter")  # "Forget the project"
            await self.settle(pilot, app)
            self.assertNotIn(notes, self.store.load()["projects"])
            plist = app.screen.query_one("#plist", OptionList)
            plist.highlighted = 0
            await pilot.press("enter")
            await pilot.pause()
            self.assertEqual(app.query_one("#search", Input).value, "@backend ")
            self.assertEqual([i for i, _ in self.labels(app) if i], ["task-a"])
        self.assertEqual(self.task("task-a")["projects"], [api])  # the rename kept the task

    async def add_dialog(self, pilot, app, source, name=""):
        await pilot.press("ctrl+p", "ctrl+n")
        await pilot.pause()
        if source.startswith("/"):
            await pilot.press("right")  # folder
        app.screen.query_one("#source", Input).value = source
        app.screen.query_one("#pname", Input).value = name
        await pilot.pause()
        await pilot.press("enter")

    async def wait_for(self, pilot, app, condition, seconds=10):
        deadline = time.time() + seconds
        while time.time() < deadline:
            await pilot.pause(0.1)
            if condition():
                return True
        return False

    async def test_add_folder_and_taken_name(self):
        core.add_project("dir", str(self.folder("code/api")), "api")
        app = tui.Switcher()
        async with app.run_test(size=(110, 30)) as pilot:
            await self.settle(pilot, app)
            await self.add_dialog(pilot, app, str(self.folder("other/api")))
            await pilot.pause()
            self.assertEqual(type(app.screen).__name__, "AddProjectScreen")
            self.assertEqual(app.screen.focused.id, "pname")
            self.assertIn("project already exists: api", text_of(app.screen.query_one("#status")))
            await self.typed(pilot, "api2")
            await pilot.press("enter")
            await self.settle(pilot, app)
            self.assertEqual(type(app.screen).__name__, "ProjectsScreen")
        self.assertEqual(sorted(p["name"] for p in self.store.load()["projects"].values()), ["api", "api2"])

    def fake_clone(self, code):
        """A stand-in for `itm project add git …` run as the clone child."""
        script = self.root / "fake_clone.py"
        script.write_text(f"import sys\nsys.path.insert(0, {str(ROOT)!r})\nimport inbox_task_manager as core\n{code}\n")
        return mock.patch.object(core, "self_command", return_value=[sys.executable, str(script)])

    async def test_clone_runs_as_a_child_and_selects_the_project(self):
        folder = self.folder("clones/backend")
        code = ("print('Receiving objects:  50%', end='\\r', flush=True)\n"
                "name = [a for a in sys.argv if a.startswith('--name=')][0].split('=', 1)[1]\n"
                "assert sys.argv[-2:] == ['--', 'git@github.com:acme/backend.git'], sys.argv\n"
                f"core.add_project('dir', {str(folder)!r}, name); print(name)\n"
                "import os; assert os.environ['GIT_TERMINAL_PROMPT'] == '0'; assert 'ITM_POPUP' not in os.environ")
        with self.fake_clone(code), mock.patch.dict(os.environ, {"ITM_POPUP": "1"}):
            app = tui.Switcher()
            async with app.run_test(size=(110, 34)) as pilot:
                await self.settle(pilot, app)
                await pilot.press("ctrl+n")
                await pilot.pause()
                field = app.screen.query_one("#project", tui.ProjectField)
                field.focus()
                field.post_message(events.Paste("git@github.com:acme/backend.git"))  # a paste opens the picker
                await pilot.pause()
                self.assertEqual(type(app.screen).__name__, "ProjectPicker")
                self.assertIn("+ Clone git@github.com:acme/backend.git", self.choices(app)[0])
                await pilot.press("enter")
                await pilot.pause(0.4)  # validation waits for typing to pause
                self.assertEqual(type(app.screen).__name__, "AddProjectScreen")
                self.assertEqual(app.screen.query_one("#source", Input).value, "git@github.com:acme/backend.git")
                self.assertEqual(app.screen.focused.id, "pname")
                self.assertIn("Clones to", text_of(app.screen.query_one("#status")))
                await pilot.press("enter")
                done = await self.wait_for(pilot, app, lambda: type(app.screen).__name__ == "TaskForm")
                self.assertTrue(done)
                project, = app.screen.query_one("#project", tui.ProjectField).value
        self.assertEqual(self.store.load()["projects"][project]["name"], "backend")
        self.assertEqual(tui._children, set())

    async def test_cancelling_a_clone_stops_the_child(self):
        with self.fake_clone("import time; print('Cloning', flush=True); time.sleep(60)"):
            app = tui.Switcher()
            async with app.run_test(size=(110, 34)) as pilot:
                await self.settle(pilot, app)
                await self.add_dialog(pilot, app, "https://github.com/acme/slow")
                started = await self.wait_for(pilot, app, lambda: app.screen.proc is not None)
                self.assertTrue(started)
                proc = app.screen.proc
                await pilot.press("escape")
                stopped = await self.wait_for(pilot, app, lambda: "Stopped" in text_of(app.screen.query_one("#status")))
                self.assertTrue(stopped)
                self.assertIsNotNone(proc.returncode)
                self.assertEqual(type(app.screen).__name__, "AddProjectScreen")
        self.assertEqual(self.store.load()["projects"], {})

    async def test_failed_clone_shows_why(self):
        code = "print(\"fatal: could not read Username for 'https://github.com'\"); print('itm: could not clone x'); sys.exit(1)"
        with self.fake_clone(code):
            app = tui.Switcher()
            async with app.run_test(size=(110, 34)) as pilot:
                await self.settle(pilot, app)
                await self.add_dialog(pilot, app, "https://github.com/acme/private")
                failed = await self.wait_for(pilot, app, lambda: "could not clone" in text_of(app.screen.query_one("#status")))
                self.assertTrue(failed)
                self.assertIn("in a terminal", text_of(app.screen.query_one("#status")))

    # --------------------------------------------------------- regressions
    async def test_a_failed_session_list_keeps_tombstones(self):
        with self.store.edit() as data:
            data["deleted"]["task-gone"] = core.now()
        self.sessions = {"task-gone": {"running": True}}  # still being deleted
        with mock.patch.object(core, "list_sessions", side_effect=core.ItmError("herdr did not answer")):
            snap = tui.load_snapshot()
        self.assertEqual(snap.sessions, {})
        self.assertIn("task-gone", self.store.load()["deleted"])
        snap = tui.load_snapshot()  # Herdr answers again: still not adopted
        self.assertNotIn("task-gone", snap.data["tasks"])
        self.assertIn("task-gone", self.store.load()["deleted"])

    async def test_exit_during_a_clone_stops_the_child(self):
        with self.fake_clone("import time; print('Cloning', flush=True); time.sleep(60)"):
            app = tui.Switcher()
            async with app.run_test(size=(110, 34)) as pilot:
                await self.settle(pilot, app)
                await self.add_dialog(pilot, app, "https://github.com/acme/slow")
                self.assertTrue(await self.wait_for(pilot, app, lambda: app.screen.proc is not None))
                proc = app.screen.proc
                app.exit()
        for _ in range(50):
            if proc.returncode is not None:
                break
            await asyncio.sleep(0.1)
        self.assertIsNotNone(proc.returncode)
        self.assertEqual(tui._children, set())

    async def test_half_typed_home_folder_is_not_a_crash(self):
        app = tui.Switcher()
        async with app.run_test(size=(110, 30)) as pilot:
            await self.settle(pilot, app)
            await pilot.press("ctrl+p", "ctrl+n", "right")
            await self.typed(pilot, "~no-such-user-x")
            await pilot.pause(0.4)
            self.assertEqual(type(app.screen).__name__, "AddProjectScreen")
            self.assertIn("not a folder", text_of(app.screen.query_one("#status")))

    async def test_names_starting_with_a_dash_reach_the_clone(self):
        folder = self.folder("clones/dash")
        code = (f"name = [a for a in sys.argv if a.startswith('--name=')][0].split('=', 1)[1]\n"
                f"core.add_project('dir', {str(folder)!r}, name); print(name)")
        with self.fake_clone(code):
            app = tui.Switcher()
            async with app.run_test(size=(110, 34)) as pilot:
                await self.settle(pilot, app)
                await self.add_dialog(pilot, app, "https://github.com/acme/dash", name="-dash")
                self.assertTrue(await self.wait_for(pilot, app, lambda: type(app.screen).__name__ == "ProjectsScreen"))
        self.assertEqual([p["name"] for p in self.store.load()["projects"].values()], ["-dash"])

    async def test_a_failing_refresh_does_not_stop_refreshing(self):
        real = tui.load_snapshot
        calls = []

        def flaky():
            calls.append(1)
            if len(calls) == 2:
                raise OSError("disk hiccup")
            return real()

        with mock.patch.object(tui, "load_snapshot", side_effect=flaky):
            app = tui.Switcher()
            async with app.run_test(size=(110, 30)) as pilot:
                await self.settle(pilot, app)
                app.refresh_data()  # this one fails
                await self.settle(pilot, app)
                self.assertIn("disk hiccup", text_of(app.query_one("#msg")))
                self.assertFalse(app.loading)
                app.refresh_data()
                await self.settle(pilot, app)
        self.assertGreaterEqual(len(calls), 3)

    async def test_form_refuses_typing_agent_without_losing_the_text(self):
        app = tui.Switcher()
        async with app.run_test(size=(110, 34)) as pilot:
            await self.settle(pilot, app)
            await pilot.press("ctrl+n")
            await self.typed(pilot, "Keep me")
            app.screen.query_one("#snooze", Select).value = "other"
            await pilot.pause()
            await self.typed(pilot, "agent")
            await pilot.press("enter")
            await pilot.pause()
            self.assertEqual(type(app.screen).__name__, "TaskForm")
            self.assertIn("no agents yet", text_of(app.screen.query_one("#error")))
            self.assertEqual(app.screen.query_one("#title", Input).value, "Keep me")

    async def test_until_an_agent_is_offered_only_when_it_can_wake(self):
        app = tui.Switcher()
        async with app.run_test(size=(110, 30)) as pilot:
            await self.settle(pilot, app)
            await pilot.press("down", "ctrl+z")  # task-b: its session is not running
            await pilot.pause()
            self.assertFalse(any(p[1] == core.UNTIL_AGENT for p in app.screen.presets))
            await pilot.press("escape")
            self.sessions["task-b"] = {"running": True}
            app.refresh_data()
            await self.settle(pilot, app)
            await pilot.press("ctrl+z")
            await pilot.pause()
            self.assertTrue(any(p[1] == core.UNTIL_AGENT for p in app.screen.presets))
            await pilot.press("escape", "ctrl+n")
            await pilot.pause()
            self.assertFalse(any(p[1] == core.UNTIL_AGENT for p in app.screen.presets))  # never for a new task

    # ------------------------------------------------------ project picker
    async def open_form_on_project(self, pilot, app):
        await pilot.press("ctrl+n")
        await pilot.pause()
        field = app.screen.query_one("#project", tui.ProjectField)
        field.focus()
        await pilot.pause()
        return field

    async def test_picker_finds_projects_fuzzily(self):
        core.add_project("dir", str(self.folder("code/api")), "api")
        backend = core.add_project("dir", str(self.folder("code/backend")), "backend")
        core.add_project("dir", str(self.folder("code/notes-app")), "[b]notes")  # markup in a name stays text
        app = tui.Switcher()
        async with app.run_test(size=(110, 34)) as pilot:
            await self.settle(pilot, app)
            field = await self.open_form_on_project(pilot, app)
            for key in "bkd":  # sent before the picker is up: one picker gets them all
                field.post_message(events.Key(key, key))
            await pilot.pause()
            self.assertEqual([type(s).__name__ for s in app.screen_stack][-2:], ["TaskForm", "ProjectPicker"])
            self.assertEqual(app.screen.query_one("#pquery", Input).value, "bkd")
            app.screen.set_focus(None)  # keys that reach the picker before its search box has the focus
            for key in "xy":
                app.screen.post_message(events.Key(key, key))
            await pilot.pause()
            self.assertEqual(app.screen.query_one("#pquery", Input).value, "bkdxy")
            app.screen.query_one("#pquery", Input).focus()
            # Nothing selected and replaced. "bac": a prefix of the name, which outranks any match in the
            # (random) temp folder path the folder projects have.
            await pilot.press("backspace", "backspace", "backspace", "backspace", "a", "c")
            await pilot.pause()
            self.assertEqual(app.screen.query_one("#pquery", Input).value, "bac")
            rows = self.choices(app)
            self.assertIn("backend", rows[0])
            self.assertTrue(rows[-3].startswith("+ Add git") and rows[-2].startswith("+ Add local folder"))
            self.assertTrue(rows[-1].startswith("+ New project “bac”"))  # last: never what Enter picks first
            await pilot.press("enter")
            await pilot.pause()
            self.assertEqual(type(app.screen).__name__, "TaskForm")
            self.assertEqual(field.value, (backend,))
            self.assertIn("code/backend", text_of(app.screen.query_one("#where")))
            await pilot.press("enter")  # Enter on the field opens it empty, on a project not chosen yet
            await pilot.pause()
            self.assertEqual(app.screen.query_one("#pquery", Input).value, "")
            rows = self.choices(app)
            self.assertTrue(rows[0].startswith("  Remove all projects"), rows)
            self.assertTrue(next(r for r in rows if "backend" in r).startswith("✓"), rows)
            self.assertNotIn("backend", rows[app.screen.query_one("#pchoices", OptionList).highlighted])
            self.assertIn("[b]notes", " ".join(rows))
            await self.typed(pilot, "zzz")
            await pilot.pause()
            self.assertIn("No project matches", text_of(app.screen.query_one("#pstatus")))
            await pilot.press("escape")  # back: the projects stay
            await pilot.pause()
            self.assertEqual((type(app.screen).__name__, field.value), ("TaskForm", (backend,)))
            await self.typed(pilot, "api")  # typing on the field adds another
            await pilot.press("enter")
            await pilot.pause()
            api = core.resolve_project(core.Store().load(), "api")
            self.assertEqual(field.value, (backend, api))
            self.assertIn("Workspaces backend · api", text_of(app.screen.query_one("#where")))
            await self.typed(pilot, "backend")  # picking a chosen one removes it
            await pilot.press("enter")
            await pilot.pause()
            self.assertEqual(field.value, (api,))
            await pilot.press("backspace")  # removes the last one
            await pilot.pause()
            self.assertEqual(field.value, ())

    async def test_picker_keys_stay_in_the_picker(self):
        core.add_project("dir", str(self.folder("code/api")), "api")
        app = tui.Switcher()
        async with app.run_test(size=(110, 34)) as pilot:
            await self.settle(pilot, app)
            await self.open_form_on_project(pilot, app)
            await pilot.press("space")
            await pilot.pause()
            self.assertEqual(app.screen.query_one("#pquery", Input).value, "")  # space opens an empty search
            await pilot.press("tab", "ctrl+j")  # tab does not leave the search; ctrl+j moves
            await pilot.pause()
            self.assertEqual(app.screen.focused.id, "pquery")
            self.assertEqual(app.screen.query_one("#pchoices", OptionList).highlighted, 1)
            await pilot.press("ctrl+k", "enter")
            await pilot.pause()
            self.assertEqual(type(app.screen).__name__, "TaskForm")
            self.assertEqual(len(app.screen.query_one("#project", tui.ProjectField).value), 1)

    async def test_picker_completes_folders_and_adds_one(self):
        base = self.folder("work")
        for name in ("alpha", "alpha/inner", "beta", "bravo"):
            self.folder(f"work/{name}")
        saved = core.add_project("dir", str(base / "beta"), "beta")
        app = tui.Switcher()
        async with app.run_test(size=(110, 34)) as pilot:
            await self.settle(pilot, app)
            field = await self.open_form_on_project(pilot, app)
            await pilot.press("enter")
            await pilot.pause()
            box = app.screen.query_one("#pquery", Input)
            box.value = f"{base}/b"
            self.assertTrue(await self.wait_for(pilot, app, lambda: len(self.choices(app)) == 2))
            rows = self.choices(app)
            self.assertTrue(rows[0].startswith(f"  {base}/beta/") and "◆ beta saved" in rows[0], rows)
            self.assertIn(f"{base}/bravo/", rows[1])
            box.value = f"{base}/al"
            self.assertTrue(await self.wait_for(pilot, app, lambda: f"{base}/alpha/" in " ".join(self.choices(app))))
            await pilot.press("tab")  # into alpha: it is offered itself, then its subfolders
            self.assertTrue(await self.wait_for(pilot, app, lambda: len(self.choices(app)) == 2))
            self.assertEqual(box.value, f"{base}/alpha/")
            self.assertTrue(self.choices(app)[0].startswith(f"Use {base}/alpha/"), self.choices(app))
            await pilot.press("enter")  # a new folder: the add dialog, ready on the name
            await pilot.pause(0.4)
            self.assertEqual(type(app.screen).__name__, "AddProjectScreen")
            self.assertEqual(app.screen.focused.id, "pname")
            self.assertIn("Saves the folder", text_of(app.screen.query_one("#status")))
            await pilot.press("enter")
            await self.settle(pilot, app)
            self.assertEqual(type(app.screen).__name__, "TaskForm")
            added = self.store.load()["projects"][field.value[-1]]
            self.assertEqual((added["name"], added["path"]), ("alpha", str(base / "alpha")))
            await pilot.press("enter")  # a saved folder picks its project
            await pilot.pause()
            app.screen.query_one("#pquery", Input).value = f"{base}/beta"
            self.assertTrue(await self.wait_for(pilot, app, lambda: "Use" in " ".join(self.choices(app))))
            await pilot.press("enter")
            await pilot.pause()
            self.assertEqual(field.value[-1], saved)
            self.assertEqual(len(field.value), 2)

    async def test_picker_waits_for_a_slow_folder(self):
        base = self.folder("slow")
        self.folder("slow/alpha")
        gate = threading.Event()
        self.addCleanup(gate.set)
        list_folders = core.list_folders

        def slow(parent, scan_limit=5000):  # an NFS or autofs mount that takes its time
            gate.wait(10)
            return list_folders(parent, scan_limit)

        app = tui.Switcher()
        with mock.patch.object(core, "list_folders", slow):
            async with app.run_test(size=(110, 34)) as pilot:
                await self.settle(pilot, app)
                await self.open_form_on_project(pilot, app)
                await pilot.press("enter")
                await pilot.pause()
                box = app.screen.query_one("#pquery", Input)
                box.value = f"{base}/al"
                await pilot.pause()
                self.assertEqual(self.choices(app), [])  # not the project rows from before
                self.assertIn("Reading the folder", text_of(app.screen.query_one("#pstatus")))
                await pilot.press("enter", "tab")  # nothing to act on yet
                await pilot.pause()
                self.assertEqual((type(app.screen).__name__, box.value), ("ProjectPicker", f"{base}/al"))
                gate.set()
                self.assertTrue(await self.wait_for(pilot, app, lambda: f"{base}/alpha/" in " ".join(self.choices(app))))
                gate.clear()
                box.value = f"{base}/alpha/"  # a folder not read yet
                await pilot.pause()
                await pilot.press("escape")
                await pilot.pause()
                gate.set()  # its listing arrives after the picker closed: ignored, no error
                await pilot.pause(0.3)
                self.assertEqual(type(app.screen).__name__, "TaskForm")

    async def test_picker_knows_a_saved_folder_through_a_symlink(self):
        base = self.folder("work")
        saved = core.add_project("dir", str(self.folder("work/real")), "real")
        (base / "link").symlink_to(base / "real")
        app = tui.Switcher()
        async with app.run_test(size=(110, 34)) as pilot:
            await self.settle(pilot, app)
            field = await self.open_form_on_project(pilot, app)
            await pilot.press("enter")
            await pilot.pause()
            app.screen.query_one("#pquery", Input).value = f"{base}/lin"
            self.assertTrue(await self.wait_for(pilot, app, lambda: bool(self.choices(app))))
            self.assertIn("◆ real saved", self.choices(app)[0])
            await pilot.press("enter")
            await pilot.pause()
            self.assertEqual(field.value, (saved,))

    async def test_picker_finds_a_saved_repository_by_url(self):
        folder = self.folder("clones/api")
        with self.store.edit() as data:
            data["projects"]["p1"] = {"name": "api", "kind": "git", "url": "git@github.com:acme/api.git",
                                      "path": str(folder), "added_at": 1, "last_used_at": 1}
        app = tui.Switcher()
        async with app.run_test(size=(110, 34)) as pilot:
            await self.settle(pilot, app)
            field = await self.open_form_on_project(pilot, app)
            field.post_message(events.Paste("https://github.com/acme/api"))
            await pilot.pause()
            self.assertIn("api", self.choices(app)[0])
            self.assertNotIn("+ Clone", " ".join(self.choices(app)))
            await pilot.press("enter")
            await pilot.pause()
            self.assertEqual(field.value, ("p1",))

    async def test_add_dialog_completes_folders_with_tab(self):
        base = self.folder("work")
        self.folder("work/projects/build")  # a subfolder: Tab must not keep going down into it
        app = tui.Switcher()
        async with app.run_test(size=(110, 34)) as pilot:
            await self.settle(pilot, app)
            await pilot.press("ctrl+p", "ctrl+n", "right")  # a folder
            await pilot.pause()
            source = app.screen.query_one("#source", Input)
            source.value = f"{base}/pro"
            source.cursor_position = len(source.value)
            self.assertTrue(await self.wait_for(pilot, app, lambda: app.screen.suggester.last is not None))
            await pilot.press("tab")
            self.assertEqual(source.value, f"{base}/projects/")
            await pilot.press("tab")  # nothing left to complete: the next field
            self.assertEqual(app.screen.focused.id, "pname")
            app.screen.action_kind("git")  # a repository: no folder completion
            await pilot.pause()
            source.value = f"{base}/pro"
            await pilot.pause(0.3)
            self.assertIsNone(app.screen.suggester.last)

    # -------------------------------------------------------------- notes
    async def test_ctrl_o_edits_the_highlighted_tasks_notes(self):
        ran = []

        def editor(command, check, env):
            ran.append(command)
            Path(command[-1]).write_text(Path(command[-1]).read_text() + "\nasked infra about the cookie\n")

        app = tui.Switcher()
        with mock.patch.dict(os.environ, {"VISUAL": "nvim"}), mock.patch.object(tui.subprocess, "run", editor), \
                mock.patch.object(tui.Switcher, "suspend", lambda self: contextlib.nullcontext()):
            async with app.run_test(size=(110, 34)) as pilot:
                await self.settle(pilot, app)
                self.assertIn("Notes · none yet", text_of(app.query_one("#preview")))  # task-a predates notes
                await pilot.press("ctrl+o")
                await pilot.pause()
                self.assertEqual(ran, [["nvim", str(core.notes_path("task-a"))]])
                preview = text_of(app.query_one("#preview"))
                self.assertIn("Notes · edited now", preview)
                self.assertIn("asked infra about the cookie", preview)
                self.assertNotIn("Created", preview)  # itm's own heading lines stay out of the preview
                await pilot.press("ctrl+e")  # not behind a dialog
                await pilot.pause()
                await pilot.press("ctrl+o")
                await pilot.pause()
                self.assertEqual(len(ran), 1)
        self.assertTrue(core.notes_path("task-a").read_text().startswith("# Fix login\n"))

    async def test_notes_errors_stay_in_the_switcher(self):
        app = tui.Switcher()
        with mock.patch.object(core, "editor_command", side_effect=core.ItmError("no editor found; set $EDITOR")):
            async with app.run_test(size=(110, 30)) as pilot:
                await self.settle(pilot, app)
                await pilot.press("ctrl+o")
                await pilot.pause()
                self.assertIn("no editor found", text_of(app.query_one("#msg")))
        app = tui.Switcher()
        with mock.patch.dict(os.environ, {"VISUAL": "nvim"}):
            async with app.run_test(size=(110, 30)) as pilot:  # the headless driver cannot suspend
                await self.settle(pilot, app)
                await pilot.press("ctrl+o")
                await pilot.pause()
                self.assertIn("notes are in", text_of(app.query_one("#msg")))

    async def test_delete_says_the_notes_go_too(self):
        app = tui.Switcher()
        async with app.run_test(size=(110, 30)) as pilot:
            await self.settle(pilot, app)
            await pilot.press("ctrl+d")
            await pilot.pause()
            self.assertIn("agents and notes", " ".join(text_of(s) for s in app.screen.query(Static)))

    # ------------------------------------------------- projects and workspaces
    async def test_preview_lists_every_project_and_its_open_workspace(self):
        api = core.add_project("dir", str(self.folder("code/api")), "api")
        web = core.add_project("dir", str(self.folder("code/web")), "web")
        with self.store.edit() as data:
            core.set_task_projects(data["tasks"]["task-a"], [api, web])
            data["tasks"]["task-a"]["workspace_projects"] = [web]
        self.sessions["task-a"] = {"running": True, "socket_path": "/sock/a"}
        app = tui.Switcher()
        async with app.run_test(size=(120, 34)) as pilot:
            await self.settle(pilot, app)
            self.assertIn("◆ api +1", self.labels(app)[1][1])
            preview = text_of(app.query_one("#preview")).splitlines()
            self.assertTrue(any("◆ api" in l and "workspace open" not in l for l in preview), preview)
            self.assertTrue(any("◆ web" in l and "workspace open" in l for l in preview), preview)
            app.query_one("#search", Input).value = "@web"
            await pilot.pause()
            self.assertEqual([i for i, _ in self.labels(app) if i], ["task-a"])  # any of its projects

    async def test_a_project_with_an_open_workspace_is_not_removed_in_the_form(self):
        api = core.add_project("dir", str(self.folder("code/api")), "api")
        with self.store.edit() as data:
            core.set_task_projects(data["tasks"]["task-a"], [api])
            data["tasks"]["task-a"]["workspace_projects"] = [api]
        app = tui.DialogApp(tui.TaskForm(task=self.task("task-a"), task_name="task-a", session_effect="running"))
        async with app.run_test(size=(110, 34)) as pilot:
            await pilot.pause()
            app.screen.query_one("#project", tui.ProjectField).value = ()
            await pilot.pause()
            self.assertIn("api still has a workspace", text_of(app.screen.query_one("#where")))
            await pilot.press("ctrl+s")
            await pilot.pause()
            self.assertEqual(type(app.screen).__name__, "TaskForm")
            self.assertIn("close it in Herdr first", text_of(app.screen.query_one("#error")))
        core.set_config("herdr", "projects_from_workspaces", False)  # then only edits change projects
        app = tui.DialogApp(tui.TaskForm(task=self.task("task-a"), task_name="task-a", session_effect="running"))
        async with app.run_test(size=(110, 34)) as pilot:
            await pilot.pause()
            app.screen.query_one("#project", tui.ProjectField).value = ()
            await pilot.pause()
            await pilot.press("ctrl+s")
            await pilot.pause()
        self.assertEqual(app.return_value["remove"], [api])

    async def test_settings_toggle_and_save(self):
        app = tui.Switcher()
        async with app.run_test(size=(110, 34)) as pilot:
            await self.settle(pilot, app)
            await pilot.press("ctrl+s")
            await pilot.pause()
            self.assertEqual(type(app.screen).__name__, "SettingsScreen")
            screen_text = " ".join(text_of(s) for s in app.screen.query(Static))
            self.assertIn("config.toml", screen_text)
            await pilot.press("enter")  # the first setting: projects follow workspaces
            await self.settle(pilot, app)
            self.assertFalse(core.load_config()["herdr"]["projects_from_workspaces"])
            await pilot.press("down", "space")
            await self.settle(pilot, app)
            self.assertFalse(core.load_config()["herdr"]["remove_projects_without_workspace"])
            await pilot.press("ctrl+n")  # list keys stay off behind the screen
            await pilot.pause()
            self.assertEqual(type(app.screen).__name__, "SettingsScreen")
            await pilot.press("escape")
            await pilot.pause()
            self.assertEqual(len(app.screen_stack), 1)

    async def test_settings_show_a_broken_file(self):
        core.config_path().parent.mkdir(parents=True)
        core.config_path().write_text("[herdr\n")
        app = tui.DialogApp(tui.SettingsScreen())
        async with app.run_test(size=(110, 34)) as pilot:
            await pilot.pause()
            self.assertIn("not valid TOML", text_of(app.screen.query_one("#status")))
            await pilot.press("enter", "space")
            await pilot.pause()
        self.assertEqual(core.config_path().read_text(), "[herdr\n")  # nothing written

    async def test_remove_all_is_never_the_default(self):
        api = core.add_project("dir", str(self.folder("code/api")), "api")
        app = tui.DialogApp(tui.TaskForm(projects=[api]))
        async with app.run_test(size=(110, 34)) as pilot:
            await pilot.pause()
            field = app.screen.query_one("#project", tui.ProjectField)
            field.focus()
            await pilot.press("enter")  # every project is chosen: the default is adding a new one
            await pilot.pause()
            self.assertIn("Add git", self.choices(app)[app.screen.query_one("#pchoices", OptionList).highlighted])
            await self.typed(pilot, "res")  # matches no project, and "remove all" is not offered for a search
            await pilot.pause()
            self.assertFalse(any("Remove all" in row for row in self.choices(app)))
            await pilot.press("enter")
            await pilot.pause()
            self.assertEqual(field.value, (api,))

    async def test_the_field_fits_many_projects_and_its_hint(self):
        ids = [core.add_project("dir", str(self.folder(f"code/{n}")), f"project-with-a-long-name-{n}")
               for n in range(5)]
        app = tui.DialogApp(tui.TaskForm(projects=ids))
        async with app.run_test(size=(90, 34)) as pilot:
            await pilot.pause()
            field = app.screen.query_one("#project", tui.ProjectField)
            field.focus()
            await pilot.pause()
            shown = text_of(field)
            self.assertIn("⌫ removes the last", shown)  # the hint is never cut off
            self.assertRegex(shown, r"\+\d")  # the others are counted
            self.assertLessEqual(len(shown), field.content_size.width)

    async def test_backspace_before_the_picker_is_up_reaches_it(self):
        core.add_project("dir", str(self.folder("code/api")), "api")
        app = tui.DialogApp(tui.TaskForm())
        async with app.run_test(size=(110, 34)) as pilot:
            await pilot.pause()
            field = app.screen.query_one("#project", tui.ProjectField)
            field.focus()
            for key in ("a", "b", "backspace"):
                field.post_message(events.Key(key, key if key != "backspace" else None))
            await pilot.pause()
            self.assertEqual(app.screen.query_one("#pquery", Input).value, "a")

    async def test_the_new_task_form_says_where_it_really_starts(self):
        gone = core.add_project("dir", str(self.folder("code/gone")), "gone")
        api = core.add_project("dir", str(self.folder("code/api")), "api")
        (self.root / "code/gone").rmdir()
        app = tui.DialogApp(tui.TaskForm(projects=[gone, api]))
        async with app.run_test(size=(110, 34)) as pilot:
            await pilot.pause()
            where = text_of(app.screen.query_one("#where"))
        self.assertIn(f"Starts in {self.root / 'code/api'}", where)
        self.assertIn("Missing, so no workspace: gone", where)

    async def test_reordering_in_the_edit_form_is_kept(self):
        api = core.add_project("dir", str(self.folder("code/api")), "api")
        web = core.add_project("dir", str(self.folder("code/web")), "web")
        with self.store.edit() as data:
            core.set_task_projects(data["tasks"]["task-a"], [api, web])
        app = tui.Switcher()
        async with app.run_test(size=(110, 34)) as pilot:
            await self.settle(pilot, app)
            await pilot.press("ctrl+e")
            await pilot.pause()
            app.screen.query_one("#project", tui.ProjectField).value = (web, api)
            await pilot.pause()
            await pilot.press("ctrl+s")
            await self.settle(pilot, app)
        self.assertEqual(self.task("task-a")["projects"], [web, api])

    async def test_a_broken_settings_file_does_not_break_the_forms(self):
        core.config_path().parent.mkdir(parents=True)
        core.config_path().write_bytes(b"# n\xe3o\n")  # Latin-1
        api = core.add_project("dir", str(self.folder("code/api")), "api")
        with self.store.edit() as data:
            core.set_task_projects(data["tasks"]["task-a"], [api])
            data["tasks"]["task-a"]["workspace_projects"] = [api]
        self.sessions["task-a"] = {"running": True, "socket_path": "/sock/a"}
        app = tui.Switcher()
        async with app.run_test(size=(110, 34)) as pilot:
            await self.settle(pilot, app)
            await pilot.press("ctrl+n")
            await pilot.pause()
            self.assertEqual(type(app.screen).__name__, "TaskForm")
            await pilot.press("escape")
            await pilot.pause()
            await pilot.press("ctrl+e")
            await pilot.pause()
            app.screen.query_one("#project", tui.ProjectField).value = ()
            await pilot.pause()
            where = text_of(app.screen.query_one("#where"))
            self.assertIn("Settings ignored", where)
            self.assertNotIn("still has a workspace", where)  # off, as for the daemon
            await pilot.press("escape", "y")
            await pilot.pause()
            await pilot.press("ctrl+s")
            await pilot.pause()
            self.assertEqual(type(app.screen).__name__, "SettingsScreen")
            self.assertIn("cannot read", text_of(app.screen.query_one("#status")))

    async def test_a_failed_save_is_shown_and_saves_do_not_overlap(self):
        calls = []
        gate = threading.Event()
        self.addCleanup(gate.set)

        def slow_failing(group, key, value):
            calls.append((key, value))
            gate.wait(5)
            raise OSError("read-only file system")

        app = tui.DialogApp(tui.SettingsScreen())
        with mock.patch.object(core, "set_config", slow_failing):
            async with app.run_test(size=(110, 34)) as pilot:
                await pilot.pause()
                await pilot.press("space", "space", "down", "space")  # one save at a time
                await pilot.pause()
                gate.set()
                await app.workers.wait_for_complete()
                await pilot.pause()
                self.assertEqual(calls, [("projects_from_workspaces", False)])
                self.assertIn("Not saved: read-only file system", text_of(app.screen.query_one("#status")))
                await pilot.press("space")  # usable again
                await app.workers.wait_for_complete()
        self.assertEqual(len(calls), 2)

    # ---------------------------------------------------- name-only projects
    async def test_the_name_only_tab(self):
        app = tui.DialogApp(tui.AddProjectScreen("name", focus="name", name="Client X"))
        async with app.run_test(size=(110, 34)) as pilot:
            await pilot.pause(0.4)
            self.assertFalse(app.screen.query_one("#source-row").display)
            self.assertEqual(app.screen.focused.id, "pname")
            self.assertIn("no folder", text_of(app.screen.query_one("#status")))
            await pilot.press("left", "left")  # the cursor moves in the name; the kind stays
            self.assertEqual(app.screen.kind, "name")
            await pilot.press("ctrl+right")  # the tabs cycle
            await pilot.pause()
            self.assertEqual(app.screen.kind, "git")
            await pilot.press("ctrl+left")
            await pilot.pause()
            self.assertEqual(app.screen.kind, "name")
            await pilot.press("enter")
            await self.settle(pilot, app)
        project = self.store.load()["projects"][app.return_value]
        self.assertEqual((project["name"], core.project_kind(project)), ("Client X", "name"))

    async def test_new_project_row_goes_through_the_dialog(self):
        core.add_project("dir", str(self.folder("code/api")), "api")
        app = tui.DialogApp(tui.TaskForm())
        async with app.run_test(size=(110, 34)) as pilot:
            await pilot.pause()
            field = app.screen.query_one("#project", tui.ProjectField)
            field.focus()
            await self.typed(pilot, "zz top")  # matches nothing
            await pilot.pause()
            rows = self.choices(app)
            self.assertTrue(rows[-1].startswith("+ New project “zz top”"), rows)
            highlighted = app.screen.query_one("#pchoices", OptionList).highlighted
            self.assertNotIn("New project", rows[highlighted])  # Enter on a typo saves nothing
            app.screen.query_one("#pchoices", OptionList).highlighted = len(rows) - 1
            await pilot.press("enter")
            await pilot.pause(0.4)
            self.assertEqual((type(app.screen).__name__, app.screen.kind), ("AddProjectScreen", "name"))
            self.assertEqual(app.screen.query_one("#pname", Input).value, "zz top")
            self.assertEqual(len(self.store.load()["projects"]), 1)  # nothing saved before the dialog's Enter
            await pilot.press("enter")
            await self.settle(pilot, app)
            self.assertEqual(type(app.screen).__name__, "TaskForm")
            pid, = field.value
            self.assertEqual(core.project_kind(self.store.load()["projects"][pid]), "name")
            self.assertIn("No folder, so no workspace: zz top", text_of(app.screen.query_one("#where")))
            self.assertIn(f"Starts in {self.root / 'home'}", text_of(app.screen.query_one("#where")))

    async def test_name_only_projects_in_the_list_preview_and_projects_screen(self):
        pid = core.add_project("name", "Client X")
        with self.store.edit() as data:  # never started: the preview says where it starts
            core.set_task_projects(data["tasks"]["task-b"], [pid])
        app = tui.Switcher()
        async with app.run_test(size=(120, 34)) as pilot:
            await self.settle(pilot, app)
            olist = app.query_one("#list", OptionList)
            olist.highlighted = [i for i, _ in self.labels(app)].index("task-b")
            await pilot.pause()
            preview = text_of(app.query_one("#preview"))
            self.assertIn("◆ Client X  name", preview)
            self.assertIn("no folder", preview)
            self.assertNotIn("missing", preview)
            self.assertIn(f"starts in {self.root / 'home'}", preview)
            await pilot.press("ctrl+p")
            await self.settle(pilot, app)
            self.assertIn("press prefix+shift+a in its workspace", text_of(app.screen.query_one("#phint")))
            await pilot.press("ctrl+d")  # used by a task: refused, nothing to choose
            await self.settle(pilot, app)
            self.assertIn("Used by 1 task", " ".join(text_of(s) for s in app.screen.query(Static)))

    async def test_kind_keys(self):
        app = tui.DialogApp(tui.AddProjectScreen("git"))
        async with app.run_test(size=(110, 34)) as pilot:
            await pilot.pause()
            await pilot.press("left")  # plain arrows stop at the ends: no jump from git to "Name only"
            await pilot.pause()
            self.assertEqual(app.screen.kind, "git")
            await pilot.press("right", "right")
            await pilot.pause()
            self.assertEqual(app.screen.kind, "name")
            await pilot.press("ctrl+right")  # Ctrl cycles
            await pilot.pause()
            self.assertEqual(app.screen.kind, "git")
            app.screen.busy = True  # a clone being started: the kind stays
            await pilot.press("ctrl+left")
            await pilot.pause()
            self.assertEqual(app.screen.kind, "git")

    async def test_the_projects_hint_goes_with_the_last_project(self):
        core.add_project("name", "Client X")
        app = tui.Switcher()
        async with app.run_test(size=(120, 34)) as pilot:
            await self.settle(pilot, app)
            await pilot.press("ctrl+p")
            await self.settle(pilot, app)
            self.assertIn("has no folder", text_of(app.screen.query_one("#phint")))
            await pilot.press("ctrl+d")
            await self.settle(pilot, app)
            await pilot.press("enter")  # forget it
            await self.settle(pilot, app)
            self.assertEqual(text_of(app.screen.query_one("#phint")), "")

    async def test_removing_a_name_only_project(self):
        core.add_project("name", "Client X")
        info = core.removal_info("Client X")
        app = tui.DialogApp(tui.RemoveProjectScreen(info))
        async with app.run_test(size=(110, 34)) as pilot:
            await pilot.pause()
            self.assertIn("It has no folder; nothing is deleted.", " ".join(text_of(s) for s in app.screen.query(Static)))
            await pilot.press("enter")
            await pilot.pause()
        self.assertEqual(app.return_value, "keep")

    async def test_the_give_folder_question(self):
        for key, answer in (("y", True), ("n", False), ("escape", None)):
            with self.subTest(key=key):
                app = tui.DialogApp(tui.ConfirmFolderScreen("Client X", "~/code/client-x"))
                async with app.run_test(size=(100, 20)) as pilot:
                    await pilot.pause()
                    self.assertIn("Give ~/code/client-x to Client X?", text_of(app.screen.query_one(".dtitle")))
                    await pilot.press(key)
                    await pilot.pause()
                self.assertEqual(app.return_value, answer)

    async def test_add_dialog_can_start_with_a_name(self):
        folder = self.folder("code/new-thing")
        app = tui.DialogApp(tui.AddProjectScreen("dir", source=str(folder), focus="name", name="My Thing"))
        async with app.run_test(size=(110, 34)) as pilot:
            await pilot.pause(0.4)
            self.assertEqual(app.screen.query_one("#pname", Input).value, "My Thing")
            await pilot.press("enter")
            await self.settle(pilot, app)
        self.assertEqual(self.store.load()["projects"][app.return_value]["name"], "My Thing")


if __name__ == "__main__":
    unittest.main()
