#!/usr/bin/env python3
"""Select release parts from the versions committed at the built revision."""

import json
from pathlib import Path
import re
import sys
import tomllib


def plan(root, version, parts):
    app = re.search(r'^\s*MARKETING_VERSION:\s*"?([0-9.]+)"?\s*$',
                    (root / "project.yml").read_text(), re.MULTILINE).group(1)
    installer = tomllib.loads((root / "installer/Cargo.toml").read_text())["package"]["version"]
    desktop = tomllib.loads((root / "desktop/Cargo.toml").read_text())["package"]["version"]
    # No version: a run on main that only warms the caches every release restores from. It builds
    # each part at its committed version, and publishes nothing.
    if not version:
        if installer != desktop:
            raise ValueError(f"installer/desktop versions differ: {installer}/{desktop}")
        return matrix(["app", "macos", "windows", "linux"], dict(app=app, installers=installer), warm=True)
    version = version.removeprefix("v")
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", version):
        raise ValueError("version must be major.minor.patch")
    # A tag created while publishing an app-only release must not rebuild old installers.
    if not parts:
        selected = (["app"] if app == version else []) + (
            ["macos", "windows", "linux"] if installer == desktop == version else [])
    else:
        selected = ["app", "macos", "windows", "linux"] if parts == "all" else parts.replace(" ", "").split(",")
    if not selected or any(part not in ("app", "macos", "windows", "linux") for part in selected):
        raise ValueError("no matching release parts, or unknown part")
    if "app" in selected and app != version:
        raise ValueError(f"project.yml version is {app}, not {version}")
    if any(part != "app" for part in selected) and not installer == desktop == version:
        raise ValueError(f"installer/desktop versions are {installer}/{desktop}, not {version}")
    return matrix(selected, dict(app=version, installers=version), warm=False)


def matrix(selected, versions, warm):
    apple = []
    for part, title, targets, brew in [
        ("app", "Apps (visionOS, iOS, tvOS)", "aarch64-apple-visionos,aarch64-apple-ios,aarch64-apple-tvos", "xcodegen"),
        ("macos", "macOS installer", "aarch64-apple-darwin,x86_64-apple-darwin", ""),
        ("windows", "Windows installer", "x86_64-pc-windows-gnu", "mingw-w64"),
    ]:
        if part in selected:
            version = versions["app" if part == "app" else "installers"]
            apple.append(dict(part=part, title=title, targets=targets, brew=brew, version=version))
    linux = [dict(arch="x86_64", runner="ubuntu-24.04", version=versions["installers"]),
             dict(arch="arm64", runner="ubuntu-24.04-arm", version=versions["installers"])] \
        if "linux" in selected else []
    return dict(parts=",".join(selected), warm=json.dumps(warm), apple=apple, linux=linux)


if __name__ == "__main__":
    try:
        result = plan(Path(__file__).resolve().parent.parent, sys.argv[1], sys.argv[2] if len(sys.argv) > 2 else "")
    except ValueError as error:
        sys.exit(f"::error::{error}")
    for key, value in result.items():
        print(f"{key}={json.dumps(value) if not isinstance(value, str) else value}")
