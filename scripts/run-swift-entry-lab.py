#!/usr/bin/env python3
"""Build and verify the isolated Swift entry-patching lab on an Apple device.

Installs only com.illixion.apparition.lab. Uses Apparition's existing pairing and
build-and-sign's existing development identity; never modifies the production app.
"""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import time


ROOT = Path(__file__).resolve().parents[1]
OUT = ROOT / "build" / "swift-entry-lab"
BUNDLE = "com.illixion.apparition.lab"


def run(*args, **kwargs):
    return subprocess.run([str(a) for a in args], cwd=ROOT, check=True, **kwargs)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--device", default="AVP")
    parser.add_argument("--platform", choices=["visionOS", "iOS"], default="visionOS")
    parser.add_argument("--skip-build", action="store_true", help="reuse the installed lab app")
    args = parser.parse_args()
    apparition = shutil.which("apparition") or str(Path.home() / ".local/bin/apparition")
    bas = Path.home() / "bin/build-and-sign"
    out = OUT / args.platform
    out.mkdir(parents=True, exist_ok=True)
    if not args.skip_build:
        run("xcodegen")
        env = dict(os.environ, SCHEME_NAME="ApparitionLab", TARGET_NAME="ApparitionLab",
                   BUILD_BUNDLE_ID=BUNDLE)
        run(bas, "--platform", args.platform, "--device", args.device, env=env)
    sdk_name, target = ("xros", "arm64-apple-xros26.0") if args.platform == "visionOS" else (
        "iphoneos", "arm64-apple-ios17.0")
    sdk = run("xcrun", "--sdk", sdk_name, "--show-sdk-path", capture_output=True,
              text=True).stdout.strip()
    dylib = out / "swift-entry-patch.dylib"
    # Compile after the host build and deliver independently, never in its bundle.
    run("xcrun", "--sdk", sdk_name, "swiftc", "-O", "-parse-as-library",
        "-emit-library", "-module-name", "SwiftEntryPatch", "-target", target,
        "-sdk", sdk, ROOT / "Lab/Patches/SwiftEntryPatch.swift", "-o", dylib)
    run(bas, "--sign-dylib", dylib)
    device = ["xcrun", "devicectl", "device"]
    domain = ["--device", args.device, "--domain-type", "appDataContainer",
              "--domain-identifier", BUNDLE]
    run(*device, "copy", "to", *domain, "--source", dylib,
        "--destination", "Documents/swift-entry-patch.dylib")
    pending = out / "pending.json"
    pending.write_text('{"status":"pending"}\n')

    def collect(name, expected):
        # A crash before writing the report must never look like an old success.
        run(*device, "copy", "to", *domain, "--source", pending,
            "--destination", "Documents/swift-entry-report.json")
        if name == "patched":
            run(apparition, "jit", BUNDLE, "--device", args.device, "--wait", "60")
        else:
            run(*device, "process", "launch", "--device", args.device,
                "--terminate-existing", BUNDLE)
        report_path = out / f"{name}.json"
        for _ in range(10):
            run(*device, "copy", "from", *domain, "--source", "Documents/swift-entry-report.json",
                "--destination", report_path)
            report = json.loads(report_path.read_text())
            if report.get("status") != "pending":
                break
            time.sleep(1)
        if report.get("status") != expected:
            raise RuntimeError(f"{name} failed: {report}")
        if name == "patched":
            if not (report["scalar"] == report["directScalar"] == [11, 17, 11]
                    and report["text"] == ["original:hello", "replacement:hello", "original:hello"]
                    and report["heapStringChecks"] == 1000 and report["writes"] == [0, 0, 0, 0]):
                raise RuntimeError(f"incorrect patch results: {report}")
        elif report.get("scalar") != 11 or report.get("text") != "original:hello":
            raise RuntimeError(f"incorrect control results: {report}")
        print(json.dumps(report, sort_keys=True), flush=True)

    collect("patched", "passed")
    collect("control", "not-traced")
    print(f"PASS: native Swift entries, restoration, heap Strings and control; reports in {out}")


if __name__ == "__main__":
    main()
