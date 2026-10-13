"""helium-tab: open/inspect tabs in a specific Helium profile over CDP.

  helium-tab new [--profile NAME] [--url URL] [--foreground]   -> prints targetId
  helium-tab contexts [--refresh]                               -> JSON {profile: browserContextId}
  helium-tab profile-of TARGET_ID                               -> profile name
  helium-tab exists TARGET_ID                                   -> exit 0/1
  helium-tab ws                                                 -> Helium CDP WebSocket URL
  helium-tab close-browser                                      -> clean quit (used by helium-restart)
Profile defaults to HELIUM_PROFILE_NAME or the configured default profile.
"""

import argparse
import json
import sys

from helium_browser_agent import helium_cdp as H

from helium_browser_agent import helium_controller as controller


class Client(H.Client):
    def __init__(self):
        super().__init__(controller.endpoint()["url"])


def main():
    ap = argparse.ArgumentParser(prog="helium-tab")
    sub = ap.add_subparsers(dest="cmd", required=True)
    n = sub.add_parser("new")
    n.add_argument("--profile", default=None)
    n.add_argument("--url", default="about:blank")
    n.add_argument("--foreground", action="store_true")
    c = sub.add_parser("contexts")
    c.add_argument("--refresh", action="store_true")
    c.add_argument("--require-complete", action="store_true", help="Fail if any live profile is unidentified")
    sub.add_parser("identify").add_argument("--profile", required=True)
    sub.add_parser("profile-of").add_argument("target_id")
    sub.add_parser("exists").add_argument("target_id")
    sub.add_parser("ws")
    sub.add_parser("close-browser")
    a = ap.parse_args()
    if a.cmd == "ws":
        print(controller.endpoint()["url"])
        return
    cl = Client()
    try:
        if a.cmd == "new":
            print(H.new_tab(cl.send, a.profile, a.url, background=not a.foreground))
        elif a.cmd == "contexts":
            contexts = H.resolve_contexts(cl.send, force=a.refresh)
            live = {t.get("browserContextId") for t in H.targets(cl.send) if t["type"] == "page"}
            if a.require_complete and live - set(contexts.values()):
                raise RuntimeError(
                    "Some open profiles are unidentified; use helium-tab identify --profile NAME before restarting"
                )
            print(json.dumps(contexts, indent=1))
        elif a.cmd == "identify":
            print(H.context_for(cl.send, a.profile))
        elif a.cmd == "profile-of":
            print(H.profile_of(cl.send, a.target_id) or "")
        elif a.cmd == "close-browser":
            cl.send("Browser.close")
        elif a.cmd == "exists":
            sys.exit(0 if H.target_exists(cl.send, a.target_id) else 1)
    finally:
        cl.ws.close()


def cli():
    try:
        main()
    except (RuntimeError, ValueError, KeyError, OSError) as e:
        print(f"helium-tab: {e}", file=sys.stderr)
        sys.exit(2)


if __name__ == "__main__":
    cli()
