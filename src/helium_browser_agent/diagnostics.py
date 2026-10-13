"""Read-only diagnostics and profile discovery without opening browser tabs."""

import argparse
import json
import subprocess
from importlib.metadata import version

from helium_browser_agent import __version__, helium_cdp as H, helium_controller as controller


def doctor():
    result = {"version": __version__, "platform": "Linux", "dependencies": {}, "telemetry": "disabled by wrappers"}
    for name in ("browser-harness", "websockets"):
        result["dependencies"][name] = version(name)
    try:
        endpoint = controller.endpoint()
        result["endpoint"] = controller.public_endpoint(endpoint)
        client = H.Client(endpoint["url"])
        try:
            running = client.send("Browser.getVersion")
            result["running_browser"] = {"product": running.get("product"), "protocol": running.get("protocolVersion")}
        finally:
            client.close()
    except (OSError, RuntimeError) as error:
        result["endpoint"] = {"ok": False, "error": str(error)}
    try:
        result["profiles"] = list(H.profiles())
    except (OSError, ValueError) as error:
        result["profiles"] = {"error": str(error)}
    try:
        proc = subprocess.run([H.SETTINGS.agent_browser, "--version"], capture_output=True, text=True, timeout=5)
        result["agent_browser"] = {"available": proc.returncode == 0, "version": proc.stdout.strip()}
    except (OSError, subprocess.TimeoutExpired):
        result["agent_browser"] = {"available": False, "required_for": "agent-browser-helium only"}
    return result


def cli():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--version", action="version", version=__version__)
    parser.add_argument("command", choices=["doctor", "profiles"])
    args = parser.parse_args()
    try:
        result = doctor() if args.command == "doctor" else H.profiles()
        print(json.dumps(result, indent=2))
        return 0 if args.command == "profiles" or result["endpoint"]["ok"] else 1
    except (OSError, ValueError, RuntimeError) as error:
        parser.exit(1, "helium-agent: " + str(error) + "\n")
