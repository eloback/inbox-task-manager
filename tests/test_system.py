import os
import shutil
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
from inbox_task_manager import system  # noqa: E402


def interpreter() -> str:
    """A Python that keeps the argv it was started with.

    On macOS a framework build (python.org, Homebrew) installs ``bin/python3``
    as a stub that starts the real interpreter in Python.app with its own
    argv[0]; that one keeps ours.
    """
    app = Path(sys.base_prefix, "Resources", "Python.app", "Contents", "MacOS", "Python")
    return str(app) if app.exists() else sys.executable


def process_tables():
    """The process-table readers this machine has: /proc (Linux), ps (BSD-style, not BusyBox's)."""
    tables = []
    if system.has_proc():
        tables.append(("proc", True))
    if shutil.which("ps") and subprocess.run(["ps", "-ww", "-A", "-o", "pid="], capture_output=True).returncode == 0:
        tables.append(("ps", False))
    return tables


class ProcessTableTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.children: list[subprocess.Popen] = []

    def tearDown(self):
        for child in self.children:
            child.kill()
            child.wait()
        self.tmp.cleanup()

    def spawn(self, argv: list[str]) -> subprocess.Popen:
        """A child that shows ``argv`` as its command line and waits.

        Python keeps the argv it was started with and runs the script named by
        argv[1] (a file of that name in the temp folder). Not ``sh``: a
        busybox ``sh`` (the Nix build sandbox's) picks its applet by argv[0].
        """
        Path(self.tmp.name, argv[1]).write_text("import time\ntime.sleep(30)\n")
        child = subprocess.Popen(argv, executable=interpreter(), cwd=self.tmp.name)
        self.children.append(child)
        return child

    def tables(self):
        tables = process_tables()
        if not tables:
            self.skipTest("no way to read the process table")
        return tables

    def test_reads_a_command_line(self):
        child = self.spawn(["/usr/local/bin/herdr", "session", "attach", "task-a"])
        for table, proc in self.tables():
            with self.subTest(table=table), mock.patch.object(system, "has_proc", return_value=proc):
                self.assertEqual(system.process_argv(child.pid), ["/usr/local/bin/herdr", "session", "attach", "task-a"])

    def test_a_finished_process_has_none(self):
        child = subprocess.Popen([sys.executable, "-c", ""])
        child.wait()
        for table, proc in self.tables():
            with self.subTest(table=table), mock.patch.object(system, "has_proc", return_value=proc):
                self.assertIsNone(system.process_argv(child.pid))

    def test_finds_only_herdr_attach_children(self):
        attach = self.spawn(["herdr", "session", "attach", "task-a"])
        self.spawn(["herdr", "session", "list"])
        self.spawn(["other", "session", "attach", "task-a"])
        for table, proc in self.tables():
            with self.subTest(table=table), mock.patch.object(system, "has_proc", return_value=proc):
                self.assertEqual(itm.attach_children(os.getpid()), [attach.pid])
                self.assertEqual(itm.attach_children(attach.pid), [])


class ItmProcessTest(unittest.TestCase):
    def test_names_that_count_as_itm(self):
        cases = {
            ("/nix/store/abc-inbox-task-manager-0.2.0/bin/.inbox-task-manager-wrapped", "attach"): True,
            ("/usr/bin/python3", "-m", "inbox_task_manager", "attach"): True,
            ("/usr/bin/python3", "/home/me/.local/bin/itm", "attach"): True,
            ("/usr/bin/python3", "/home/me/.local/bin/inbox-task-manager", "attach"): True,
            ("/usr/bin/python3", "/home/me/bin/item", "attach"): False,
            ("vim", "notes.md"): False,
        }
        for argv, expected in cases.items():
            with self.subTest(argv=argv), mock.patch.object(system, "process_argv", return_value=list(argv)):
                self.assertIs(itm.is_itm_process(1234), expected)

    def test_a_gone_process_is_not_itm(self):
        with mock.patch.object(system, "process_argv", return_value=None):
            self.assertFalse(itm.is_itm_process(1234))

    def test_an_unreadable_live_client_is_kept_but_not_counted(self):
        with tempfile.TemporaryDirectory() as tmp, mock.patch.dict(os.environ, {"XDG_RUNTIME_DIR": tmp}):
            marker = itm.clients_dir() / "1234"
            marker.parent.mkdir(parents=True)
            marker.write_text("task-a\n")
            with mock.patch.object(system, "process_argv", return_value=None):
                with mock.patch.object(system, "pid_alive", return_value=True):  # ps failed
                    self.assertEqual(itm.live_clients(), {})
                self.assertTrue(marker.exists())
                with mock.patch.object(system, "pid_alive", return_value=False):  # gone
                    self.assertEqual(itm.live_clients(), {})
                self.assertFalse(marker.exists())


class RuntimeDirTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)

    def tearDown(self):
        self.tmp.cleanup()

    def test_xdg_runtime_dir_wins(self):
        with mock.patch.dict(os.environ, {"XDG_RUNTIME_DIR": str(self.root)}):
            self.assertEqual(itm.runtime_dir(), self.root / "inbox-task-manager")

    def test_private_folder_is_created_private(self):
        path = system._private_dir(self.root / "itm-1000")
        self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o700)

    def test_open_folder_is_made_private(self):
        path = self.root / "itm-1000"
        path.mkdir(mode=0o755)
        system._private_dir(path)
        self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o700)

    def test_link_is_refused(self):
        (self.root / "elsewhere").mkdir()
        (self.root / "itm-1000").symlink_to(self.root / "elsewhere")
        with self.assertRaises(PermissionError):
            system._private_dir(self.root / "itm-1000")

    def test_refusal_is_a_user_error(self):
        with (
            mock.patch.dict(os.environ, {"XDG_RUNTIME_DIR": ""}),
            mock.patch.object(system, "user_runtime_dir", side_effect=PermissionError("/tmp/x is not yours")),
            self.assertRaisesRegex(itm.ItmError, "XDG_RUNTIME_DIR"),
        ):
            itm.runtime_dir()


class NotifyCommandTest(unittest.TestCase):
    def setUp(self):
        self.env = mock.patch.dict(os.environ)
        self.env.start()
        os.environ.pop("ITM_NOTIFY_COMMAND", None)

    def tearDown(self):
        self.env.stop()

    def test_custom_command_gets_title_and_body(self):
        os.environ["ITM_NOTIFY_COMMAND"] = "ntfy publish 'my topic'"
        self.assertEqual(
            system.notify_command("T", "B", "itm", with_open=True),
            (["ntfy", "publish", "my topic", "T", "B"], False),
        )

    def test_notify_send_offers_open(self):
        with mock.patch.object(system, "is_macos", return_value=False), mock.patch.object(shutil, "which", return_value="/bin/x"):
            args, offers_open = system.notify_command("T", "B", "itm", with_open=True)
        self.assertEqual(args, ["notify-send", "--action=open=Open", "--wait", "--app-name=itm", "T", "B"])
        self.assertTrue(offers_open)

    def test_macos_uses_osascript_without_open(self):
        with mock.patch.object(system, "is_macos", return_value=True), mock.patch.object(shutil, "which", return_value="/usr/bin/osascript"):
            args, offers_open = system.notify_command('say "hi"', "B", "itm", with_open=True)
        self.assertEqual(args[0], "osascript")
        self.assertEqual(args[-2:], ['say "hi"', "B"])  # arguments, never script text
        self.assertFalse(offers_open)

    def test_blank_custom_command_is_ignored(self):
        os.environ["ITM_NOTIFY_COMMAND"] = "  \t"
        with mock.patch.object(system, "is_macos", return_value=False), mock.patch.object(shutil, "which", return_value="/bin/x"):
            args, _ = system.notify_command("T", "B", "itm", with_open=False)
        self.assertEqual(args[0], "notify-send")

    def test_nothing_to_notify_with(self):
        with mock.patch.object(shutil, "which", return_value=None):
            self.assertIsNone(system.notify_command("T", "B", "itm", with_open=False))

    def test_daemon_survives_a_broken_command(self):
        os.environ["ITM_NOTIFY_COMMAND"] = "ntfy 'unclosed"
        with mock.patch.object(itm, "live_clients", return_value={}), mock.patch("sys.stderr") as stderr:
            itm.desktop_notify("T", "B", "task-a")
        self.assertIn("ITM_NOTIFY_COMMAND", "".join(c.args[0] for c in stderr.write.call_args_list))

    def test_daemon_survives_without_a_notifier(self):
        with (
            mock.patch.object(system, "notify_command", return_value=None),
            mock.patch.object(itm, "live_clients", return_value={}),
            mock.patch("sys.stderr") as stderr,
        ):
            itm.desktop_notify("T", "B", "task-a")
        self.assertIn("ITM_NOTIFY_COMMAND", "".join(c.args[0] for c in stderr.write.call_args_list))


class MissingProgramTest(unittest.TestCase):
    """pip installs bring no herdr or git: say so instead of a traceback."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.env = mock.patch.dict(os.environ, {"PATH": self.tmp.name, "XDG_STATE_HOME": self.tmp.name})
        self.env.start()

    def tearDown(self):
        self.env.stop()
        self.tmp.cleanup()

    def test_herdr(self):
        with self.assertRaisesRegex(itm.ItmError, "herdr not found"):
            itm.list_sessions()

    def test_attach_saves_no_task_without_herdr(self):
        with self.assertRaisesRegex(itm.ItmError, "herdr not found"):
            itm.attach_loop("task-x")
        self.assertEqual(itm.Store().load()["tasks"], {})

    def test_daemon_logs_a_repeated_error_once(self):
        sleeps = []

        def sleep(_seconds):
            sleeps.append(1)
            if len(sleeps) == 3:
                raise KeyboardInterrupt

        with (
            mock.patch.object(itm, "poll_once", side_effect=itm.ItmError("herdr not found")),
            mock.patch.object(itm.time, "sleep", side_effect=sleep),
            mock.patch.object(itm.signal, "signal"),
            mock.patch("sys.stderr") as stderr,
            self.assertRaises(KeyboardInterrupt),
        ):
            itm.cmd_daemon()
        self.assertEqual(sum("poll failed" in c.args[0] for c in stderr.write.call_args_list), 1)

    def test_git(self):
        with self.assertRaisesRegex(itm.ItmError, "git not found"):
            itm.local_work(Path(self.tmp.name))

    def test_other_missing_files_are_not_hidden(self):
        with self.assertRaises(FileNotFoundError), itm.needs("herdr"):
            open(Path(self.tmp.name) / "nothing")


class DayAndClockTest(unittest.TestCase):
    """Snooze phrases itm reads itself, so they work without GNU date (macOS)."""

    base = int(datetime(2026, 11, 4, 10, 0).timestamp())  # a Wednesday

    def at(self, *args) -> int:
        return int(datetime(*args).timestamp())

    def test_phrases(self):
        cases = {
            "tomorrow 9:00": self.at(2026, 11, 5, 9, 0),
            "tomorrow": self.at(2026, 11, 5, 10, 0),
            "Tomorrow at 2pm": self.at(2026, 11, 5, 14, 0),
            "friday 2pm": self.at(2026, 11, 6, 14, 0),
            "wed 18:00": self.at(2026, 11, 4, 18, 0),
            "next wed": self.at(2026, 11, 11, 0, 0),
            "next friday 9am": self.at(2026, 11, 6, 9, 0),
            "thurs 8:15": self.at(2026, 11, 5, 8, 15),
            "today 17:00": self.at(2026, 11, 4, 17, 0),
            "2026-12-01 14:30": self.at(2026, 12, 1, 14, 30),
            "2026-12-01": self.at(2026, 12, 1, 0, 0),
            "5pm": self.at(2026, 11, 4, 17, 0),
            "at 9:30": self.at(2026, 11, 5, 9, 30),
            "12am": self.at(2026, 11, 5, 0, 0),
            "12pm": self.at(2026, 11, 4, 12, 0),
        }
        with mock.patch.object(system, "gnu_date", return_value=None):
            for text, expected in cases.items():
                with self.subTest(text=text):
                    self.assertEqual(itm.parse_when(text, self.base), expected)

    def test_invalid_phrases(self):
        with mock.patch.object(system, "gnu_date", return_value=None):
            for text in ("tomorrow 25:00", "friday 13pm", "0am", "2026-13-01", "0001-01-01", "9999-12-31 23:59", "monkey", "sundae 9:00", "wednesday", "next week"):
                with self.subTest(text=text), self.assertRaises(itm.ItmError):
                    itm.parse_when(text, self.base)

    @unittest.skipUnless(system.gnu_date(), "needs GNU date")
    def test_other_phrases_go_to_gnu_date(self):
        self.assertGreater(itm.parse_when("next week"), itm.now() + 6 * 86400)


if __name__ == "__main__":
    unittest.main()
