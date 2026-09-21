"""Herdr link handler: open clicked file paths and file:// links in sid.

The link is the token herdr found under the click (`src/a.rs:12:3`, `main.ts(12,3)`,
`~/x/y.md`). It is resolved against the clicked pane's working directory; a token
that is not an existing file is ignored, so a false positive costs nothing.
Reuse sid in the tab or workspace; otherwise launch it in a new adjacent pane.
"""
import json
import os
import re
import shlex
import shutil
import socket
import subprocess
import sys
from urllib.parse import unquote, urlsplit

HERDR = os.environ.get("HERDR_BIN_PATH", "herdr")


def herdr(*args):
    result = subprocess.run([HERDR, *args], capture_output=True, text=True, check=False)
    if result.returncode != 0:
        raise RuntimeError(f"herdr {' '.join(args)}: {result.stderr.strip() or result.stdout.strip()}")
    if not result.stdout.strip():
        return None
    payload = json.loads(result.stdout)
    if "error" in payload:
        raise RuntimeError(f"herdr {' '.join(args)}: {payload['error']}")
    return payload["result"]


def split_position(token):
    """`a.rs:12:3` → (a.rs, 12, 3); `a.ts(12,3)` → (a.ts, 12, 3); `a.rs` → (a.rs, None, None)."""
    match = re.fullmatch(r"(.*?)\((\d+)(?:[,:](\d+))?\)", token)
    if match:
        return match.group(1), int(match.group(2)), int(match.group(3)) if match.group(3) else None
    match = re.fullmatch(r"(.*?):(\d+)(?::(\d+))?:?", token)
    if match:
        return match.group(1), int(match.group(2)), int(match.group(3)) if match.group(3) else None
    return token.rstrip(":"), None, None


def resolve(path, bases):
    path = os.path.expanduser(path)
    if os.path.isabs(path):
        return path if os.path.isfile(path) else None
    for base in bases:
        if not base:
            continue
        candidate = os.path.normpath(os.path.join(base, path))
        if os.path.isfile(candidate):
            return candidate
    return None


def helix_pane(clicked_pane_id, tab_id, workspace_id):
    """The pane running sid in the clicked pane's tab, else anywhere in its workspace."""
    panes = herdr("pane", "list")["panes"]
    same_tab = [p for p in panes if p.get("tab_id") == tab_id and p["pane_id"] != clicked_pane_id]
    same_workspace = [
        p for p in panes
        if p.get("workspace_id") == workspace_id and p.get("tab_id") != tab_id
    ]
    for pane in same_tab + same_workspace:
        info = herdr("pane", "process-info", "--pane", pane["pane_id"])["process_info"]
        for process in info.get("foreground_processes", []):
            if process.get("name") == "sid":
                return pane
    return None


def focus(clicked_pane_id, target_pane_id):
    for direction in ("left", "right", "up", "down"):
        try:
            neighbor = herdr("pane", "neighbor", "--pane", clicked_pane_id, "--direction", direction)
        except RuntimeError:
            continue
        found = neighbor.get("neighbor", {}).get("neighbor_pane_id")
        if found == target_pane_id:
            herdr("pane", "focus", "--pane", clicked_pane_id, "--direction", direction)
            return


def main():
    context = json.loads(os.environ.get("HERDR_PLUGIN_CONTEXT_JSON", "{}"))
    token = context.get("clicked_url") or os.environ.get("HERDR_PLUGIN_CLICKED_URL")
    if not token:
        return
    clicked_pane_id = context.get("focused_pane_id")
    if not clicked_pane_id:
        return

    token = token.strip()
    if token.startswith("file://"):
        url = urlsplit(token)
        if url.netloc not in ("", "localhost", socket.gethostname(), socket.getfqdn()):
            raise ValueError(f"Cannot open a file on another host: {url.netloc}")
        token = unquote(url.path)
        if re.fullmatch(r"L?\d+(?::\d+)?", url.fragment):
            token += ":" + url.fragment.removeprefix("L")
    if any(char in token for char in "\r\n\0"):
        raise ValueError("A clicked path cannot contain a line break or NUL")
    path, line, col = split_position(token)
    pane = herdr("pane", "get", clicked_pane_id)["pane"]
    bases = [pane.get("foreground_cwd"), pane.get("cwd"), context.get("workspace_cwd")]
    resolved = resolve(path, bases)
    if not resolved:
        print(f"{token}: not a file under {bases}")
        return

    target = helix_pane(clicked_pane_id, pane.get("tab_id"), pane.get("workspace_id"))

    location = resolved
    if line:
        location += f":{line}"
        if col:
            location += f":{col}"
    if not target:
        sid = shutil.which("sid")
        if not sid:
            raise RuntimeError("sid is not on PATH")
        cwd = next(
            (base for base in bases if base and os.path.isdir(base)),
            os.path.dirname(resolved),
        )
        target = herdr(
            "pane", "split", clicked_pane_id, "--direction", "right",
            "--cwd", cwd, "--focus",
        )["pane"]
        herdr("pane", "run", target["pane_id"], shlex.join([sid, "--", location]))
        print(f"{token} -> {location} (new sid)")
        return

    target_id = target["pane_id"]
    # sid opens in insert mode, where ":" is text and Escape no longer leaves the mode,
    # so the command line is reached through F10, which opens it in any mode.
    herdr("pane", "send-keys", target_id, "esc")
    herdr("pane", "send-keys", target_id, "f10")
    # sid's command line escapes a literal single quote by doubling it.
    quoted = "'" + location.replace("'", "''") + "'"
    herdr("pane", "send-text", target_id, f"open {quoted}")
    herdr("pane", "send-keys", target_id, "enter")
    focus(clicked_pane_id, target_id)
    # What the click resolved to, so `herdr plugin log` answers what was taken.
    print(f"{token} -> {location}")


if __name__ == "__main__":
    try:
        main()
    except Exception as err:  # a plugin's stderr lands in `herdr plugin log`
        print(err, file=sys.stderr)
        sys.exit(1)
