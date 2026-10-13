"""Explicit local configuration; no project-directory or account assumptions."""

import os
import shutil
import tomllib
from dataclasses import dataclass, field, fields
from pathlib import Path


def xdg(name, fallback):
    return Path(os.environ.get(name, Path.home() / fallback)).expanduser()


@dataclass
class Settings:
    profile_dir: Path = field(default_factory=lambda: xdg("XDG_CONFIG_HOME", ".config") / "net.imput.helium")
    cache_dir: Path = field(default_factory=lambda: xdg("XDG_CACHE_HOME", ".cache") / "helium-browser-agent")
    launcher: str = field(default_factory=lambda: shutil.which("helium-browser") or "/usr/bin/helium-browser")
    agent_browser: str = "agent-browser"
    default_profile: str = "Personal"
    personal_profile: str = "Personal"
    work_profile: str = "Work"
    workspaces: dict = field(default_factory=dict)

    def profile_for(self, cwd):
        override = os.environ.get("HELIUM_PROFILE_NAME")
        if override:
            return override
        cwd = Path(cwd).resolve()
        matches = [(Path(folder).expanduser().resolve(), name) for folder, name in self.workspaces.items()]
        for folder, name in sorted(matches, key=lambda pair: len(pair[0].parts), reverse=True):
            if cwd.is_relative_to(folder):
                return name
        return self.default_profile


def load(path=None):
    explicit = path or os.environ.get("HELIUM_AGENT_CONFIG")
    path = (
        Path(explicit).expanduser()
        if explicit
        else xdg("XDG_CONFIG_HOME", ".config") / "helium-browser-agent/config.toml"
    )
    if not path.exists() and not explicit:
        values = {}
    else:
        with path.open("rb") as stream:
            values = tomllib.load(stream)
    unknown = set(values) - {f.name for f in fields(Settings)}
    if unknown:
        raise ValueError("Unknown configuration keys: " + ", ".join(sorted(unknown)))
    for key, value in values.items():
        if key == "workspaces":
            if not isinstance(value, dict) or not all(
                isinstance(k, str) and isinstance(v, str) and v.strip() for k, v in value.items()
            ):
                raise ValueError("workspaces must map directory paths to profile names")
        elif not isinstance(value, str) or not value.strip():
            raise ValueError(key + " must be a nonempty string")
    for key in ("profile_dir", "cache_dir"):
        if key in values:
            values[key] = Path(values[key]).expanduser().resolve()
    for key in ("launcher", "agent_browser"):
        if key in values:
            values[key] = os.path.expanduser(values[key])
    if os.environ.get("HELIUM_PROFILE_DIR"):
        values["profile_dir"] = Path(os.environ["HELIUM_PROFILE_DIR"]).expanduser().resolve()
    if os.environ.get("HELIUM_PROFILE_NAME"):
        values["default_profile"] = os.environ["HELIUM_PROFILE_NAME"]
    return Settings(**values)


SETTINGS = load()
