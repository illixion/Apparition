#!/usr/bin/env python3
"""Select release parts from the versions committed at the built revision."""

import json
from pathlib import Path
import re
import sys
import tomllib


def plan(root, version, parts):
    version = version.removeprefix("v")
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", version):
        raise ValueError("version must be major.minor.patch")
    app = re.search(r'^\s*MARKETING_VERSION:\s*"?([0-9.]+)"?\s*$',
                    (root / "project.yml").read_text(), re.MULTILINE).group(1)
    installer = tomllib.loads((root / "installer/Cargo.toml").read_text())["package"]["version"]
    desktop = tomllib.loads((root / "desktop/Cargo.toml").read_text())["package"]["version"]
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
    apple = []
    for part, title, targets, brew in [
        ("app", "Apps (visionOS, iOS, tvOS)", "aarch64-apple-visionos,aarch64-apple-ios,aarch64-apple-tvos", "xcodegen"),
        ("macos", "macOS installer", "aarch64-apple-darwin,x86_64-apple-darwin", ""),
        ("windows", "Windows installer", "x86_64-pc-windows-gnu", "mingw-w64"),
    ]:
        if part in selected:
            apple.append(dict(part=part, title=title, targets=targets, brew=brew))
    linux = [dict(arch="x86_64", runner="ubuntu-24.04"),
             dict(arch="arm64", runner="ubuntu-24.04-arm")] if "linux" in selected else []
    return dict(parts=",".join(selected), apple=apple, linux=linux)


if __name__ == "__main__":
    try:
        result = plan(Path(__file__).resolve().parent.parent, sys.argv[1], sys.argv[2])
    except ValueError as error:
        sys.exit(f"::error::{error}")
    for key, value in result.items():
        print(f"{key}={json.dumps(value) if not isinstance(value, str) else value}")
