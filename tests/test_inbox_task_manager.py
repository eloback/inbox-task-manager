import contextlib
import json
import os
import shutil
import signal
import stat
import subprocess
import sys
import tempfile
import unittest
from datetime import datetime
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

import inbox_task_manager as itm  # noqa: E402


class NamesTest(unittest.TestCase):
    def test_slugify_strips_accents_and_symbols(self):
        self.assertEqual(itm.slugify("Café crème: résumé #42!"), "cafe-creme-resume-42")

    def test_slugify_empty_falls_back(self):
        self.assertEqual(itm.slugify("!!!"), "task")

    def test_slugify_truncates_without_trailing_dash(self):
        slug = itm.slugify("a" * 39 + " b")
        self.assertLessEqual(len(slug), 40)
        self.assertFalse(slug.endswith("-"))

    def test_unique_session_name_adds_suffix(self):
        taken = {"task-fix-bug", "Task-Fix-Bug-2"}
        self.assertEqual(itm.unique_session_name("Fix bug", taken), "task-fix-bug-3")


class ParseWhenTest(unittest.TestCase):
    base = int(datetime(2026, 11, 4, 10, 0).timestamp())

    def test_durations(self):
        self.assertEqual(itm.parse_when("30m", self.base), self.base + 1800)
        self.assertEqual(itm.parse_when("1h30m", self.base), self.base + 5400)
        self.assertEqual(itm.parse_when("2d", self.base), self.base + 2 * 86400)
        self.assertEqual(itm.parse_when("1h 30m", self.base), self.base + 5400)
        self.assertEqual(itm.parse_when("45", self.base), self.base + 45 * 60)

    def test_clock_later_today(self):
        self.assertEqual(itm.parse_when("14:30", self.base), int(datetime(2026, 11, 4, 14, 30).timestamp()))

    def test_clock_already_passed_is_tomorrow(self):
        self.assertEqual(itm.parse_when("9:00", self.base), int(datetime(2026, 11, 5, 9, 0).timestamp()))

    def test_invalid(self):
        for text in ("", "25:00", "soonish please", "0m", "0", "²", "99999999d"):
            with self.subTest(text=text), self.assertRaises(itm.ItmError):
                itm.parse_when(text, self.base)

    def test_day_and_clock(self):
        when = itm.parse_when("tomorrow 9:00")
        self.assertGreater(when, itm.now())


class StoreTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.store = itm.Store(Path(self.tmp.name) / "tasks.json")

    def tearDown(self):
        self.tmp.cleanup()

    def test_roundtrip(self):
        self.assertEqual(self.store.load()["tasks"], {})
        with self.store.edit() as data:
            data["tasks"]["task-a"] = itm.new_task("A", created=1)
        self.assertEqual(self.store.load()["tasks"]["task-a"]["title"], "A")

    def test_corrupt_file_is_an_error(self):
        self.store.path.write_text("{nope")
        with self.assertRaises(itm.ItmError):
            self.store.load()


class TaskLogicTest(unittest.TestCase):
    def data(self):
        return {
            "tasks": {
                "task-old": {**itm.new_task("Old", created=1), "last_used_at": 10},
                "task-new": {**itm.new_task("New", created=1), "last_used_at": 20},
                "task-zz": {**itm.new_task("Zz", created=1), "state": "snoozed", "snoozed_until": 99},
                "task-done": {**itm.new_task("Done", created=1), "state": "archived", "archived_at": 5},
            }
        }

    def test_next_active_prefers_waiting_then_recent(self):
        data = self.data()
        self.assertEqual(itm.next_active(data, {}, exclude=None), "task-new")
        self.assertEqual(itm.next_active(data, {"task-old": {"blocked": 1}}, exclude=None), "task-old")
        self.assertEqual(itm.next_active(data, {}, exclude="task-new"), "task-old")

    def test_next_active_falls_back_to_default(self):
        self.assertEqual(itm.next_active({"tasks": {}}, {}, exclude=None), itm.DEFAULT_SESSION)

    def test_task_rows_views_and_order(self):
        data = self.data()
        rows = itm.task_rows(data, "active", {"task-old": {"blocked": 2}}, {})
        self.assertEqual([r["session"] for r in rows], ["task-old", "task-new"])
        self.assertEqual(len(itm.task_rows(data, "all", {}, {})), 4)
        self.assertEqual([r["session"] for r in itm.task_rows(data, "archived", {}, {})], ["task-done"])

    def test_resolve_task(self):
        data = self.data()
        self.assertEqual(itm.resolve_task(data, "task-old"), "task-old")
        self.assertEqual(itm.resolve_task(data, "old"), "task-old")
        self.assertEqual(itm.resolve_task(data, "NEW"), "task-new")
        with self.assertRaises(itm.ItmError):
            itm.resolve_task(data, "missing")

    def test_split_snooze_args(self):
        data = self.data()
        self.assertEqual(itm.split_snooze_args(data, None, ["old", "30m"]), ("task-old", "30m"))
        self.assertEqual(itm.split_snooze_args(data, None, ["old"]), ("task-old", None))
        self.assertEqual(itm.split_snooze_args(data, "new", ["2h"]), ("task-new", "2h"))
        with mock.patch.object(itm, "target_or_current", lambda d, q: "task-cur"):
            self.assertEqual(itm.split_snooze_args(data, None, ["1h", "30m"]), ("task-cur", "1h 30m"))

    def status(self, **sinces):
        return {name: {"blocked": 1, "agents": [{"status": "blocked", "since": since}]} for name, since in sinces.items()}

    def test_longest_waiting_skips_archived(self):
        data = self.data()
        self.assertIsNone(itm.longest_waiting(data, {}))
        status = self.status(**{"task-new": 50, "task-old": 40, "task-done": 1})
        self.assertEqual(itm.longest_waiting(data, status), "task-old")

    def test_previous_task_is_the_most_recent_other_active_one(self):
        data = self.data()
        self.assertEqual(itm.previous_task(data, "task-new"), "task-old")
        self.assertEqual(itm.previous_task(data, None), "task-new")
        self.assertIsNone(itm.previous_task({"tasks": {}}, None))

    def test_counts(self):
        counts = itm.counts(self.data(), self.status(**{"task-zz": 1, "task-done": 1}))
        self.assertEqual(counts, {"active": 2, "snoozed": 1, "archived": 1, "waiting": 1})

    def test_snooze_until_an_agent_needs_you(self):
        self.assertEqual(itm.snooze_until("agent"), itm.UNTIL_AGENT)
        task = {**itm.new_task("A"), "snoozed_until": 5}
        itm.apply_snooze(task, itm.UNTIL_AGENT)
        self.assertEqual(task["state"], "snoozed")
        self.assertNotIn("snoozed_until", task)
        self.assertEqual(itm.describe_snooze(None), "until an agent needs you")

    def test_tombstone_blocks_adoption_until_session_is_gone(self):
        data = {"tasks": {}, "deleted": {"task-gone": 1}}
        itm.adopt_sessions(data, {"task-gone": {}})
        self.assertEqual(data["tasks"], {})
        itm.adopt_sessions(data, {})
        self.assertEqual(data["deleted"], {})

    def test_adopt_only_prefixed_sessions(self):
        data = {"tasks": {}}
        itm.adopt_sessions(data, {"default": {}, "task-from-cli": {}})
        self.assertEqual(list(data["tasks"]), ["task-from-cli"])
        self.assertEqual(data["tasks"]["task-from-cli"]["title"], "from cli")


class PollTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        env = {"XDG_STATE_HOME": self.tmp.name, "XDG_RUNTIME_DIR": self.tmp.name, "XDG_CONFIG_HOME": self.tmp.name}
        self.env = mock.patch.dict(os.environ, env)
        self.env.start()
        self.store = itm.Store()
        with self.store.edit() as data:
            data["tasks"]["task-a"] = itm.new_task("A")
            data["tasks"]["task-b"] = {**itm.new_task("B"), "state": "snoozed", "snoozed_until": itm.now() + 3600}
            data["tasks"]["task-c"] = {**itm.new_task("C"), "state": "snoozed", "snoozed_until": itm.now() - 1}
        self.sessions = {
            name: {"name": name, "running": True, "socket_path": f"/sock/{name}"}
            for name in ("default", "task-a", "task-b", "task-c")
        }
        self.agents = {
            "/sock/default": [{"agent": "claude", "agent_status": "blocked", "pane_id": "p0"}],
            "/sock/task-a": [
                {"agent": "claude", "agent_status": "blocked", "pane_id": "p1", "state_change_seq": 7},
                {"agent": "codex", "agent_status": "working", "pane_id": "p2"},
            ],
            "/sock/task-b": [{"agent": "claude", "agent_status": "blocked", "pane_id": "p3", "state_change_seq": 1}],
            "/sock/task-c": [],
        }
        self.patches = [
            mock.patch.object(itm, "list_sessions", lambda: self.sessions),
            mock.patch.object(itm, "list_agents", lambda socket: self.agents[socket]),
            mock.patch.object(itm, "session_workspaces", lambda socket: None),
            mock.patch.object(itm, "live_clients", lambda: {}),
            mock.patch.object(itm, "deliver"),
        ]
        for patch in self.patches:
            patch.start()

    def tearDown(self):
        for patch in self.patches:
            patch.stop()
        self.env.stop()
        self.tmp.cleanup()

    def test_notifies_wakes_and_dedupes(self):
        notified = {}
        messages = itm.poll_once(self.store, notified)
        titles = sorted(m[0] for m in messages)
        self.assertEqual(titles, ["Needs input · A", "Task back · C", "Woke · Needs input · B"])
        tasks = self.store.load()["tasks"]
        self.assertEqual({n: t["state"] for n, t in tasks.items()}, {"task-a": "active", "task-b": "active", "task-c": "active"})
        self.assertEqual(itm.load_status()["task-a"]["blocked"], 1)
        self.assertNotIn("default", itm.load_status())

        self.assertEqual(itm.poll_once(self.store, notified), [])

    def test_wait_is_reported_again_after_unblocking(self):
        notified = {}
        itm.poll_once(self.store, notified)
        self.agents["/sock/task-a"] = []
        itm.poll_once(self.store, notified)
        self.assertFalse(any(k.startswith("task-a|") for k in notified))
        self.agents["/sock/task-a"] = [{"agent": "claude", "agent_status": "blocked", "pane_id": "p1", "state_change_seq": 7}]
        self.assertEqual([m[0] for m in itm.poll_once(self.store, notified)], ["Needs input · A"])

    def test_unpolled_session_keeps_its_waits(self):
        notified = {}
        itm.poll_once(self.store, notified)
        with mock.patch.object(itm, "list_agents", lambda socket: None):
            itm.poll_once(self.store, notified)
        self.assertTrue(any(k.startswith("task-a|") for k in notified))

    def test_failed_store_update_does_not_swallow_waits(self):
        notified = {}
        self.store.path.write_text("{broken")
        with self.assertRaises(itm.ItmError):
            itm.poll_once(self.store, notified)
        self.assertEqual(notified, {})

    def test_attached_session_left_to_herdr(self):
        with mock.patch.object(itm, "live_clients", lambda: {123: "task-a"}):
            messages = itm.poll_once(self.store, {})
        self.assertNotIn("Needs input · A", [m[0] for m in messages])

    def test_status_records_when_a_wait_started(self):
        notified = {}
        with mock.patch.object(itm, "now", return_value=1000):
            itm.poll_once(self.store, notified)
        with mock.patch.object(itm, "now", return_value=1300):
            itm.poll_once(self.store, notified)
            status, fresh = itm.read_status()
        self.assertTrue(fresh)
        blocked = [a for a in status["task-a"]["agents"] if a["status"] == "blocked"]
        self.assertEqual([a["since"] for a in blocked], [1000])
        self.assertEqual(itm.waiting_since(status, "task-a"), 1000)

    def test_stale_status_is_reported_as_stale(self):
        itm.poll_once(self.store, {})
        with mock.patch.object(itm, "now", return_value=itm.now() + itm.STATUS_MAX_AGE + 1):
            self.assertEqual(itm.read_status(), ({}, False))

    def test_snoozed_until_an_agent_wakes_only_for_an_agent(self):
        with self.store.edit() as data:
            for name in ("task-b", "task-c"):
                itm.apply_snooze(data["tasks"][name], itm.UNTIL_AGENT)
        itm.poll_once(self.store, {})
        tasks = self.store.load()["tasks"]
        self.assertEqual(tasks["task-b"]["state"], "active")  # its agent is blocked
        self.assertEqual(tasks["task-c"]["state"], "snoozed")  # no timer, no agent

    def test_until_an_agent_wakes_when_its_session_stopped(self):
        with self.store.edit() as data:
            itm.apply_snooze(data["tasks"]["task-c"], itm.UNTIL_AGENT)
        self.sessions["task-c"]["running"] = False
        messages = itm.poll_once(self.store, {})
        self.assertEqual(self.store.load()["tasks"]["task-c"]["state"], "active")
        self.assertIn("Task back · C", [m[0] for m in messages])

    def test_until_an_agent_wakes_on_a_wait_already_reported(self):
        notified = {}
        itm.poll_once(self.store, notified)  # task-a's wait is now reported
        with self.store.edit() as data:
            itm.apply_snooze(data["tasks"]["task-a"], itm.UNTIL_AGENT)
        messages = itm.poll_once(self.store, notified)
        self.assertEqual(self.store.load()["tasks"]["task-a"]["state"], "active")
        self.assertIn("Woke · Needs input · A", [m[0] for m in messages])


class AttachLoopTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        env = {"XDG_STATE_HOME": self.tmp.name, "XDG_RUNTIME_DIR": self.tmp.name}
        self.env = mock.patch.dict(os.environ, env)
        self.env.start()

    def tearDown(self):
        self.env.stop()
        self.tmp.cleanup()

    def test_hand_off_written_during_an_attach_is_followed(self):
        attached = []
        next_file = itm.clients_dir() / f"{os.getpid()}.next"

        def attach(argv, **kwargs):
            attached.append(argv[-1])
            if len(attached) == 1:
                itm.write_atomic(next_file, "task-b\n")  # a switch while task-a is attached
            if len(attached) == 2:
                itm.write_atomic(next_file, "task-c\n")  # one more, right as task-b starts
            return subprocess.CompletedProcess(argv, 0)

        next_file.parent.mkdir(parents=True)
        next_file.write_text("task-stale\n")  # left by a crashed loop with this pid
        with mock.patch.object(itm.subprocess, "run", side_effect=attach), mock.patch.object(itm.shutil, "which", return_value="/bin/herdr"):
            itm.attach_loop("task-a")
        self.assertEqual(attached, ["task-a", "task-b", "task-c"])
        self.assertEqual(list(itm.clients_dir().iterdir()), [])


class GitUrlTest(unittest.TestCase):
    def test_forms_of_one_repository_are_equal(self):
        for url in (
            "git@github.com:acme/api.git",
            "github.com:acme/api",
            "https://github.com/acme/api",
            "https://github.com/acme/api.git/",
            "ssh://git@GitHub.com:22/acme/api.git",
            "  https://user@github.com/acme/api  ",
        ):
            with self.subTest(url=url):
                self.assertEqual(itm.normalize_git_url(url), "github.com/acme/api")

    def test_gitlab_subgroups_and_self_hosted(self):
        for url in ("git@gitlab.com:group/sub/app.git", "https://gitlab.com/group/sub/app", "ssh://git@gitlab.com:2222/group/sub/app"):
            with self.subTest(url=url):
                self.assertEqual(itm.normalize_git_url(url), "gitlab.com/group/sub/app")

    def test_azure_devops_forms_are_one_repository(self):
        for url in (
            "https://dev.azure.com/acme/Platform/_git/api",
            "https://acme@dev.azure.com/acme/Platform/_git/api",
            "git@ssh.dev.azure.com:v3/acme/Platform/api",
            "ssh://git@ssh.dev.azure.com/v3/acme/Platform/api",
            "https://acme.visualstudio.com/Platform/_git/api",
            "https://acme.visualstudio.com/DefaultCollection/Platform/_git/api",
            "acme@vs-ssh.visualstudio.com:v3/acme/Platform/api",
        ):
            with self.subTest(url=url):
                self.assertEqual(itm.normalize_git_url(url), "dev.azure.com/acme/Platform/api")

    def test_azure_devops_organization_ignores_case(self):
        self.assertEqual(
            itm.normalize_git_url("https://Contoso.visualstudio.com/Platform/_git/api"),
            itm.normalize_git_url("https://Contoso@dev.azure.com/Contoso/Platform/_git/api"),
        )

    def test_azure_devops_short_form_and_escapes(self):
        self.assertEqual(
            itm.normalize_git_url("https://dev.azure.com/acme/_git/api"),
            itm.normalize_git_url("git@ssh.dev.azure.com:v3/acme/api/api"),
        )
        self.assertEqual(
            itm.normalize_git_url("https://dev.azure.com/acme/My%20Project/_git/api"),
            "dev.azure.com/acme/My Project/api",
        )
        self.assertEqual(
            itm.normalize_git_url("git@ssh.dev.azure.com:v3/acme/My%20Project/api"),
            "dev.azure.com/acme/My Project/api",
        )

    def test_invalid(self):
        for url in (
            "", "api", "/home/me/repo", "./repo", "https://github.com/", "git@github.com:../x",
            "file:///tmp/repo", "x:.git", "..:foo/bar", "https://../foo", "-c:foo/bar",
            "https://host/a/%2e%2e/b", "https://host/a%2Fb/c", "https://host/a/b%00", "git@ssh.dev.azure.com:v3",
            "git@host:a/.git/hooks", "ext::sh -c touch% /tmp/x", "fd::3",
        ):
            with self.subTest(url=url), self.assertRaises(itm.ItmError):
                itm.normalize_git_url(url)

    @unittest.skipUnless(shutil.which("git"), "needs git")
    def test_repo_key_follows_insteadof(self):
        env = {
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_CONFIG_GLOBAL": os.devnull,
            "GIT_CONFIG_COUNT": "1",
            "GIT_CONFIG_KEY_0": "url.git@github.com:.insteadOf",
            "GIT_CONFIG_VALUE_0": "gh:",
        }
        with mock.patch.dict(os.environ, env):
            self.assertEqual(itm.repo_key("gh:acme/api"), "github.com/acme/api")


class ProjectEnvTest(unittest.TestCase):
    """Runs with state, data and the task folder in a temporary directory."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        # Resolved: macOS temp folders sit behind a symlink (/private), and
        # itm stores resolved paths.
        self.root = Path(self.tmp.name).resolve()
        env = {
            "XDG_STATE_HOME": str(self.root / "state"),
            "XDG_DATA_HOME": str(self.root / "data"),
            "XDG_RUNTIME_DIR": str(self.root / "run"),
            "XDG_CONFIG_HOME": str(self.root / "config"),
            "ITM_TASK_DIR": str(self.root / "home"),
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_CONFIG_GLOBAL": os.devnull,
        }
        self.env = mock.patch.dict(os.environ, env)
        self.env.start()
        for name in ("HERDR_SOCKET_PATH", "ITM_PROJECTS_DIR", "GIT_CONFIG_COUNT"):
            os.environ.pop(name, None)
        self.store = itm.Store()
        self.projects = self.root / "data/inbox-task-manager/projects"
        self.api_clone = self.projects / "github.com/acme/api"

    def tearDown(self):
        self.env.stop()
        self.tmp.cleanup()

    def folder(self, name):
        path = self.root / name
        path.mkdir(parents=True, exist_ok=True)
        return path

    def fake_clone(self):
        """clone_repo that creates the target and reports a fresh clone."""
        def clone(url, target):
            target.mkdir(parents=True)
            return True

        return mock.patch.object(itm, "clone_repo", side_effect=clone)

    @contextlib.contextmanager
    def fake_git(self, origin="https://github.com/acme/api", clone_rc=0, during_clone=None):
        """git stand-in: a clone makes a checkout (after calling ``during_clone``),
        ``remote get-url origin`` prints ``origin``. Yields the clone's process."""
        real_run = subprocess.run
        clone = mock.Mock()

        def popen(argv):
            self.assertEqual(argv[:2], ["git", "clone"])

            def wait(timeout=None):
                if clone.wait.call_count == 1:
                    if during_clone:
                        during_clone()
                    if clone_rc == 0:
                        (Path(argv[-1]) / ".git").mkdir(parents=True)
                return clone_rc

            clone.wait.side_effect = wait
            return clone

        def run(argv, *args, **kwargs):
            if argv[0] == "git" and argv[-2:] == ["get-url", "origin"]:
                return subprocess.CompletedProcess(argv, 0, origin + "\n", "")
            if argv[:3] == ["git", "ls-remote", "--get-url"]:
                return subprocess.CompletedProcess(argv, 0, argv[-1] + "\n", "")
            return real_run(argv, *args, **kwargs)

        with mock.patch.object(itm.subprocess, "Popen", side_effect=popen), \
                mock.patch.object(itm.subprocess, "run", side_effect=run):
            yield clone

    def save_project(self, name, path, **fields):
        """Save a project directly, past add_project's checks (a hand-edited tasks.json); returns its id."""
        with self.store.edit() as data:
            project_id = itm.new_project_id(data["projects"])
            data["projects"][project_id] = {"name": name, "kind": "dir", "path": str(path), **fields}
        return project_id

    def project(self, name):
        """The saved project named ``name``, with its id under "id"."""
        matches = [{**p, "id": i} for i, p in self.store.load()["projects"].items() if p["name"] == name]
        self.assertEqual(len(matches), 1, f"projects named {name}")
        return matches[0]

    def add_api(self):
        with self.fake_clone():
            return itm.add_project("git", "git@github.com:acme/api.git")


class ProjectTest(ProjectEnvTest):
    def test_add_folder_defaults_to_its_name(self):
        path = self.folder("code/notes")
        project_id = itm.add_project("dir", str(path))
        project = self.project("notes")
        self.assertEqual(project["id"], project_id)
        self.assertEqual((project["kind"], project["path"]), ("dir", str(path.resolve())))
        self.assertEqual(project["last_used_at"], project["added_at"])

    def test_same_folder_again_is_an_error(self):
        path = self.folder("code/notes")
        (self.root / "link").symlink_to(path)
        itm.add_project("dir", str(path))
        for source in (str(path) + "/", str(self.root / "link")):
            with self.subTest(source=source), self.assertRaisesRegex(itm.ItmError, "project already exists: notes"):
                itm.add_project("dir", source, "other")

    def test_taken_name_is_an_error(self):
        itm.add_project("dir", str(self.folder("a")), "Notes")
        with self.assertRaisesRegex(itm.ItmError, "project already exists: Notes .*rename it with `itm project rename Notes"):
            itm.add_project("dir", str(self.folder("b")), "notes")

    def test_two_remotes_with_one_name_need_another_name(self):
        self.add_api()
        with self.fake_clone() as clone, self.assertRaisesRegex(itm.ItmError, r"project already exists: api \(git@github.com:acme/api.git\)"):
            itm.add_project("git", "git@gitlab.com:other/api.git")
        clone.assert_not_called()
        with self.fake_clone():
            itm.add_project("git", "git@gitlab.com:other/api.git", "other-api")
        self.assertEqual(self.project("other-api")["url"], "git@gitlab.com:other/api.git")

    def test_rename_frees_the_name_and_keeps_tasks(self):
        api_id = self.add_api()
        with self.store.edit() as data:
            data["tasks"]["task-a"] = itm.new_task("A", projects=[api_id])
        with mock.patch("builtins.print"):
            itm.cmd_project_rename("api", "acme-api")
        with self.fake_clone():
            other_id = itm.add_project("git", "git@gitlab.com:other/api.git")
        data = self.store.load()
        self.assertEqual(data["projects"][api_id]["name"], "acme-api")
        self.assertEqual(data["projects"][other_id]["name"], "api")
        self.assertEqual(data["tasks"]["task-a"]["projects"], [api_id])

    def test_rename_checks_the_new_name(self):
        self.add_api()
        itm.add_project("dir", str(self.folder("notes")))
        with mock.patch("builtins.print"):
            itm.cmd_project_rename("api", "API")  # only the case changes
            with self.assertRaisesRegex(itm.ItmError, "project already exists: notes"):
                itm.cmd_project_rename("API", "Notes")
            with self.assertRaisesRegex(itm.ItmError, "1 to 60 characters"):
                itm.cmd_project_rename("API", " ")
            with self.assertRaisesRegex(itm.ItmError, "no project named"):
                itm.cmd_project_rename("missing", "x")
        self.assertEqual(self.project("API")["kind"], "git")

    def test_resolve_by_name_or_id(self):
        api_id = self.add_api()
        data = self.store.load()
        for query in ("api", "API", api_id):
            with self.subTest(query=query):
                self.assertEqual(itm.resolve_project(data, query), api_id)

    def test_missing_folder_and_bad_name(self):
        with self.assertRaisesRegex(itm.ItmError, "not a folder"):
            itm.add_project("dir", str(self.root / "nope"))
        with self.assertRaisesRegex(itm.ItmError, "1 to 60 characters"):
            itm.add_project("dir", str(self.folder("a")), "bad\tname")

    def test_add_git_clones_into_itm_folder(self):
        with self.fake_clone() as clone:
            itm.add_project("git", "git@github.com:acme/api.git")
        clone.assert_called_once_with("git@github.com:acme/api.git", self.api_clone)
        project = self.project("api")
        self.assertEqual(project, {**project, "kind": "git", "url": "git@github.com:acme/api.git", "path": str(self.api_clone)})

    def test_same_repo_again_is_an_error_without_cloning(self):
        self.add_api()
        with self.fake_clone() as clone, self.assertRaisesRegex(itm.ItmError, "project already exists: api"):
            itm.add_project("git", "https://github.com/acme/api", "api2")
        clone.assert_not_called()

    def test_folders_inside_the_projects_folder_are_refused(self):
        self.add_api()
        for path in (self.api_clone, self.projects, self.folder("data/inbox-task-manager/projects/github.com/acme")):
            with self.subTest(path=path), self.assertRaisesRegex(itm.ItmError, "inside itm's projects folder"):
                itm.add_project("dir", str(path), "x")

    def test_relative_projects_folder_is_made_absolute(self):
        with mock.patch.dict(os.environ, {"ITM_PROJECTS_DIR": "rel/projects"}):
            self.assertTrue(itm.projects_dir().is_absolute())

    def test_clone_renames_into_place_with_normal_permissions(self):
        umask = os.umask(0o022)
        os.umask(umask)
        with self.fake_git():
            itm.clone_repo("git@github.com:acme/api.git", self.api_clone)
        self.assertTrue((self.api_clone / ".git").is_dir())
        self.assertEqual(list(self.api_clone.parent.iterdir()), [self.api_clone])
        self.assertEqual(stat.S_IMODE(self.api_clone.stat().st_mode), 0o777 & ~umask)

    def test_interrupted_clone_stops_git_and_leaves_nothing(self):
        before = {sig: signal.getsignal(sig) for sig in (signal.SIGHUP, signal.SIGTERM, signal.SIGINT)}
        with self.fake_git(during_clone=lambda: signal.raise_signal(signal.SIGHUP)) as clone:
            with self.assertRaises(SystemExit) as exit:
                itm.clone_repo("git@github.com:acme/api.git", self.api_clone)
        self.assertEqual(exit.exception.code, 128 + signal.SIGHUP)
        clone.terminate.assert_called_once()
        self.assertEqual(list(self.projects.iterdir()), [])
        self.assertEqual({sig: signal.getsignal(sig) for sig in before}, before)

    def test_signals_ignored_before_stay_ignored(self):
        before = signal.signal(signal.SIGHUP, signal.SIG_IGN)  # as under nohup
        try:
            with itm.exit_on_signals():
                signal.raise_signal(signal.SIGHUP)
                self.assertIs(signal.getsignal(signal.SIGHUP), signal.SIG_IGN)
            self.assertIs(signal.getsignal(signal.SIGHUP), signal.SIG_IGN)
        finally:
            signal.signal(signal.SIGHUP, before)

    def test_cleanup_after_a_failed_clone_is_not_interrupted(self):
        real_rmtree = shutil.rmtree

        def rmtree(path, *args, **kwargs):
            signal.raise_signal(signal.SIGTERM)  # would raise SystemExit if not ignored
            real_rmtree(path, *args, **kwargs)

        with self.fake_git(clone_rc=128), mock.patch.object(itm.shutil, "rmtree", side_effect=rmtree), \
                self.assertRaisesRegex(itm.ItmError, "could not clone"):
            itm.clone_repo("git@github.com:acme/api.git", self.api_clone)
        self.assertEqual(list(self.projects.iterdir()), [])

    def test_signals_during_cleanup_are_ignored(self):
        with self.assertRaises(SystemExit), itm.exit_on_signals():
            try:
                signal.raise_signal(signal.SIGTERM)
            except SystemExit:
                self.assertEqual(signal.getsignal(signal.SIGTERM), signal.SIG_IGN)
                signal.raise_signal(signal.SIGTERM)  # ignored: cleanup goes on
                raise

    def test_clone_failure_leaves_nothing(self):
        with self.fake_git(clone_rc=128), mock.patch("builtins.print"), \
                self.assertRaisesRegex(itm.ItmError, "could not clone"):
            itm.clone_repo("git@github.com:acme/api.git", self.api_clone)
        self.assertEqual(list(self.projects.iterdir()), [])

    def test_existing_checkout_of_the_repo_is_reused(self):
        (self.api_clone / ".git").mkdir(parents=True)
        with self.fake_git(origin="git@github.com:acme/api") as clone:
            itm.clone_repo("https://github.com/acme/api", self.api_clone)
        clone.wait.assert_not_called()
        with self.fake_git(origin="git@github.com:other/api"), self.assertRaisesRegex(itm.ItmError, "not a checkout"):
            itm.clone_repo("https://github.com/acme/api", self.api_clone)

    def test_existing_folder_or_symlink_is_not_reused(self):
        checkout = self.folder("code/api")
        (checkout / ".git").mkdir()
        self.api_clone.parent.mkdir(parents=True)
        self.api_clone.symlink_to(checkout)
        with self.fake_git(), self.assertRaisesRegex(itm.ItmError, "not a checkout"):
            itm.clone_repo("git@github.com:acme/api.git", self.api_clone)
        plain = self.folder("data/inbox-task-manager/projects/github.com/acme/web")
        with self.assertRaisesRegex(itm.ItmError, "not a checkout"):
            itm.clone_repo("git@github.com:acme/web.git", plain)

    def test_task_cwd(self):
        path = self.folder("code/api")
        data = {
            "projects": {
                "p1": {"name": "api", "kind": "dir", "path": str(path)},
                "p2": {"name": "gone", "kind": "dir", "path": str(self.root / "gone")},
            },
            "tasks": {
                "task-a": itm.new_task("A", projects=["p1"]),
                "task-b": itm.new_task("B", projects=["p1"]),
                "task-plain": itm.new_task("Plain"),
                "task-gone": itm.new_task("Gone", projects=["p2"]),
                "task-both": itm.new_task("Both", projects=["p2", "p1"]),
            },
        }
        self.assertEqual(itm.task_cwd(data, "task-a"), path)
        self.assertEqual(itm.task_cwd(data, "task-b"), path)
        self.assertEqual(itm.task_cwd(data, "task-plain"), itm.task_dir())
        with mock.patch("sys.stderr"):
            self.assertEqual(itm.task_cwd(data, "task-gone"), itm.task_dir())
            # The first project whose folder exists.
            self.assertEqual(itm.task_cwd(data, "task-both"), path)
            self.assertEqual(itm.task_folders(data, "task-both"), [("api", path)])

    def test_attach_starts_the_session_in_the_project_folder(self):
        path = self.folder("code/api")
        api_id = itm.add_project("dir", str(path), "api")
        with self.store.edit() as data:
            data["tasks"]["task-a"] = itm.new_task("A", projects=[api_id])
        started = {"task-a": {"running": False, "socket_path": "/sock/a"}}
        with mock.patch.object(itm.subprocess, "run") as run, mock.patch.object(itm.shutil, "which", return_value="/bin/herdr"), \
                mock.patch.object(itm, "list_sessions", return_value=started), \
                mock.patch.object(itm, "start_with_workspaces") as build:
            itm.attach_loop("task-a")
        # A session that ran before is restored by Herdr: only attached.
        build.assert_not_called()
        run.assert_called_once_with(["herdr", "session", "attach", "task-a"], cwd=path, check=False)

    def test_first_start_opens_a_named_workspace_per_project(self):
        api, web = self.folder("code/api"), self.folder("code/web")
        ids = [itm.add_project("dir", str(api), "api"), itm.add_project("dir", str(web), "web"),
               itm.add_project("dir", str(self.folder("gone")), "gone")]
        shutil.rmtree(self.root / "gone")
        with self.store.edit() as data:
            data["tasks"]["task-a"] = itm.new_task("A", projects=[ids[2], ids[1], ids[0]])
        with mock.patch.object(itm.subprocess, "run") as run, mock.patch.object(itm.shutil, "which", return_value="/bin/herdr"), \
                mock.patch.object(itm, "list_sessions", return_value={}), \
                mock.patch.object(itm, "start_with_workspaces") as build, mock.patch("sys.stderr"):
            itm.attach_loop("task-a")
        build.assert_called_once_with("task-a", [("web", web), ("api", api)])  # missing folders left out
        run.assert_called_once_with(["herdr", "session", "attach", "task-a"], cwd=web, check=False)
        # A failure to build them still attaches.
        with mock.patch.object(itm.subprocess, "run") as run, mock.patch.object(itm.shutil, "which", return_value="/bin/herdr"), \
                mock.patch.object(itm, "list_sessions", return_value={}), \
                mock.patch.object(itm, "start_with_workspaces", side_effect=itm.ItmError("no")), \
                mock.patch("sys.stderr") as err:
            itm.attach_loop("task-a")
        run.assert_called_once()
        self.assertIn("workspaces may be missing", "".join(str(c.args[0]) for c in err.write.call_args_list))

    def test_start_with_workspaces(self):
        api, web = self.folder("code/api"), self.folder("code/web")
        sessions = iter([{}, {"task-a": {"running": True, "socket_path": "/sock/a"}}])
        calls = []

        def herdr(args, socket=None):
            calls.append((args, socket))
            return {"result": {"workspaces": []}}

        with mock.patch.object(itm.subprocess, "Popen") as popen, \
                mock.patch.dict(os.environ, {"HERDR_SOCKET_PATH": "/elsewhere", "HERDR_WORKSPACE_ID": "w9"}), \
                mock.patch.object(itm, "list_sessions", side_effect=lambda: next(sessions)), \
                mock.patch.object(itm, "herdr_json", side_effect=herdr), mock.patch.object(itm.time, "sleep"):
            popen.return_value.poll.return_value = None  # still running
            itm.start_with_workspaces("task-a", [("api", api), ("-web", web)])
        argv, kwargs = popen.call_args.args[0], popen.call_args.kwargs
        self.assertEqual((argv, kwargs["env"]["HERDR_SESSION"], kwargs["cwd"]), (["herdr", "server"], "task-a", api))
        self.assertTrue(kwargs["start_new_session"])
        self.assertFalse([k for k in kwargs["env"] if k.startswith("HERDR_") and k != "HERDR_SESSION"])
        self.assertEqual(calls, [
            (["workspace", "list"], "/sock/a"),
            (["workspace", "create", "--cwd", str(api), "--label", "api", "--focus"], "/sock/a"),
            # Herdr takes a value starting with "-" as is (checked against Herdr 0.9.3).
            (["workspace", "create", "--cwd", str(web), "--label", "-web", "--no-focus"], "/sock/a"),
        ])
        with mock.patch.object(itm.subprocess, "Popen") as popen, mock.patch.object(itm, "list_sessions", return_value={}), \
                mock.patch.object(itm, "SESSION_START_TIMEOUT", 0), mock.patch.object(itm.time, "sleep"), \
                self.assertRaisesRegex(itm.ItmError, "did not start"):
            popen.return_value.poll.return_value = None
            itm.start_with_workspaces("task-a", [("api", api)])
        with mock.patch.object(itm.subprocess, "Popen") as popen, mock.patch.object(itm, "list_sessions", return_value={}), \
                mock.patch.object(itm.time, "sleep") as sleep, self.assertRaisesRegex(itm.ItmError, "exited with 1"):
            popen.return_value.poll.return_value = popen.return_value.returncode = 1
            itm.start_with_workspaces("task-a", [("api", api)])
        sleep.assert_not_called()  # not waited for
        # Built already (another client won the race): no workspaces added.
        calls.clear()
        running = {"task-a": {"running": True, "socket_path": "/sock/a"}}
        with mock.patch.object(itm.subprocess, "Popen"), mock.patch.object(itm, "list_sessions", return_value=running), \
                mock.patch.object(itm, "herdr_json", side_effect=lambda a, s=None: calls.append(a) or
                                  {"result": {"workspaces": [{"workspace_id": "w1"}]}}):
            itm.start_with_workspaces("task-a", [("api", api)])
        self.assertEqual(calls, [["workspace", "list"]])

    def test_first_start_is_built_once(self):
        api = self.folder("code/api")
        api_id = itm.add_project("dir", str(api), "api")
        with self.store.edit() as data:
            data["tasks"]["task-a"] = itm.new_task("A", projects=[api_id])
        sessions: dict = {}

        def build(name, folders):  # the session exists once built
            sessions[name] = {"running": True}

        with mock.patch.object(itm, "list_sessions", side_effect=lambda: sessions), \
                mock.patch.object(itm, "start_with_workspaces", side_effect=build) as start:
            data = self.store.load()
            self.assertEqual(itm.prepare_first_start("task-a", data), [("api", api)])
            self.assertEqual(itm.prepare_first_start("task-a", data), [("api", api)])  # a second client
        start.assert_called_once()

    def test_projects_by_recent(self):
        projects = {
            "p1": {"name": "old", "kind": "dir", "path": "/old", "last_used_at": 1},
            "p2": {"name": "new", "kind": "git", "path": "/p/new", "url": "git@h:o/new.git", "last_used_at": 9},
        }
        self.assertEqual([pid for pid, _ in itm.projects_by_recent(projects)], ["p2", "p1"])

    def test_remove_refused_while_used(self):
        a_id = itm.add_project("dir", str(self.folder("a")), "a")
        with self.store.edit() as data:
            data["tasks"]["task-x"] = {**itm.new_task("X", projects=[a_id]), "state": "archived"}
        with self.assertRaisesRegex(itm.ItmError, "project 'a' is used by 1 task"):
            itm.cmd_project_remove("a", None)
        self.assertIn(a_id, self.store.load()["projects"])

    def test_remove_folder_keeps_it(self):
        path = self.folder("a")
        itm.add_project("dir", str(path), "a")
        with mock.patch("builtins.print"):
            itm.cmd_project_remove("A", None)
        self.assertEqual(self.store.load()["projects"], {})
        self.assertTrue(path.is_dir())

    def test_remove_git_deletes_clone_and_empty_parents(self):
        self.add_api()
        with mock.patch.object(itm, "is_checkout_of", return_value=True), mock.patch("builtins.print"):
            itm.cmd_project_remove("api", delete_clone=True)
        self.assertEqual(list(self.projects.iterdir()), [])

    def test_remove_git_asks_about_a_clean_clone(self):
        for answer, kept in (("keep", True), ("if-clean", False), (None, True)):
            with self.subTest(answer=answer):
                self.add_api()
                screens = mock.Mock()
                screens.remove_project_dialog.return_value = answer
                with mock.patch.object(itm, "is_checkout_of", return_value=True), mock.patch("builtins.print"), \
                        mock.patch.object(itm, "local_work", return_value=None), \
                        mock.patch.object(itm, "screens", return_value=screens), \
                        mock.patch.object(itm.sys.stdin, "isatty", return_value=True), \
                        mock.patch.object(itm.sys.stdout, "isatty", return_value=True):
                    if answer is None:
                        with self.assertRaises(itm.Cancelled):
                            itm.cmd_project_remove("api", delete_clone=None)
                    else:
                        itm.cmd_project_remove("api", delete_clone=None)
                info = screens.remove_project_dialog.call_args[0][0]
                self.assertEqual((info["name"], info["clone"], info["local_work"]), ("api", "deletable", None))
                self.assertEqual(self.api_clone.is_dir(), kept)
                # Cancelling changes nothing.
                self.assertEqual(bool(self.store.load()["projects"]), answer is None)
                shutil.rmtree(self.projects, ignore_errors=True)
                with self.store.edit() as data:
                    data["projects"].clear()

    def test_remove_git_keeps_a_clone_with_local_work_unless_forced(self):
        for forced in (None, True):
            with self.subTest(forced=forced):
                self.add_api()
                with mock.patch.object(itm, "is_checkout_of", return_value=True), mock.patch("builtins.print"), \
                        mock.patch("sys.stderr"), mock.patch.object(itm, "screens") as screens, \
                        mock.patch.object(itm, "local_work", return_value="unpushed commits"):
                    itm.cmd_project_remove("api", delete_clone=forced)
                screens.assert_not_called()
                self.assertEqual(self.api_clone.is_dir(), forced is None)
                shutil.rmtree(self.projects, ignore_errors=True)

    def test_if_clean_checks_local_work_again_when_deleting(self):
        self.add_api()
        with mock.patch.object(itm, "is_checkout_of", return_value=True), \
                mock.patch.object(itm, "local_work", return_value="stashes"):
            lines = itm.remove_project("api", "if-clean")
        self.assertTrue(self.api_clone.is_dir())
        self.assertIn("it has stashes", lines[-1])

    def test_removal_info_changes_nothing(self):
        api_id = self.add_api()
        with mock.patch.object(itm, "is_checkout_of", return_value=False):
            info = itm.removal_info("API")
        self.assertEqual((info["id"], info["clone"], info["users"]), (api_id, "foreign", []))
        shutil.rmtree(self.api_clone)
        self.assertEqual(itm.removal_info("api")["clone"], "missing")
        self.assertIn(api_id, self.store.load()["projects"])

    def test_clone_child_never_prompts(self):
        with mock.patch.dict(os.environ, {"ITM_POPUP": "1", "GIT_SSH_COMMAND": "ssh -i key"}):
            env = itm.clone_env()
        self.assertNotIn("ITM_POPUP", env)
        self.assertEqual(env["GIT_TERMINAL_PROMPT"], "0")
        self.assertEqual(env["GIT_SSH_COMMAND"], "ssh -i key -o BatchMode=yes")
        self.assertIn("in a terminal", itm.clone_hint("fatal: could not read Username for 'https://x'"))
        self.assertIsNone(itm.clone_hint("fatal: repository not found"))

    @unittest.skipUnless(shutil.which("git"), "needs git")
    def test_local_work(self):
        repo = self.folder("repo")
        env = {"GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "t@t", "GIT_COMMITTER_NAME": "t", "GIT_COMMITTER_EMAIL": "t@t"}
        with mock.patch.dict(os.environ, env):
            git = lambda *args: subprocess.run(["git", "-C", str(repo), *args], check=True, capture_output=True)
            git("init", "-q")
            self.assertIsNone(itm.local_work(repo))  # an empty clone
            (repo / "notes.txt").write_text("draft")
            self.assertEqual(itm.local_work(repo), "uncommitted changes")
            git("add", ".")
            git("commit", "-qm", "draft")
            self.assertEqual(itm.local_work(repo), "unpushed commits")
            git("update-ref", "refs/remotes/origin/main", "HEAD")  # as if pushed
            self.assertIsNone(itm.local_work(repo))
            git("config", "status.showUntrackedFiles", "no")
            (repo / "new.txt").write_text("new")
            self.assertEqual(itm.local_work(repo), "uncommitted changes")
            (repo / "new.txt").unlink()
            git("checkout", "-q", "--detach")
            git("commit", "-q", "--allow-empty", "-m", "detached")
            self.assertEqual(itm.local_work(repo), "unpushed commits")
            git("tag", "only-here")
            git("checkout", "-q", "-")
            self.assertEqual(itm.local_work(repo), "unpushed commits")  # reachable from the tag only
        self.assertIn("git could not tell", itm.local_work(self.folder("not-a-repo")))

    def test_remove_git_keeps_what_is_not_its_clone(self):
        cases = {
            "not a checkout": lambda: None,
            "another project inside": lambda: self.save_project("docs", self.folder(
                "data/inbox-task-manager/projects/github.com/acme/api/docs")),
        }
        for label, setup in cases.items():
            with self.subTest(label):
                self.add_api()
                setup()
                checkout = label != "not a checkout"
                with mock.patch.object(itm, "is_checkout_of", return_value=checkout), \
                        mock.patch("sys.stderr"), mock.patch("builtins.print"):
                    itm.cmd_project_remove("api", delete_clone=True)
                self.assertTrue(self.api_clone.is_dir())
                with mock.patch("builtins.print"):
                    if checkout:
                        itm.cmd_project_remove("docs", None)
                shutil.rmtree(self.projects)

    def test_remove_git_keeps_a_clone_behind_a_symlinked_folder(self):
        elsewhere = self.folder("code/acme/api")
        (elsewhere / ".git").mkdir()
        self.save_project("api", self.api_clone, kind="git", url="git@github.com:acme/api.git")
        self.api_clone.parent.parent.mkdir(parents=True)
        self.api_clone.parent.symlink_to(elsewhere.parent)
        with mock.patch.object(itm, "is_checkout_of", return_value=True), mock.patch("sys.stderr"), \
                mock.patch("builtins.print"):
            itm.cmd_project_remove("api", delete_clone=True)
        self.assertTrue((elsewhere / ".git").is_dir())

    def test_remove_git_keeps_a_symlinked_clone(self):
        checkout = self.folder("code/api")
        (checkout / ".git").mkdir()
        self.save_project("api", self.api_clone, kind="git", url="git@github.com:acme/api.git")
        self.api_clone.parent.mkdir(parents=True)
        self.api_clone.symlink_to(checkout)
        with self.fake_git(), mock.patch("sys.stderr"), mock.patch("builtins.print"):
            itm.cmd_project_remove("api", delete_clone=True)
        self.assertTrue((checkout / ".git").is_dir())


class CliTest(unittest.TestCase):
    def test_cancelling_closes_a_popup_without_waiting(self):
        with mock.patch.dict(os.environ, {"ITM_POPUP": "1"}), mock.patch.object(itm.time, "sleep") as sleep, \
                mock.patch("sys.stderr"):
            for error, rc in ((itm.Cancelled("cancelled"), 1), (KeyboardInterrupt(), 130)):
                with self.subTest(error=error), mock.patch.object(itm, "run", side_effect=error):
                    self.assertEqual(itm.main(["current"]), rc)
            sleep.assert_not_called()

    def test_screens_need_a_terminal(self):
        with mock.patch("sys.stdin") as stdin:
            stdin.isatty.return_value = False
            with self.assertRaisesRegex(itm.ItmError, "needs a terminal"):
                itm.screens()

    def test_no_command_opens_the_switcher(self):
        with mock.patch.object(itm, "cmd_ui", return_value=0) as ui:
            self.assertEqual(itm.main([]), 0)
            itm.main(["ui", "--view", "archived"])
        self.assertEqual([c.args for c in ui.call_args_list], [("inbox",), ("archived",)])

    def test_switcher_result_switches_after_it_closed(self):
        screens = mock.Mock()
        for result, switched in ((("switch", "task-a"), True), (None, False)):
            screens.switcher.return_value = result
            with self.subTest(result=result), mock.patch.object(itm, "screens", return_value=screens), \
                    mock.patch.object(itm, "switch_to", return_value=0) as switch_to:
                self.assertEqual(itm.cmd_ui("inbox"), 0)
            self.assertEqual(switch_to.called, switched)


class TaskEnvTest(ProjectEnvTest):
    def setUp(self):
        super().setUp()
        self.patches = [
            mock.patch.object(itm, "list_sessions", return_value={}),
            mock.patch.object(itm, "switch_to", return_value=0),
            mock.patch("builtins.print"),
        ]
        self.switch_to = self.patches[1].start()
        for patch in (self.patches[0], self.patches[2]):
            patch.start()

    def tearDown(self):
        for patch in self.patches:
            patch.stop()
        super().tearDown()

    def task(self, name):
        return self.store.load()["tasks"][name]

    def form(self, answers, **kwargs):
        """cmd_new with the TUI form answering ``answers`` (None: Esc); returns the form mock."""
        screens = mock.Mock()
        screens.task_form.return_value = answers
        with mock.patch.object(itm, "screens", return_value=screens):
            itm.cmd_new(kwargs.pop("title", None), **kwargs)
        return screens.task_form

    def answers(self, title="Ask", description=None, projects=(), until=None):
        return {"title": title, "description": description, "projects": list(projects), "until": until}


class NewTaskTest(TaskEnvTest):
    def test_description_and_project(self):
        api_id = itm.add_project("dir", str(self.folder("code/api")), "api")
        web_id = itm.add_project("dir", str(self.folder("code/web")), "web")
        itm.cmd_new("Fix login", description="  token expires  ", projects=["API", "web", web_id])
        task = self.task("task-fix-login")
        self.assertEqual((task["description"], task["projects"]), ("token expires", [api_id, web_id]))
        self.switch_to.assert_called_once_with("task-fix-login")

    def test_creating_a_task_marks_its_project_used(self):
        old_id = itm.add_project("dir", str(self.folder("old")), "old")
        new_id = itm.add_project("dir", str(self.folder("new")), "new")
        with mock.patch.object(itm, "now", return_value=itm.now() + 60):
            itm.cmd_new("Use old", projects=["old"])
        self.assertEqual([pid for pid, _ in itm.projects_by_recent(self.store.load()["projects"])], [old_id, new_id])

    def test_plain_task_has_no_optional_fields(self):
        itm.cmd_new("Plain", description="   ")
        self.assertNotIn("description", self.task("task-plain"))
        self.assertNotIn("projects", self.task("task-plain"))

    def test_snoozed_at_creation_does_not_switch(self):
        itm.cmd_new("Later", snooze="2h")
        task = self.task("task-later")
        self.assertEqual(task["state"], "snoozed")
        self.assertGreater(task["snoozed_until"], itm.now() + 7000)
        self.switch_to.assert_not_called()

    def test_invalid_input_writes_nothing(self):
        for kwargs in ({"snooze": "soonish"}, {"projects": ["missing"]}):
            with self.subTest(**kwargs), self.assertRaises(itm.ItmError):
                itm.cmd_new("Nope", **kwargs)
        self.assertEqual(self.store.load()["tasks"], {})

    def test_form_gets_what_was_given(self):
        api_id = itm.add_project("dir", str(self.folder("code/api")), "api")
        form = self.form(self.answers(title="Given", description="d", projects=[api_id]), title="Given",
                         description="d", projects=["API"], snooze="1h", interactive=True)
        kwargs = form.call_args.kwargs
        self.assertEqual((kwargs["title"], kwargs["description"], kwargs["projects"]), ("Given", "d", [api_id]))
        self.assertGreater(kwargs["until"], itm.now())
        self.assertEqual(self.task("task-given")["projects"], [api_id])

    def test_form_answers(self):
        api_id = itm.add_project("dir", str(self.folder("code/api")), "api")
        self.form(self.answers(description="notes", projects=[api_id], until=itm.now() + 600))
        task = self.task("task-ask")
        self.assertEqual((task["description"], task["projects"], task["state"]), ("notes", [api_id], "snoozed"))
        self.switch_to.assert_not_called()

    def test_form_without_optional_fields_switches(self):
        self.form(self.answers())
        task = self.task("task-ask")
        self.assertEqual(task["state"], "active")
        self.assertNotIn("description", task)
        self.switch_to.assert_called_once_with("task-ask")

    def test_esc_in_the_form_saves_nothing(self):
        with self.assertRaises(itm.Cancelled):
            self.form(None)
        self.assertEqual(self.store.load()["tasks"], {})

    def test_plain_herdr_client(self):
        with mock.patch.dict(os.environ, {"HERDR_SOCKET_PATH": "/sock"}), \
                mock.patch.object(itm, "switchable_session", side_effect=itm.ItmError("not itm")):
            with self.assertRaisesRegex(itm.ItmError, "not itm"):
                itm.cmd_new("Typed")  # a title alone: fail before saving
            itm.cmd_new("Later", snooze="1h")
            with self.assertRaisesRegex(itm.ItmError, "task-kept saved without switching: not itm"):
                self.form(self.answers(title="Kept"))  # the form keeps what was typed
        self.assertEqual(sorted(self.store.load()["tasks"]), ["task-kept", "task-later"])
        self.switch_to.assert_not_called()

    def test_rows_show_the_project_name(self):
        api_id = itm.add_project("dir", str(self.folder("code/api")), "api")
        web_id = itm.add_project("dir", str(self.folder("code/web")), "web")
        itm.cmd_new("Fix", projects=["api", "web"])
        row = itm.task_rows(self.store.load(), "all", {}, {})[0]
        # "project" and "project_id" keep naming the first project for scripts written before.
        self.assertEqual((row["project"], row["project_id"]), ("api", api_id))
        self.assertEqual(row["projects"], [{"id": api_id, "name": "api"}, {"id": web_id, "name": "web"}])
        self.assertEqual(itm.project_rows(self.store.load()), [
            {"id": api_id, "name": "api", "kind": "dir", "path": str(self.folder("code/api")), "url": None, "tasks": 1},
            {"id": web_id, "name": "web", "kind": "dir", "path": str(self.folder("code/web")), "url": None, "tasks": 1},
        ])

    def test_list_prints_project_and_waiting(self):
        itm.add_project("dir", str(self.folder("code/api")), "api")
        itm.add_project("dir", str(self.folder("code/web")), "web")
        itm.cmd_new("Fix", projects=["api", "web"])
        with mock.patch("builtins.print") as out:
            itm.cmd_list("all", as_json=False)
        self.assertEqual(out.call_args[0][0], "task-fix\tactive\tFix\tapi,web\t0")


class EditTaskTest(TaskEnvTest):
    def test_title_and_description(self):
        itm.cmd_new("Old", description="text")
        itm.edit_task("task-old", title=" New ", description="")
        task = self.task("task-old")
        self.assertEqual(task["title"], "New")
        self.assertNotIn("description", task)
        with self.assertRaisesRegex(itm.ItmError, "title cannot be empty"):
            itm.edit_task("task-old", title=" ")

    def test_projects_can_change_and_say_what_happens_to_the_session(self):
        api_id = itm.add_project("dir", str(self.folder("code/api")), "api")
        web_id = itm.add_project("dir", str(self.folder("code/web")), "web")
        itm.cmd_new("Later", snooze="1h")
        self.assertEqual(itm.edit_task("task-later", add=["api"]),
                         [f"its session will start with a workspace in {self.folder('code/api')}"])
        self.assertEqual(itm.edit_task("task-later", add=["web"]),
                         [f"its session will start with a workspace in {self.folder('code/web')}"])
        self.assertEqual(self.task("task-later")["projects"], [api_id, web_id])
        self.assertEqual(itm.edit_task("task-later", remove=["API"]), [])
        self.assertEqual(self.task("task-later")["projects"], [web_id])
        self.assertEqual(itm.edit_task("task-later", remove=["api"]), [])  # not there: nothing to do
        self.assertEqual(self.task("task-later")["projects"], [web_id])
        with self.assertRaisesRegex(itm.ItmError, "no project named 'nope'"):
            itm.edit_task("task-later", remove=["nope"])
        self.assertEqual(itm.edit_task("task-later", projects=[]), [])
        self.assertNotIn("projects", self.task("task-later"))
        stopped = {"task-later": {"running": False, "socket_path": "/sock/later"}}
        with mock.patch.object(itm, "list_sessions", return_value=stopped):
            notes = itm.edit_task("task-later", title="Fine", projects=["web", "api"])
        self.assertIn("stopped session keeps its workspaces", notes[0])
        task = self.task("task-later")
        self.assertEqual((task["title"], task["projects"]), ("Fine", [web_id, api_id]))

    def test_a_running_session_gets_a_named_workspace_per_new_project(self):
        api_id = itm.add_project("dir", str(self.folder("code/api")), "api")
        web_id = itm.add_project("dir", str(self.folder("code/web")), "web")
        itm.cmd_new("Run", switch=False)
        running = {"task-run": {"running": True, "socket_path": "/sock/run"}}
        with mock.patch.object(itm, "list_sessions", return_value=running), \
                mock.patch.object(itm, "herdr_json", return_value={}) as herdr:
            notes = itm.edit_task("task-run", add=["api", "web"])
        self.assertEqual([c.args for c in herdr.call_args_list], [
            (["workspace", "create", "--cwd", str(self.folder("code/api")), "--label", "api", "--no-focus"], "/sock/run"),
            (["workspace", "create", "--cwd", str(self.folder("code/web")), "--label", "web", "--no-focus"], "/sock/run"),
        ])
        self.assertEqual(notes, [f"opened a workspace in {self.folder('code/api')}",
                                 f"opened a workspace in {self.folder('code/web')}"])
        self.assertEqual(self.task("task-run")["projects"], [api_id, web_id])
        # A failure to open one does not undo the change; it is reported.
        itm.edit_task("task-run", remove=["api"])
        with mock.patch.object(itm, "list_sessions", return_value=running), \
                mock.patch.object(itm, "herdr_json", side_effect=itm.ItmError("no answer")):
            self.assertIn("could not open a workspace", itm.edit_task("task-run", add=["api"])[0])
        self.assertEqual(self.task("task-run")["projects"], [web_id, api_id])

    def test_edit_reports_without_failing(self):
        api_id = itm.add_project("dir", str(self.folder("code/api")), "api")
        itm.cmd_new("Run", switch=False)
        broken = mock.patch.object(itm, "list_sessions", side_effect=itm.ItmError("no answer"))
        with broken:
            # Removing or keeping projects never asks Herdr.
            self.assertEqual(itm.edit_task("task-run", title="T", projects=[]), [])
            notes = itm.edit_task("task-run", add=["api"])  # saved even though Herdr does not answer
        self.assertIn("could not tell whether its session runs", notes[0])
        self.assertEqual(self.task("task-run")["projects"], [api_id])
        with broken:
            self.assertEqual(itm.edit_task("task-run", title="Same", add=["API"]), [])
        shutil.rmtree(self.folder("code/api"))
        itm.edit_task("task-run", projects=[])
        with broken:
            self.assertIn("is missing", itm.edit_task("task-run", add=["api"])[0])

    def test_editing_the_task_you_are_in_focuses_the_new_workspace(self):
        itm.add_project("dir", str(self.folder("code/api")), "api")
        itm.cmd_new("Here", switch=False)
        running = {"task-here": {"running": True, "socket_path": "/sock/here"}}
        with mock.patch.dict(os.environ, {"HERDR_SOCKET_PATH": "/sock/here"}), \
                mock.patch.object(itm, "list_sessions", return_value=running), \
                mock.patch.object(itm, "herdr_json", return_value={}) as herdr, \
                mock.patch("builtins.print") as out:
            itm.cmd_edit("here", None, None, add=["api"])
        self.assertIn("--focus", herdr.call_args.args[0])
        self.assertEqual(out.call_args.args[0], f"task-here updated; opened a workspace in {self.folder('code/api')}")

    def test_cmd_edit_opens_the_form_without_options(self):
        api_id = itm.add_project("dir", str(self.folder("code/api")), "api")
        itm.cmd_new("Old")
        screens = mock.Mock()
        screens.edit_form.return_value = {"title": "From form", "description": "d", "add": [api_id], "remove": []}
        with mock.patch.object(itm, "screens", return_value=screens):
            itm.cmd_edit("old", None, None)
        self.assertEqual((self.task("task-old")["title"], self.task("task-old")["projects"]), ("From form", [api_id]))

    def test_cli_options(self):
        itm.add_project("dir", str(self.folder("code/api")), "api")
        itm.add_project("dir", str(self.folder("code/web")), "web")
        itm.main(["new", "Both", "-p", "api", "-p", "web", "--no-switch"])
        self.assertEqual([self.project(n)["id"] for n in ("api", "web")], self.task("task-both")["projects"])
        itm.main(["edit", "both", "--remove-project", "api"])
        itm.main(["edit", "both", "--add-project", "api"])
        self.assertEqual([self.project(n)["id"] for n in ("web", "api")], self.task("task-both")["projects"])
        itm.main(["edit", "both", "--no-project"])
        self.assertNotIn("projects", self.task("task-both"))
        with mock.patch("builtins.print") as out:
            self.assertEqual(itm.main(["edit", "both", "--project", "api", "--add-project", "web"]), 1)
        self.assertIn("set the projects", out.call_args.args[0])


class WorkspaceSyncTest(TaskEnvTest):
    """Projects found in a running task's workspaces (the daemon's sync)."""

    def setUp(self):
        super().setUp()
        self.api = itm.add_project("dir", str(self.folder("code/api")), "api")
        self.web = itm.add_project("dir", str(self.folder("code/web")), "web")

    def workspaces(self, *folders):
        return [{"id": f"w{i}", "label": "", "focused": i == 0, "folder": str(f) if f else None}
                for i, f in enumerate(folders)]

    def test_folders_map_to_the_innermost_saved_project(self):
        projects = self.store.load()["projects"]
        lib = itm.add_project("dir", str(self.folder("code/api/lib")), "lib")
        projects = self.store.load()["projects"]
        self.assertEqual(itm.folder_project(projects, str(self.folder("code/api/src"))), self.api)
        self.assertEqual(itm.folder_project(projects, str(self.folder("code/api/lib/x"))), lib)
        self.assertIsNone(itm.folder_project(projects, str(self.root / "code/apix")))  # a prefix is not a folder
        self.assertIsNone(itm.folder_project(projects, None))
        found = itm.workspace_projects(projects, self.workspaces(self.folder("code/web"), self.folder("code/api"),
                                                                 self.folder("code/api/src"), None))
        self.assertEqual(found, ([self.web, self.api], False))  # one workspace's folder is unknown

    def test_home_is_not_a_project_unless_saved(self):
        # Tasks without a project start in the home folder; projects inside it must not claim it.
        home = itm.task_dir()
        home.mkdir(parents=True, exist_ok=True)
        inside = itm.add_project("dir", str(self.folder("home/code/app")), "app")
        projects = self.store.load()["projects"]
        self.assertIsNone(itm.folder_project(projects, str(home)))
        self.assertEqual(itm.workspace_projects(projects, self.workspaces(home)), ([], True))
        with mock.patch.dict(os.environ, {"HOME": str(home)}):
            self.assertIsNone(itm.folder_project(projects, os.path.expanduser("~")))
        self.assertEqual(itm.folder_project(projects, str(home / "code/app")), inside)
        # A project without a folder (named only) never matches a workspace.
        projects["named"] = {"name": "named", "kind": "name"}
        self.assertIsNone(itm.folder_project(projects, str(home)))

    def test_found_projects_come_and_go_with_their_workspaces(self):
        task = itm.new_task("T", projects=[self.api])
        itm.sync_task_projects(task, [self.api, self.web], remove=True)
        self.assertEqual((task["projects"], task["found_projects"]), ([self.api, self.web], [self.web]))
        itm.sync_task_projects(task, [self.api], remove=True)  # web's workspace closed
        self.assertEqual(task["projects"], [self.api])
        self.assertNotIn("found_projects", task)
        itm.sync_task_projects(task, [], remove=True)  # api was added by hand: it stays
        self.assertEqual(task["projects"], [self.api])
        self.assertNotIn("workspace_projects", task)

    def test_without_removal_projects_are_only_added(self):
        task = itm.new_task("T")
        itm.sync_task_projects(task, [self.web], remove=False)
        itm.sync_task_projects(task, [], remove=False)
        self.assertEqual(task["projects"], [self.web])

    def test_a_workspace_still_opening_does_not_remove_its_project(self):
        task = itm.new_task("T")
        itm.sync_task_projects(task, [self.web], remove=True)
        task["workspace_projects"] = []  # e.g. the daemon restarted with the session rebuilt
        itm.sync_task_projects(task, [], remove=True)
        self.assertEqual(task["projects"], [self.web])  # never seen closing: kept

    def test_adding_by_hand_keeps_a_found_project(self):
        itm.cmd_new("Run", switch=False)
        with self.store.edit() as data:
            itm.sync_task_projects(data["tasks"]["task-run"], [self.web], remove=True)
        itm.edit_task("task-run", add=["web"])  # already there: now the user's
        self.assertNotIn("found_projects", self.task("task-run"))
        with self.store.edit() as data:
            itm.sync_task_projects(data["tasks"]["task-run"], [], remove=True)
        self.assertEqual(self.task("task-run")["projects"], [self.web])

    def daemon_running(self):
        itm.write_atomic(itm.status_path(), json.dumps({"updated_at": itm.now(), "sessions": {}}))

    def test_a_project_with_an_open_workspace_is_kept_while_following(self):
        itm.cmd_new("Run", projects=["api", "web"], switch=False)
        with self.store.edit() as data:
            itm.sync_task_projects(data["tasks"]["task-run"], [self.api], remove=True)
        self.daemon_running()
        notes = itm.edit_task("task-run", title="Renamed", remove=["api", "web"])
        self.assertIn("could not remove api: it still has a workspace", notes[0])
        task = self.task("task-run")  # the rest of the edit is saved
        self.assertEqual((task["title"], task["projects"]), ("Renamed", [self.api]))
        # Without a running daemon nothing keeps the record current: it does not hold the project.
        itm.status_path().unlink()
        itm.edit_task("task-run", remove=["api"])
        self.assertNotIn("projects", self.task("task-run"))
        itm.edit_task("task-run", add=["api"])
        with self.store.edit() as data:
            itm.sync_task_projects(data["tasks"]["task-run"], [self.api], remove=True)
        self.daemon_running()
        itm.set_config("herdr", "projects_from_workspaces", False)
        self.assertEqual(itm.edit_task("task-run", remove=["api"]), [])
        self.assertNotIn("projects", self.task("task-run"))
        # A broken settings file is "off", as for the daemon.
        itm.set_config("herdr", "projects_from_workspaces", True)
        itm.config_path().write_bytes(b"# n\xe3o\n")
        self.assertFalse(itm.follows_workspaces())

    def test_a_named_workspace_stays_with_its_project(self):
        projects = self.store.load()["projects"]
        api, web = self.folder("code/api"), self.folder("code/web")
        named = {"id": "w1", "label": "api", "focused": True, "folder": str(web / "src")}  # its pane went to web
        self.assertEqual(itm.workspace_project(projects, named), self.api)
        self.assertEqual(itm.workspace_project(projects, {**named, "label": "API", "folder": None}), self.api)
        # An unnamed workspace's label is its folder's name: it says nothing about projects.
        other = self.folder("other/api")
        self.assertIsNone(itm.workspace_project(projects, {**named, "label": "api", "folder": str(other)}))
        self.assertEqual(itm.workspace_project(projects, {**named, "label": "web", "folder": str(web)}), self.web)
        self.assertEqual(itm.workspace_project(projects, {**named, "label": "my stuff", "folder": str(api)}),
                         self.api)

    def test_untrusted_passes_only_add(self):
        itm.cmd_new("Run", switch=False)
        sessions = {"task-run": {"running": True, "socket_path": "/sock/run"}}
        found = {"/sock/run": self.workspaces(self.folder("code/api"), self.folder("code/web"))}
        config, settled = itm.parse_config(""), set()
        with mock.patch.object(itm, "list_sessions", return_value=sessions), \
                mock.patch.object(itm, "list_agents", return_value=[]), \
                mock.patch.object(itm, "live_clients", return_value={}), \
                mock.patch.object(itm, "session_workspaces", side_effect=lambda socket: found[socket]):
            itm.poll_once(self.store, {}, config, settled)
            self.assertEqual(settled, {"task-run"})
            found["/sock/run"] = self.workspaces(self.folder("code/api"), None)  # web's folder unknown
            itm.poll_once(self.store, {}, config, settled)
            self.assertEqual(self.task("task-run")["projects"], [self.api, self.web])
            # A daemon that just started has not read the session before: no removal yet.
            found["/sock/run"] = self.workspaces(self.folder("code/api"))
            itm.poll_once(self.store, {}, config, set())
            self.assertEqual(self.task("task-run")["projects"], [self.api, self.web])
            itm.poll_once(self.store, {}, config, settled)
            self.assertEqual(self.task("task-run")["projects"], [self.api])

    def test_the_record_of_open_workspaces_ends_with_the_session(self):
        itm.cmd_new("Run", projects=["api"], switch=False)
        itm.cmd_new("Other", projects=["web"], switch=False)
        with self.store.edit() as data:
            for name in ("task-run", "task-other"):
                data["tasks"][name]["workspace_projects"] = itm.task_projects(data["tasks"][name])
        sessions = {"task-run": {"running": False, "socket_path": "/sock/run"}}
        with mock.patch.object(itm, "list_sessions", return_value=sessions), \
                mock.patch.object(itm, "live_clients", return_value={}):
            itm.poll_once(self.store, {}, itm.parse_config(""))
        self.assertNotIn("workspace_projects", self.task("task-run"))  # stopped
        self.assertNotIn("workspace_projects", self.task("task-other"))  # gone
        with self.store.edit() as data:
            data["tasks"]["task-run"]["workspace_projects"] = [self.api]
        with mock.patch.object(itm, "move_clients_off"), mock.patch.object(itm, "stop_session_detached"):
            itm.archive_task("task-run")
        self.assertNotIn("workspace_projects", self.task("task-run"))

    def test_hand_edits_and_old_files(self):
        with self.store.edit() as data:
            data["tasks"]["task-x"] = {**itm.new_task("X"), "projects": None}
            data["tasks"]["task-y"] = {**itm.new_task("Y"), "projects": [self.api, self.web], "project": None}
            data["tasks"]["task-z"] = {**itm.new_task("Z"), "project": self.web}
            data["tasks"]["task-g"] = {**itm.new_task("G"), "projects": ["gone", self.api]}
        data = self.store.load()
        self.assertEqual(itm.task_projects(data["tasks"]["task-x"]), [])
        self.assertEqual(data["tasks"]["task-y"]["projects"], [self.api, self.web])  # not replaced by "project"
        self.assertNotIn("project", data["tasks"]["task-y"])
        self.assertEqual(data["tasks"]["task-z"]["projects"], [self.web])
        itm.task_rows(data, "all", {}, {})
        itm.edit_task("task-g", remove=["gone"])  # an id the task holds, of a project no longer saved
        self.assertEqual(self.task("task-g")["projects"], [self.api])
        with self.store.edit() as data:
            data["tasks"]["task-g"]["projects"] = ["gone"]
        itm.edit_task("task-g", projects=[])
        self.assertIn("Project removed\n\ngone", itm.notes_path("task-g").read_text())

    def test_order_puts_projects_first(self):
        itm.cmd_new("Run", projects=["api", "web"], switch=False)
        itm.edit_task("task-run", order=[self.web, self.api])
        self.assertEqual(self.task("task-run")["projects"], [self.web, self.api])

    def test_daemon_follows_workspaces(self):
        itm.cmd_new("Run", projects=["api"], switch=False)
        sessions = {"task-run": {"running": True, "socket_path": "/sock/run"}}
        found = {"/sock/run": self.workspaces(self.folder("code/api"), self.folder("code/web/src"))}
        config = itm.parse_config("")
        with mock.patch.object(itm, "list_sessions", return_value=sessions), \
                mock.patch.object(itm, "list_agents", return_value=[]), \
                mock.patch.object(itm, "live_clients", return_value={}), \
                mock.patch.object(itm, "session_workspaces", side_effect=lambda socket: found[socket]):
            itm.poll_once(self.store, {}, config)
            self.assertEqual(self.task("task-run")["projects"], [self.api, self.web])
            found["/sock/run"] = []  # starting or restoring: nothing to follow yet
            itm.poll_once(self.store, {}, config)
            self.assertEqual(self.task("task-run")["projects"], [self.api, self.web])
            found["/sock/run"] = self.workspaces(self.folder("code/api"))
            itm.poll_once(self.store, {}, config)
            self.assertEqual(self.task("task-run")["projects"], [self.api])
            # Off: nothing is asked or changed.
            found["/sock/run"] = self.workspaces(self.folder("code/web"))
            off = itm.parse_config("[herdr]\nprojects_from_workspaces = false\n")
            itm.poll_once(self.store, {}, off)
            self.assertEqual(self.task("task-run")["projects"], [self.api])
            # An archived task is left alone.
            with self.store.edit() as data:
                data["tasks"]["task-run"]["state"] = "archived"
            itm.poll_once(self.store, {}, config)
            self.assertEqual(self.task("task-run")["projects"], [self.api])

    def test_session_workspaces_reads_the_first_pane_of_the_first_tab(self):
        snapshot = {"result": {"snapshot": {
            "workspaces": [{"workspace_id": "w1", "label": "api", "focused": False},
                           {"workspace_id": "w2", "label": "web", "focused": True},
                           {"workspace_id": "w3", "label": "empty"}],
            # Tab order, not numbers: w1's second tab was moved to the front.
            "tabs": [{"tab_id": "w1:t3", "workspace_id": "w1"}, {"tab_id": "w1:t1", "workspace_id": "w1"},
                     {"tab_id": "w2:t1", "workspace_id": "w2"}],
            "layouts": [{"tab_id": "w1:t1", "panes": [{"pane_id": "w1:p1"}]},
                        {"tab_id": "w1:t3", "panes": [{"pane_id": "w1:p5"}, {"pane_id": "w1:p2"}]},
                        {"tab_id": "w2:t1", "panes": [{"pane_id": "w2:p1"}]}],
            "panes": [{"pane_id": "w1:p1", "cwd": "/a"}, {"pane_id": "w1:p5", "cwd": "/b"},
                      {"pane_id": "w1:p2", "cwd": "/c"}, {"pane_id": "w2:p1", "cwd": "/d"}],
        }}}
        with mock.patch.object(itm, "herdr_json", return_value=snapshot) as herdr:
            found = itm.session_workspaces("/sock")
        herdr.assert_called_once_with(["api", "snapshot"], "/sock")
        self.assertEqual(found, [{"id": "w1", "label": "api", "focused": False, "folder": "/b"},
                                 {"id": "w2", "label": "web", "focused": True, "folder": "/d"},
                                 {"id": "w3", "label": "empty", "focused": False, "folder": None}])
        with mock.patch.object(itm, "herdr_json", side_effect=itm.ItmError("gone")):
            self.assertIsNone(itm.session_workspaces("/sock"))
        with mock.patch.object(itm, "herdr_json", return_value={"result": {}}):
            self.assertIsNone(itm.session_workspaces("/sock"))


class RepositoryMatchTest(TaskEnvTest):
    """A workspace outside every saved folder, in the same git repository as a saved project."""

    def setUp(self):
        super().setUp()
        itm._repositories.clear()
        self.addCleanup(itm._repositories.clear)

    def git(self, *args, cwd=None):
        subprocess.run(["git", *args], cwd=cwd, check=True, capture_output=True,
                       env={**os.environ, "GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "t@t",
                            "GIT_COMMITTER_NAME": "t", "GIT_COMMITTER_EMAIL": "t@t"})

    def checkout(self, name, origin=None):
        path = self.folder(name)
        self.git("init", "-q", str(path))
        self.git("commit", "-q", "--allow-empty", "-m", "init", cwd=path)
        if origin:
            self.git("remote", "add", "origin", origin, cwd=path)
        return path

    def workspace(self, folder, label=""):
        return {"id": "w1", "label": label, "focused": True, "folder": str(folder)}

    def test_a_linked_worktree_is_in_the_project_of_its_checkout(self):
        api = self.checkout("code/api")
        api_id = itm.add_project("dir", str(api), "api")
        self.git("worktree", "add", "-q", str(self.root / "worktrees/api-feature"), "-b", "feature", cwd=api)
        projects = self.store.load()["projects"]
        tree = self.root / "worktrees/api-feature"
        self.assertIsNone(itm.workspace_project(projects, self.workspace(tree)))  # by folder: in none
        self.assertEqual(itm.workspace_project(projects, self.workspace(tree), repositories=True), api_id)
        self.assertEqual(itm.repository(str(tree)), (str(api), None))

    def test_another_checkout_is_in_the_project_with_its_remote(self):
        clone = self.checkout("clones/api", origin="git@github.com:acme/api.git")
        with self.store.edit() as data:
            data["projects"]["git1"] = {"name": "api", "kind": "git", "url": "git@github.com:acme/api.git",
                                        "path": str(clone)}
        mine = self.checkout("mine/api", origin="https://github.com/acme/api")  # another form of the URL
        (mine / "src").mkdir()
        projects = self.store.load()["projects"]
        self.assertEqual(itm.workspace_project(projects, self.workspace(mine / "src"), repositories=True), "git1")
        other = self.checkout("mine/web", origin="https://github.com/acme/web")
        self.assertIsNone(itm.workspace_project(projects, self.workspace(other), repositories=True))
        self.assertIsNone(itm.workspace_project(projects, self.workspace(self.folder("plain")), repositories=True))
        # Two saved projects with that remote: in neither.
        itm.add_project("dir", str(self.checkout("work/api", origin="git@github.com:acme/api")), "work-api")
        projects = self.store.load()["projects"]
        self.assertIsNone(itm.workspace_project(projects, self.workspace(mine), repositories=True))

    def test_lookups_are_cached(self):
        api = self.checkout("code/api", origin="git@github.com:acme/api.git")
        real_run = subprocess.run
        with mock.patch.object(itm.subprocess, "run", side_effect=real_run) as run:
            self.assertEqual(itm.repository(str(api)), (str(api), "github.com/acme/api"))
            calls = run.call_count
            itm.repository(str(api))
            self.assertEqual(run.call_count, calls)
        self.assertIsNone(itm.repository(str(self.folder("plain"))))
        with mock.patch.object(itm.subprocess, "run", side_effect=FileNotFoundError("git")):
            self.assertIsNone(itm.repository(str(self.folder("no-git-here"))))  # git missing: no repository

    def test_herdr_labels_are_not_names(self):
        # Herdr names an unnamed workspace after its folder, or after the repository it was opened in.
        self.assertTrue(itm.automatic_label("sub", "/code/api/sub"))
        self.assertTrue(itm.automatic_label("api", "/code/api/sub"))
        self.assertFalse(itm.automatic_label("my api", "/code/api/sub"))
        api_id = itm.add_project("dir", str(self.folder("code/api")), "api")
        other = self.checkout("other/api")  # another repository with that name
        (other / "src").mkdir()
        projects = self.store.load()["projects"]
        self.assertIsNone(itm.workspace_project(projects, self.workspace(other / "src", "api"), repositories=True))
        named = self.workspace(self.folder("elsewhere"), "API")
        self.assertEqual(itm.workspace_project(projects, named), api_id)

    def test_the_daemon_and_the_hotkey_use_it(self):
        api = self.checkout("code/api")
        api_id = itm.add_project("dir", str(api), "api")
        self.git("worktree", "add", "-q", str(self.root / "worktrees/fix"), "-b", "fix", cwd=api)
        itm.cmd_new("Run", switch=False)
        sessions = {"task-run": {"running": True, "socket_path": "/sock/run"}}
        found = [self.workspace(self.root / "worktrees/fix", "fix")]
        with mock.patch.object(itm, "list_sessions", return_value=sessions), \
                mock.patch.object(itm, "list_agents", return_value=[]), \
                mock.patch.object(itm, "live_clients", return_value={}), \
                mock.patch.object(itm, "session_workspaces", return_value=found):
            itm.poll_once(self.store, {}, itm.parse_config(""))
            self.assertEqual(self.task("task-run")["projects"], [api_id])
            with mock.patch.dict(os.environ, {"HERDR_SOCKET_PATH": "/sock/run"}), \
                    mock.patch.object(itm, "herdr_json", return_value={}) as herdr:
                itm.cmd_project_add_workspace(None)  # the worktree's project, not a new one
        self.assertEqual(list(self.store.load()["projects"]), [api_id])
        herdr.assert_called_once_with(["workspace", "rename", "w1", "api"], "/sock/run")


class AddWorkspaceTest(TaskEnvTest):
    """`itm project add-workspace` (prefix+shift+a)."""

    def setUp(self):
        super().setUp()
        self.code = self.folder("code/new-thing")
        itm.cmd_new("Here", switch=False)
        self.sessions = {"task-here": {"running": True, "socket_path": "/sock/here"}}
        self.found = [{"id": "w1", "label": "other", "focused": False, "folder": str(self.root)},
                      {"id": "w2", "label": "My Thing", "focused": True, "folder": str(self.code)}]
        self.calls = []
        self.extra = [
            mock.patch.dict(os.environ, {"HERDR_SOCKET_PATH": "/sock/here"}),
            mock.patch.object(itm, "list_sessions", return_value=self.sessions),
            mock.patch.object(itm, "session_workspaces", side_effect=lambda socket: self.found),
            mock.patch.object(itm, "herdr_json", side_effect=lambda args, socket=None: self.calls.append(args) or {}),
        ]
        for patch in self.extra:
            patch.start()
        os.environ.pop("HERDR_WORKSPACE_ID", None)
        os.environ.pop("HERDR_ACTIVE_WORKSPACE_ID", None)

    def tearDown(self):
        for patch in reversed(self.extra):
            patch.stop()
        super().tearDown()

    def test_saves_the_focused_workspace_named_like_it_and_gives_it_to_the_task(self):
        with mock.patch("sys.stdin") as stdin:
            stdin.isatty.return_value = False  # no terminal: no dialog
            itm.cmd_project_add_workspace(None)
        project = self.project("My Thing")
        self.assertEqual(project["path"], str(self.code))
        self.assertEqual(self.task("task-here")["projects"], [project["id"]])
        # Projects follow workspaces: it is one found there, and goes when the workspace closes.
        self.assertEqual(self.task("task-here")["found_projects"], [project["id"]])
        self.assertEqual(self.calls, [["workspace", "rename", "w2", "My Thing"]])  # no new workspace

    def test_without_following_workspaces_it_is_added_by_hand(self):
        itm.set_config("herdr", "projects_from_workspaces", False)
        itm.cmd_project_add_workspace("mine")
        self.assertEqual(self.task("task-here")["projects"], [self.project("mine")["id"]])
        self.assertNotIn("found_projects", self.task("task-here"))

    def test_a_stale_pane_workspace_is_not_trusted_and_a_name_can_be_given(self):
        # A pane keeps HERDR_WORKSPACE_ID from before it moved; the focused workspace is the one typed in.
        with mock.patch.dict(os.environ, {"HERDR_WORKSPACE_ID": "w1"}):
            itm.cmd_project_add_workspace("chosen")
        self.assertEqual(self.project("chosen")["path"], str(self.code))
        self.assertEqual(self.calls, [["workspace", "rename", "w2", "chosen"]])

    def test_home_and_folders_above_it_are_refused(self):
        home = itm.task_dir()
        home.mkdir(parents=True, exist_ok=True)
        for folder in (home, self.root, Path("/")):
            with self.subTest(folder=folder), mock.patch.dict(os.environ, {"HOME": str(home)}):
                self.found[1]["folder"] = str(folder)
                with self.assertRaisesRegex(itm.ItmError, "holds every workspace under it"):
                    itm.cmd_project_add_workspace("everything")
        self.assertEqual(self.store.load()["projects"], {})

    def test_a_popup_names_the_workspace_it_was_opened_over(self):
        self.code = self.folder("elsewhere")
        self.found[0]["folder"] = str(self.code)
        with mock.patch.dict(os.environ, {"HERDR_ACTIVE_WORKSPACE_ID": "w1"}):
            itm.cmd_project_add_workspace("popup")
        self.assertEqual(self.project("popup")["path"], str(self.code))
        self.assertEqual(self.calls, [["workspace", "rename", "w1", "popup"]])

    def test_a_saved_folder_is_not_added_twice(self):
        api_id = itm.add_project("dir", str(self.code.parent), "code")
        itm.cmd_project_add_workspace(None)  # a folder inside a saved project is that project
        self.assertEqual(list(self.store.load()["projects"]), [api_id])
        self.assertEqual(self.task("task-here")["projects"], [api_id])
        self.assertEqual(self.calls, [["workspace", "rename", "w2", "code"]])

    def test_the_dialog_gets_the_folder_and_name(self):
        screens = mock.Mock()
        screens.add_project_dialog.return_value = None
        with mock.patch.object(itm, "screens", return_value=screens), mock.patch("sys.stdin") as stdin, \
                mock.patch("sys.stdout") as stdout, self.assertRaises(itm.Cancelled):
            stdin.isatty.return_value = stdout.isatty.return_value = True
            itm.cmd_project_add_workspace(None)
        screens.add_project_dialog.assert_called_once_with("dir", str(self.code), "My Thing")
        self.assertEqual(self.store.load()["projects"], {})

    def test_outside_herdr_or_a_task(self):
        with mock.patch.dict(os.environ, {"HERDR_SOCKET_PATH": ""}), \
                self.assertRaisesRegex(itm.ItmError, "run this inside Herdr"):
            itm.cmd_project_add_workspace("x")
        self.sessions["work"] = self.sessions.pop("task-here")
        self.sessions["work"]["socket_path"] = "/sock/here"
        itm.cmd_project_add_workspace("x")  # saved, and no task to give it to
        self.assertEqual(self.project("x")["path"], str(self.code))


class SettingsTest(TaskEnvTest):
    def test_defaults_without_a_file(self):
        self.assertEqual(itm.load_config(), itm.CONFIG_DEFAULTS)

    def test_set_keeps_the_rest_of_the_file(self):
        path = itm.config_path()
        path.parent.mkdir(parents=True)
        path.write_text("# mine\n[herdr]\n# why\nprojects_from_workspaces = true  # keep\n")
        itm.set_config("herdr", "projects_from_workspaces", False)
        itm.set_config("herdr", "remove_projects_without_workspace", False)
        self.assertEqual(path.read_text(), "# mine\n[herdr]\n# why\nprojects_from_workspaces = false  # keep\n"
                                           "remove_projects_without_workspace = false\n")
        self.assertFalse(itm.load_config()["herdr"]["remove_projects_without_workspace"])

    def test_set_creates_the_file(self):
        itm.set_config("herdr", "remove_projects_without_workspace", False)
        self.assertEqual(itm.config_path().read_text(), "[herdr]\nremove_projects_without_workspace = false\n")

    def test_mistakes_are_errors(self):
        for text, error in (("[herdr\n", "not valid TOML"), ("[herdr]\nnope = true\n", "unknown setting herdr.nope"),
                            ("[other]\n", "unknown setting other"),
                            ("[herdr]\nprojects_from_workspaces = 1\n", "must be true or false")):
            with self.subTest(text=text), self.assertRaisesRegex(itm.ItmError, error):
                itm.parse_config(text)

    def test_a_file_it_cannot_change_is_left_alone(self):
        path = itm.config_path()
        path.parent.mkdir(parents=True)
        path.write_text("herdr.projects_from_workspaces = true\n")
        with self.assertRaises(itm.ItmError):
            itm.set_config("herdr", "projects_from_workspaces", False)
        self.assertEqual(path.read_text(), "herdr.projects_from_workspaces = true\n")

    def test_odd_files(self):
        path = itm.config_path()
        path.parent.mkdir(parents=True)
        path.write_bytes(b"# n\xe3o\n")  # Latin-1
        with self.assertRaisesRegex(itm.ItmError, "cannot read"):
            itm.load_config()
        path.write_text('[herdr]\n"projects_from_workspaces" = true\n')  # valid, but not a line it follows
        with self.assertRaisesRegex(itm.ItmError, "edit it by hand"):
            itm.set_config("herdr", "projects_from_workspaces", False)
        # Through a symlink (home-manager, stow): the target changes, the link stays.
        target = self.root / "dotfiles/itm.toml"
        target.parent.mkdir()
        target.write_text("[herdr]\nprojects_from_workspaces = true\n")
        path.unlink()
        path.symlink_to(target)
        itm.set_config("herdr", "projects_from_workspaces", False)
        self.assertTrue(path.is_symlink())
        self.assertIn("projects_from_workspaces = false", target.read_text())
        target.parent.chmod(0o555)
        try:
            if not os.access(target.parent, os.W_OK):  # not as root
                with self.assertRaisesRegex(itm.ItmError, "cannot write"):
                    itm.set_config("herdr", "projects_from_workspaces", True)
        finally:
            target.parent.chmod(0o755)

    def test_the_daemon_goes_on_with_a_broken_file(self):
        path = itm.config_path()
        path.parent.mkdir(parents=True)
        path.write_text("[herdr\n")
        passes = []

        def poll(store, notified, config, settled):
            passes.append(config)
            if len(passes) == 2:
                raise SystemExit
        with mock.patch.object(itm, "poll_once", side_effect=poll), mock.patch.object(itm.time, "sleep"), \
                mock.patch.object(itm.signal, "signal"), mock.patch("builtins.print") as out, self.assertRaises(SystemExit):
            itm.cmd_daemon()
        self.assertFalse(passes[0]["herdr"]["projects_from_workspaces"])
        logged = "".join(str(c.args[0]) for c in out.call_args_list)
        self.assertEqual(logged.count("settings ignored"), 1)  # once, not every pass


class JumpLastStatusTest(TaskEnvTest):
    def write_status(self, sessions):
        itm.write_atomic(itm.status_path(), itm.json.dumps({"updated_at": itm.now(), "sessions": sessions}))

    def test_jump_opens_the_longest_wait(self):
        for title in ("A", "B"):
            itm.cmd_new(title, switch=False)
        blocked = lambda since: {"blocked": 1, "agents": [{"status": "blocked", "since": since}]}
        self.write_status({"task-a": blocked(200), "task-b": blocked(100)})
        itm.cmd_jump()
        self.switch_to.assert_called_once_with("task-b")
        self.write_status({})
        with self.assertRaisesRegex(itm.ItmError, "no agent is waiting"):
            itm.cmd_jump()
        itm.status_path().unlink()
        with self.assertRaisesRegex(itm.ItmError, "daemon is not running"):
            itm.cmd_jump()

    def test_last_goes_back(self):
        itm.cmd_new("A", switch=False)
        with mock.patch.object(itm, "now", return_value=itm.now() + 60):
            itm.cmd_new("B", switch=False)
        with mock.patch.dict(os.environ, {"HERDR_SOCKET_PATH": "/sock"}), \
                mock.patch.object(itm, "current_session", return_value="task-b"):
            itm.cmd_last()
        self.switch_to.assert_called_once_with("task-a")

    def test_status(self):
        itm.cmd_new("A", switch=False)
        itm.cmd_new("B", snooze="1h")
        self.write_status({"task-a": {"blocked": 1, "agents": []}})
        with mock.patch("builtins.print") as out:
            itm.cmd_status(itm.STATUS_FORMAT, as_json=False)
            itm.cmd_status("{waiting}/{active}", as_json=False)
            itm.status_path().unlink()
            itm.cmd_status(itm.STATUS_FORMAT, as_json=False)
        self.assertEqual([c.args[0] for c in out.call_args_list], ["⚑ 1 ● 1 ◔ 1", "1/1", "⚑ ? ● 1 ◔ 1"])
        with self.assertRaisesRegex(itm.ItmError, "bad --format"):
            itm.cmd_status("{nope}", as_json=False)


class ReviewFixesTest(TaskEnvTest):
    """TUI regressions."""

    def test_half_typed_home_of_another_user_is_not_a_crash(self):
        with self.assertRaisesRegex(itm.ItmError, "not a folder"):
            itm.project_spec("dir", "~no-such-user-x/code")

    def test_snooze_times_must_be_in_the_future(self):
        task = itm.new_task("A")
        with self.assertRaisesRegex(itm.ItmError, "in the future"):
            itm.apply_snooze(task, itm.now() - 5)  # a preset that passed while a dialog was open
        self.assertEqual(task["state"], "active")
        with self.assertRaises(itm.ItmError):
            itm.create_task("Late", until=itm.now() - 5)
        self.assertEqual(self.store.load()["tasks"], {})

    def test_until_an_agent_needs_a_way_to_wake(self):
        with self.assertRaisesRegex(itm.ItmError, "new task has no agents"):
            itm.cmd_new("Asked", snooze="agent")
        with self.assertRaisesRegex(itm.ItmError, "new task has no agents"):
            itm.create_task("Asked", until=itm.UNTIL_AGENT)
        itm.cmd_new("Run", switch=False)
        with self.assertRaisesRegex(itm.ItmError, "not running"):
            itm.snooze_task("task-run", itm.UNTIL_AGENT)
        running = {"task-run": {"running": True}}
        with mock.patch.object(itm, "list_sessions", return_value=running), \
                mock.patch.object(itm, "load_status", return_value={"task-run": {"blocked": 1}}), \
                self.assertRaisesRegex(itm.ItmError, "already waiting"):
            itm.snooze_task("task-run", itm.UNTIL_AGENT)
        with mock.patch.object(itm, "list_sessions", return_value=running), \
                mock.patch.object(itm, "load_status", return_value={}), \
                mock.patch.object(itm, "move_clients_off"):
            itm.snooze_task("task-run", itm.UNTIL_AGENT)
        self.assertEqual(self.task("task-run")["state"], "snoozed")

    def test_edit_with_the_current_project_after_start(self):
        api_id = itm.add_project("dir", str(self.folder("code/api")), "api")
        itm.cmd_new("Fix", projects=["api"], switch=False)
        with mock.patch.object(itm, "list_sessions", return_value={"task-fix": {"running": True}}):
            itm.edit_task("task-fix", title="Fix it", projects=["API"])  # unchanged projects: not an error
            itm.edit_task("task-fix", add=["API"])
        task = self.task("task-fix")
        self.assertEqual((task["title"], task["projects"]), ("Fix it", [api_id]))

    def test_form_starts_with_the_project_of_the_task_you_are_in(self):
        api_id = itm.add_project("dir", str(self.folder("code/api")), "api")
        itm.add_project("dir", str(self.folder("code/web")), "web")
        itm.cmd_new("Here", projects=["api", "web"], switch=False)
        screens = mock.Mock()
        screens.task_form.return_value = None
        with mock.patch.dict(os.environ, {"HERDR_SOCKET_PATH": "/sock"}), \
                mock.patch.object(itm, "current_session", return_value="task-here"), \
                mock.patch.object(itm, "switchable_session", return_value="task-here"), \
                mock.patch.object(itm, "screens", return_value=screens), self.assertRaises(itm.Cancelled):
            itm.cmd_new(None)
        self.assertEqual(screens.task_form.call_args.kwargs["projects"], [api_id])  # its first project

    def test_remove_without_a_terminal_keeps_a_clean_clone(self):
        self.add_api()
        with mock.patch.object(itm, "is_checkout_of", return_value=True), \
                mock.patch.object(itm, "local_work", return_value=None), \
                mock.patch("sys.stdin") as stdin, mock.patch("builtins.print") as out, \
                mock.patch.object(itm, "screens") as screens:
            stdin.isatty.return_value = False
            itm.cmd_project_remove("api", delete_clone=None)
        screens.assert_not_called()
        self.assertTrue(self.api_clone.is_dir())
        self.assertEqual(self.store.load()["projects"], {})
        self.assertIn("no terminal to ask", " ".join(str(c.args[0]) for c in out.call_args_list))

    def test_without_a_question_nothing_is_deleted(self):
        # Local work seen when asking, gone by the time the lock is taken: still kept.
        self.add_api()
        work = iter(["stashes"])
        with mock.patch.object(itm, "is_checkout_of", return_value=True), \
                mock.patch.object(itm, "local_work", side_effect=lambda path: next(work, None)), \
                mock.patch("builtins.print"), mock.patch("sys.stderr"), mock.patch.object(itm, "screens") as screens:
            itm.cmd_project_remove("api", delete_clone=None)
        screens.assert_not_called()
        self.assertTrue(self.api_clone.is_dir())



if __name__ == "__main__":
    unittest.main()
