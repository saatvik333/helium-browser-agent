"""Verify Linux Helium process identity and ownership of its loopback CDP socket."""

import os
import re
from pathlib import Path
from typing import Any
from urllib.parse import urlsplit

WS_PATH = re.compile(r"^/devtools/browser/[A-Za-z0-9._~-]+$")


class Error(RuntimeError):
    pass


def _start_time(pid: int) -> str:
    text = Path(f"/proc/{pid}/stat").read_text()
    end = text.rfind(")")
    if end < 0:
        raise Error("cannot parse Helium process identity")
    return text[end + 2 :].split()[19]


def _helium_pid() -> int:
    matches = []
    for proc in Path("/proc").iterdir():
        if not proc.name.isdigit():
            continue
        try:
            if Path(os.readlink(proc / "exe").removesuffix(" (deleted)")).name != "helium":
                continue
            if b"--type=" in (proc / "cmdline").read_bytes().replace(b"\0", b" "):
                continue
            matches.append(int(proc.name))
        except (FileNotFoundError, PermissionError, ProcessLookupError):
            pass
    if len(matches) != 1:
        raise Error(f"expected one Helium main process, found {len(matches)}")
    return matches[0]


def _owned_sockets(pid: int) -> set[str]:
    result = set()
    try:
        entries = list(Path(f"/proc/{pid}/fd").iterdir())
    except OSError as exc:
        raise Error("cannot inspect Helium socket ownership") from exc
    for entry in entries:
        try:
            match = re.fullmatch(r"socket:\[(\d+)]", os.readlink(entry))
            if match:
                result.add(match.group(1))
        except OSError:
            pass
    return result


def _listener(port: int) -> str:
    wanted = f"{port:04X}"
    loopback = {"0100007F", "00000000000000000000000001000000"}
    for table in (Path("/proc/net/tcp"), Path("/proc/net/tcp6")):
        try:
            lines = table.read_text().splitlines()[1:]
        except OSError:
            continue
        for line in lines:
            fields = line.split()
            if len(fields) < 10 or fields[3] != "0A":
                continue
            address, port_hex = fields[1].split(":", 1)
            if address in loopback and port_hex == wanted:
                return fields[9]
    raise Error("Helium DevTools endpoint is not listening on loopback")


def endpoint() -> dict[str, Any]:
    pid = _helium_pid()
    from helium_browser_agent import helium_cdp

    try:
        resolved = urlsplit(helium_cdp.ws_url())
        port, path = resolved.port, resolved.path
    except (OSError, ValueError, KeyError, RuntimeError) as exc:
        raise Error("Helium direct debugging endpoint is unavailable") from exc
    if resolved.hostname != "127.0.0.1" or port is None:
        raise Error("Helium debugging endpoint is not loopback")
    if not 1024 <= port <= 65535 or not WS_PATH.fullmatch(path):
        raise Error("Helium DevTools endpoint failed validation")
    inode = _listener(port)
    if inode not in _owned_sockets(pid):
        raise Error("Helium DevTools endpoint owner does not match its process")
    return {"pid": pid, "start": _start_time(pid), "inode": inode, "url": f"ws://127.0.0.1:{port}{path}"}


def public_endpoint(value: dict[str, Any]) -> dict[str, Any]:
    return {
        "ok": True,
        "pid": value["pid"],
        "process_start_time": value["start"],
        "transport": "verified_local_cdp",
        "network_scope": "loopback",
        "owner_verified": True,
    }
