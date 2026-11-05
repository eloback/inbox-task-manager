"""Task notes and folder completion (core, standard library only)."""

import json
import os
import sys
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

import inbox_task_manager as itm  # noqa: E402
from test_inbox_task_manager import ProjectEnvTest, TaskEnvTest  # noqa: E402  (fixtures only, no tests)


class NotesTest(TaskEnvTest):
    def setUp(self):
        super().setUp()
        # Archive and delete stop sessions and hand clients off: never against the real Herdr.
        for patch in (mock.patch.object(itm, "stop_session_detached"), mock.patch.object(itm, "handoff", return_value=False)):
            patch.start()
            self.addCleanup(patch.stop)

    def notes(self, name):
        return itm.notes_path(name).read_text()

    def test_a_new_task_starts_its_notes_with_title_and_description(self):
        itm.add_project("dir", str(self.folder("code/api")), "api")
        itm.cmd_new("Fix login", description="Safari logs out\nafter 5 minutes", project="api")
        path = itm.notes_path("task-fix-login")
        self.assertEqual(path, self.root / "data/inbox-task-manager/tasks/task-fix-login/notes.md")
        text = self.notes("task-fix-login")
        self.assertTrue(text.startswith("# Fix login\n\n*Created "), text)
        self.assertIn("· project api · task-fix-login*\n\nSafari logs out\nafter 5 minutes\n", text)
        itm.cmd_new("Bare")
        self.assertIn("· no project · task-bare*\n", self.notes("task-bare"))
        self.assertTrue(self.notes("task-bare").endswith("task-bare*\n"))

    def test_edits_append_only_what_changed(self):
        itm.add_project("dir", str(self.folder("code/api")), "api")
        itm.cmd_new("Fix login", description="old text", switch=False)
        before = self.notes("task-fix-login")
        itm.edit_task("task-fix-login", title=" Fix login ", description="old text ")  # nothing really changed
        self.assertEqual(self.notes("task-fix-login"), before)
        itm.edit_task("task-fix-login", title="Fix login on Safari", description="ITP drops it")
        itm.edit_task("task-fix-login", project="api")
        itm.edit_task("task-fix-login", description="", project=None)
        added = self.notes("task-fix-login")[len(before):]
        stamp = itm.notes_stamp()[:10]
        self.assertRegex(added, (
            rf"^\n## {stamp} \d\d:\d\d · Title changed\n\nFix login on Safari\n\n"
            rf"## {stamp} \d\d:\d\d · Description changed\n\nITP drops it\n\n"
            rf"## {stamp} \d\d:\d\d · Project changed\n\napi \(.*code/api\)\n\n"
            rf"## {stamp} \d\d:\d\d · Description cleared\n\n"
            rf"## {stamp} \d\d:\d\d · Project removed\n\n$"))

    def test_appending_keeps_one_blank_line_after_what_the_user_wrote(self):
        itm.cmd_new("Fix", switch=False)
        path = itm.notes_path("task-fix")
        path.write_text("# Fix\n\nmy note without a newline")
        itm.edit_task("task-fix", title="Fixed")
        self.assertIn("my note without a newline\n\n## ", path.read_text())

    def test_a_task_without_notes_gets_them_from_before_the_edit(self):
        with self.store.edit() as data:  # from before notes existed, or adopted by the daemon
            data["tasks"]["task-old"] = itm.new_task("Old title", description="old text")
        itm.edit_task("task-old", title="New title")
        text = self.notes("task-old")
        self.assertTrue(text.startswith("# Old title\n"), text)
        self.assertIn("old text\n\n## ", text)
        self.assertIn("Title changed\n\nNew title\n", text)

    def test_failing_to_write_notes_keeps_the_edit(self):
        itm.cmd_new("Fix", switch=False)
        with mock.patch.object(itm, "append_notes", side_effect=OSError("disk full")):
            messages = itm.edit_task("task-fix", title="Fixed")
        self.assertEqual(self.task("task-fix")["title"], "Fixed")
        self.assertEqual(messages, ["could not update its notes: disk full"])
        with mock.patch.object(itm, "ensure_notes", side_effect=OSError("read-only")):
            itm.cmd_new("Still saved", switch=False)
        self.assertIn("task-still-saved", self.store.load()["tasks"])

    def test_delete_removes_the_notes_and_keeps_other_files(self):
        itm.cmd_new("Gone", switch=False)
        itm.cmd_new("Kept", switch=False)
        (itm.notes_path("task-kept").parent / "screenshot.png").write_bytes(b"png")
        itm.delete_task("task-gone")
        itm.delete_task("task-kept")
        self.assertFalse(itm.notes_path("task-gone").parent.exists())
        self.assertFalse(itm.notes_path("task-kept").exists())
        self.assertTrue((itm.notes_path("task-kept").parent / "screenshot.png").exists())
        with self.store.edit() as data:  # its session is gone, so the tombstone expires
            data["deleted"].clear()
        itm.cmd_new("Gone", switch=False)  # the same session name again: fresh notes
        self.assertTrue(self.notes("task-gone").startswith("# Gone\n"))

    def test_archive_and_reopen_keep_the_notes(self):
        itm.cmd_new("Keep me", switch=False)
        itm.notes_path("task-keep-me").write_text("# Keep me\n\ncontext\n")
        itm.archive_task("task-keep-me")
        itm.cmd_reopen("task-keep-me")
        self.assertEqual(self.notes("task-keep-me"), "# Keep me\n\ncontext\n")

    def test_a_leftover_notes_folder_is_never_inherited(self):
        folder = itm.notes_path("task-fix").parent
        folder.mkdir(parents=True)
        (folder / ".notes.md.swp").write_text("an editor's swap file")
        itm.cmd_new("Fix", switch=False)
        self.assertIn("task-fix-2", self.store.load()["tasks"])

    def test_the_heading_is_one_line_and_existing_notes_are_kept(self):
        with self.store.edit() as data:
            data["tasks"]["task-x"] = itm.new_task("two\nlines")
        path = itm.ensure_notes("task-x", self.task("task-x"), {})
        self.assertTrue(path.read_text().startswith("# two lines\n"))
        path.write_text("mine")
        itm.ensure_notes("task-x", self.task("task-x"), {})
        self.assertEqual(path.read_text(), "mine")
        self.assertEqual([p.name for p in path.parent.iterdir()], ["notes.md"])  # no temp file left

    def test_notes_start_without_hard_links(self):
        with self.store.edit() as data:
            data["tasks"]["task-x"] = itm.new_task("On sshfs")
        with mock.patch.object(itm.os, "link", side_effect=PermissionError("no hard links here")):
            path = itm.ensure_notes("task-x", self.task("task-x"), {})
            self.assertTrue(path.read_text().startswith("# On sshfs\n"))
            path.write_text("mine")
            itm.ensure_notes("task-x", self.task("task-x"), {})
        self.assertEqual(path.read_text(), "mine")

    def test_session_names_that_are_not_safe_paths(self):
        for name in ("", ".", "..", "task-a/../../x", "task-\0"):
            with self.subTest(name=name), self.assertRaises(itm.ItmError):
                itm.notes_path(name)

    def test_tail(self):
        self.assertIsNone(itm.notes_tail("task-none"))
        itm.cmd_new("Fix", switch=False)
        itm.notes_path("task-fix").write_text("# Fix\n\none\n\ntwo\nthree\n\n")
        mtime, lines = itm.notes_tail("task-fix")
        self.assertEqual(lines, ["one", "two", "three"])
        self.assertAlmostEqual(mtime, itm.now(), delta=5)

    def test_list_json_has_the_notes_path(self):
        itm.cmd_new("Fix", switch=False)
        with mock.patch("builtins.print") as out:
            itm.cmd_list("all", as_json=True)
        self.assertEqual(json.loads(out.call_args[0][0])[0]["notes"], str(itm.notes_path("task-fix")))


class NotesCommandTest(TaskEnvTest):
    def run_notes(self, *args, tty=True):
        with mock.patch.object(itm.sys.stdin, "isatty", return_value=tty), \
                mock.patch.object(itm.sys.stdout, "isatty", return_value=tty), \
                mock.patch.object(itm.subprocess, "run") as run:
            code = itm.main(["notes", *args])
        return code, run

    def test_opens_the_named_task_in_the_editor(self):
        itm.cmd_new("Fix login", switch=False)
        with mock.patch.dict(os.environ, {"VISUAL": "", "EDITOR": "code -w", "ITM_POPUP": "1"}):
            code, run = self.run_notes("Fix login")
        self.assertEqual(code, 0)
        self.assertEqual(run.call_args.args[0], ["code", "-w", str(itm.notes_path("task-fix-login"))])
        self.assertNotIn("ITM_POPUP", run.call_args.kwargs["env"])  # itm run inside the editor shows errors at once

    def test_the_current_task_and_a_missing_file(self):
        with self.store.edit() as data:
            data["tasks"]["task-here"] = itm.new_task("Here")
        sessions = {"task-here": {"socket_path": "/sock/here", "running": True}}
        with mock.patch.object(itm, "list_sessions", return_value=sessions), \
                mock.patch.dict(os.environ, {"HERDR_SOCKET_PATH": "/sock/here", "VISUAL": "vi"}):
            code, run = self.run_notes()
        self.assertEqual(run.call_args.args[0], ["vi", str(itm.notes_path("task-here"))])
        self.assertTrue(itm.notes_path("task-here").read_text().startswith("# Here\n"))

    def test_outside_a_task_names_the_switcher(self):
        with mock.patch("builtins.print") as out:
            code, run = self.run_notes()
        self.assertEqual(code, 1)
        run.assert_not_called()
        self.assertIn("ctrl+o", out.call_args.args[0])

    def test_herdr_failing_is_said_as_such(self):
        with mock.patch.object(itm, "list_sessions", side_effect=itm.ItmError("herdr session list --json timed out")), \
                mock.patch.dict(os.environ, {"HERDR_SOCKET_PATH": "/sock/x"}), mock.patch("builtins.print") as out:
            code, run = self.run_notes()
        self.assertEqual(code, 1)
        self.assertIn("timed out", out.call_args.args[0])

    def test_path_prints_without_an_editor_or_terminal(self):
        itm.cmd_new("Fix", switch=False)
        with mock.patch("builtins.print") as out:
            code, run = self.run_notes("Fix", "--path", tty=False)
        run.assert_not_called()
        out.assert_called_with(itm.notes_path("task-fix"))

    def test_append_adds_a_dated_note_for_agents(self):
        itm.cmd_new("Fix", switch=False)
        code, run = self.run_notes("Fix", "--append", "  tried the cookie flag  ", tty=False)
        self.assertEqual(code, 0)
        with mock.patch.object(itm.sys, "stdin") as stdin:
            stdin.read.return_value = "from stdin\n"
            stdin.isatty.return_value = False
            itm.main(["notes", "Fix", "--append", "-"])
        text = itm.notes_path("task-fix").read_text()
        self.assertRegex(text, r"\n\n## \d{4}-\d\d-\d\d \d\d:\d\d · Note\n\ntried the cookie flag\n\n## .* · Note\n\nfrom stdin\n\n$")
        with mock.patch("builtins.print") as out:
            self.assertEqual(itm.main(["notes", "Fix", "--append", " "]), 1)
        self.assertIn("empty", out.call_args.args[0])

    def test_needs_a_terminal(self):
        itm.cmd_new("Fix", switch=False)
        code, run = self.run_notes("Fix", tty=False)
        self.assertEqual(code, 1)
        run.assert_not_called()

    def test_editor_choice(self):
        path = Path("/n.md")
        with mock.patch.dict(os.environ, {"VISUAL": "nvim -u NONE", "EDITOR": "nano"}):
            self.assertEqual(itm.editor_command(path), ["nvim", "-u", "NONE", "/n.md"])
        with mock.patch.dict(os.environ, {"VISUAL": "", "EDITOR": ""}), \
                mock.patch.object(itm.shutil, "which", side_effect=lambda n: n == "vi"):
            self.assertEqual(itm.editor_command(path), ["vi", "/n.md"])
        with mock.patch.dict(os.environ, {"VISUAL": "", "EDITOR": ""}), \
                mock.patch.object(itm.shutil, "which", return_value=None), \
                self.assertRaisesRegex(itm.ItmError, "no editor found"):
            itm.editor_command(path)
        with mock.patch.dict(os.environ, {"VISUAL": "'unclosed"}), self.assertRaises(itm.ItmError):
            itm.editor_command(path)


class CompleteFoldersTest(ProjectEnvTest):
    def setUp(self):
        super().setUp()
        for name in ("code", "Config", "docs", ".hidden", "decode", "cxoxdxe"):
            self.folder(f"home/{name}")
        (self.root / "home/code.txt").write_text("a file")
        (self.root / "home/linked").symlink_to(self.root / "home/docs")
        self.home = mock.patch.dict(os.environ, {"HOME": str(self.root / "home")})
        self.home.start()

    def tearDown(self):
        self.home.stop()
        super().tearDown()

    def test_prefix_matches_come_first_and_keep_the_spelling(self):
        self.assertEqual(itm.complete_folders("~/co"), ["~/code/", "~/Config/", "~/decode/", "~/cxoxdxe/"])
        self.assertEqual(itm.complete_folders("~/cde"), ["~/code/", "~/cxoxdxe/", "~/decode/"])
        base = str(self.root / "home")
        self.assertEqual(itm.complete_folders(base + "/do"), [base + "/docs/", base + "/decode/"])

    def test_hidden_folders_symlinks_and_files(self):
        self.assertNotIn("~/.hidden/", itm.complete_folders("~/"))
        self.assertEqual(itm.complete_folders("~/.h"), ["~/.hidden/"])
        self.assertIn("~/linked/", itm.complete_folders("~/"))
        self.assertNotIn("~/code.txt/", itm.complete_folders("~/code"))

    def test_listings_resolve_symlinked_folders(self):
        real, names, links = itm.list_folders("~/")
        self.assertEqual(real, str(self.root / "home"))
        self.assertIn("linked", names)
        self.assertEqual(links, {"linked": str(self.root / "home/docs")})
        self.assertIsNone(itm.list_folders("~/missing/"))

    def test_odd_input(self):
        self.assertEqual(itm.complete_folders("~"), ["~/"])
        self.assertEqual(itm.complete_folders("~no-such-user-x/a"), [])
        self.assertEqual(itm.complete_folders("~/missing/a"), [])
        self.assertEqual(itm.complete_folders("plain"), [])

    def test_limits(self):
        self.assertEqual(len(itm.complete_folders("~/", limit=2)), 2)
        self.assertLessEqual(len(itm.complete_folders("~/", scan_limit=1)), 1)

    def test_fuzzy_match(self):
        self.assertEqual(itm.fuzzy_match("api", "api")[1], [0, 1, 2])
        prefix, word, inner, letters = (itm.fuzzy_match(q, t)[0] for q, t in
                                        [("ba", "backend"), ("api", "backend-api"), ("ken", "backend"), ("bkd", "backend")])
        self.assertGreater(prefix, word)
        self.assertGreater(word, inner)
        self.assertGreater(inner, letters)
        self.assertEqual(itm.fuzzy_match("fl", "fixLogin")[1], [0, 3])  # word starts, camelCase too
        self.assertIsNone(itm.fuzzy_match("xyz", "backend"))
        self.assertIsNone(itm.fuzzy_match("aaaaaaaaz", "a" * 400))  # linear: no stall on long names
        self.assertEqual(itm.fuzzy_match("", "x"), (0, []))

    def test_looks_like_path(self):
        for text in ("/", "~", "~/x", "./x", "../x", ".", ".."):
            self.assertTrue(itm.looks_like_path(text), text)
        for text in ("api", "my api", ".x", "git@github.com:a/b"):
            self.assertFalse(itm.looks_like_path(text), text)


if __name__ == "__main__":
    unittest.main()
