"""Run background Helium tasks with verified profile and owned-page boundaries."""

import argparse
import json
import math
import os
import re
import subprocess
import sys
from pathlib import Path

from helium_browser_agent import helium_cdp as H
from helium_browser_agent.helium_session import State, close_owned

from helium_browser_agent import helium_controller as controller


os.environ.update(
    BH_TELEMETRY="0", BROWSER_HARNESS_TELEMETRY="0", ANONYMIZED_TELEMETRY="false", BROWSER_USE_CLOUD_SYNC="false"
)
LIB = Path(__file__).resolve().parent


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--session", required=True)
    group = parser.add_mutually_exclusive_group()
    group.add_argument("--work", action="store_true")
    group.add_argument("--personal", action="store_true")
    group.add_argument("--profile")
    parser.add_argument("--close", action="store_true", help="Close owned pages and stop the task daemon")
    parser.add_argument("--lock-timeout", type=float, default=30)
    args = parser.parse_args()
    if not re.fullmatch(r"[A-Za-z0-9_-]{1,48}", args.session):
        parser.error("Session must use 1–48 letters, digits, underscores or hyphens")
    if not math.isfinite(args.lock_timeout) or args.lock_timeout < 0:
        parser.error("--lock-timeout must be nonnegative")
    explicit = args.profile or (
        H.SETTINGS.work_profile if args.work else H.SETTINGS.personal_profile if args.personal else None
    )
    root = H.CACHE_DIR / "harness"
    root.mkdir(mode=0o700, parents=True, exist_ok=True)
    os.chmod(root, 0o700)
    path = root / (args.session + ".json")
    # Read stdin before acquiring the lock, so an idle writer cannot block cleanup.
    code = "" if args.close else sys.stdin.read()
    with H.file_lock(root / (args.session + ".lock"), args.lock_timeout):
        workspace = H.CACHE_DIR / "workspace"
        workspace.mkdir(mode=0o700, parents=True, exist_ok=True)
        env = dict(
            os.environ,
            BU_NAME="hba-" + args.session,
            BH_TAB_MARKER="0",
            BH_RECORD="0",
            BH_AGENT_WORKSPACE=str(workspace),
        )
        for key in ("BU_BROWSER_ID", "BU_CDP_URL", "BU_AUTOSPAWN", "BH_REQUIRE_EXISTING_DAEMON"):
            env.pop(key, None)
        command = [sys.executable, "-m", "browser_harness.run"]
        if args.close and not path.exists():
            # --reload is a stop operation; it never provisions a browser/daemon.
            return subprocess.run([*command, "--reload"], env=env, capture_output=True, check=False).returncode
        if args.close:
            saved = json.loads(path.read_text())
            profile = H.canonical_profile(explicit or saved["profile"])
        else:
            profile = H.canonical_profile(explicit or H.default_profile())
        endpoint = H.verified_endpoint(controller, profile, launch=not args.close)
        generation = endpoint["url"].rsplit("/", 1)[-1]
        state = State(path, generation, profile)  # Validate before provisioning a daemon.
        env.update(BU_CDP_WS=endpoint["url"], HELIUM_PROFILE_NAME=profile)
        if args.close:
            stopped = subprocess.run([*command, "--reload"], env=env, capture_output=True, text=True, check=False)
            if stopped.returncode:
                raise RuntimeError("Could not stop task daemon; ownership state retained")
            client = H.Client(endpoint["url"])
            try:
                close_owned(state, client.send)
            finally:
                client.close()
            print("Closed task-owned tabs")
            return 0
        source = (
            "import sys\nsys.path.insert(0, " + repr(str(LIB.parent)) + ")\n"
            "from helium_browser_agent.helium_session import run_task\n"
            f"run_task({code!r}, globals(), {str(path)!r}, {generation!r}, {profile!r})\n"
        )
        return subprocess.run(command, input=source, text=True, env=env, check=False).returncode


def cli():
    try:
        raise SystemExit(main())
    except (OSError, ValueError, KeyError, RuntimeError) as error:
        print("helium-browser-use: " + str(error), file=sys.stderr)
        raise SystemExit(1)


if __name__ == "__main__":
    cli()
