"""What differs between operating systems: processes, temp folders, notifications.

Linux reads the process table from ``/proc``; macOS and the BSDs ask ``ps``.
Desktop notifications use ``notify-send`` (Linux, BSD) or ``osascript``
(macOS), unless ``ITM_NOTIFY_COMMAND`` names another program. Standard
library only, like the rest of the CLI and the daemon.
"""

from __future__ import annotations

import contextlib
import os
import shlex
import shutil
import stat
import subprocess
import sys
import tempfile
from pathlib import Path

PS_TIMEOUT = 5


def has_proc() -> bool:
    return Path("/proc/self/cmdline").exists()


def _ps(args: list[str]) -> str | None:
    try:
        result = subprocess.run(
            ["ps", "-ww", *args], capture_output=True, text=True, check=False, timeout=PS_TIMEOUT
        )
    except (OSError, subprocess.TimeoutExpired):
        return None
    return result.stdout if result.returncode == 0 else None


def process_argv(pid: int) -> list[str] | None:
    """The command line of ``pid``, or None when it is gone (or cannot be read).

    ``ps`` joins the arguments with spaces, so there an argument containing a
    space comes back as several.
    """
    if has_proc():
        try:
            raw = Path(f"/proc/{pid}/cmdline").read_bytes()
        except OSError:
            return None
        return [arg.decode(errors="replace") for arg in raw.split(b"\0") if arg]
    output = _ps(["-o", "command=", "-p", str(pid)])
    return output.split() if output and output.strip() else None


def pid_alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True  # someone else's process
    return True


def child_processes(parent: int) -> list[tuple[int, list[str]]]:
    """(pid, argv) of every process whose parent is ``parent``."""
    found = []
    if has_proc():
        for entry in Path("/proc").iterdir():
            if not entry.name.isdigit():
                continue
            try:
                stat_line = (entry / "stat").read_text()
                raw = (entry / "cmdline").read_bytes()
            except OSError:
                continue
            # The name in parentheses may contain spaces; the parent pid is the
            # second field after it.
            ppid = int(stat_line[stat_line.rindex(")") + 2 :].split()[1])
            if ppid == parent:
                found.append((int(entry.name), [a.decode(errors="replace") for a in raw.split(b"\0") if a]))
        return found
    # One -o per column: FreeBSD reads everything after the first "=" as its header.
    for line in (_ps(["-A", "-o", "pid=", "-o", "ppid=", "-o", "command="]) or "").splitlines():
        fields = line.split()
        if len(fields) >= 3 and fields[0].isdigit() and fields[1] == str(parent):
            found.append((int(fields[0]), fields[2:]))
    return found


def user_runtime_dir(name: str) -> Path:
    """The folder for ``name``'s runtime files (markers, caches).

    Every process must find the same folder, whether a shell or a service
    manager started it, and their environments differ: ``$XDG_RUNTIME_DIR``,
    else ``/run/user/<uid>`` when it is ours (what that variable names on
    systemd and elogind systems), else a private folder in ``/tmp`` (macOS:
    ``$TMPDIR`` differs between a login and a launchd agent).
    """
    if os.environ.get("XDG_RUNTIME_DIR"):
        return Path(os.environ["XDG_RUNTIME_DIR"]) / name
    uid = os.getuid()
    run_user = Path(f"/run/user/{uid}")
    with contextlib.suppress(OSError):
        if run_user.is_dir() and run_user.stat().st_uid == uid and os.access(run_user, os.W_OK):
            return run_user / name
    tmp = Path("/tmp") if os.access("/tmp", os.W_OK | os.X_OK) else Path(tempfile.gettempdir())
    return _private_dir(tmp / f"{name}-{uid}")


def _private_dir(path: Path) -> Path:
    """``path`` created private; refused when someone else owns it or it is a link.

    Shared temp folders let anyone create it first.
    """
    with contextlib.suppress(FileExistsError):
        path.mkdir(mode=0o700)
    info = path.lstat()
    if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid():
        raise PermissionError(f"{path} is not a folder owned by you")
    if stat.S_IMODE(info.st_mode) & 0o077:
        path.chmod(0o700)
    return path


def is_macos() -> bool:
    return sys.platform == "darwin"


def notify_command(title: str, body: str, app: str, with_open: bool) -> tuple[list[str], bool] | None:
    """The command that shows a desktop notification, and whether it offers Open.

    A command offering Open waits until the notification is closed and prints
    ``open`` when it was chosen. None when this system has no way to notify.
    """
    custom = (os.environ.get("ITM_NOTIFY_COMMAND") or "").strip()
    if custom:  # shlex raises ValueError for an unclosed quote
        return [*shlex.split(custom), title, body], False
    if is_macos():
        if not shutil.which("osascript"):
            return None
        script = "on run argv\ndisplay notification (item 2 of argv) with title (item 1 of argv)\nend run"
        return ["osascript", "-e", script, title, body], False
    if not shutil.which("notify-send"):
        return None
    args = ["notify-send", f"--app-name={app}", title, body]
    if with_open:
        args[1:1] = ["--action=open=Open", "--wait"]
    return args, with_open


def gnu_date() -> str | None:
    """A ``date`` that reads ``-d`` phrases: GNU's (``gdate`` on macOS) or uutils'.

    Probed with a relative phrase: BusyBox ``date`` reads ``-d`` too, but only
    absolute times.
    """
    for name in ("gdate", "date"):
        path = shutil.which(name)
        if not path:
            continue
        try:
            result = subprocess.run(
                [path, "-u", "-d", "1970-01-01 +1 day", "+%s"],
                capture_output=True,
                text=True,
                check=False,
                timeout=PS_TIMEOUT,
            )
        except (OSError, subprocess.TimeoutExpired):
            continue
        if result.returncode == 0 and result.stdout.strip() == "86400":
            return path
    return None
