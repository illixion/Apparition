# Shared symbol retention for build-and-sign and Xcode product registration.
DEBUGTRACE_DSYM_DIR="${DEBUGTRACE_DSYM_DIR:-${XDG_STATE_HOME:-$HOME/.local/state}/debugtrace/dsyms}"
archive_dsyms() {
    local build_no
    build_no=$(plutil -extract CFBundleVersion raw -o - "$APP_BUNDLE/Info.plist" 2>/dev/null \
        || plutil -extract CFBundleVersion raw -o - "$APP_BUNDLE/Contents/Info.plist" 2>/dev/null || true)
    DSYM_PRODUCTS="${DSYM_PRODUCTS_OVERRIDE:-$(dirname "$APP_PATH")}" DSYM_APP="$APP_PATH" DSYM_ARCHIVE="$DEBUGTRACE_DSYM_DIR" \
    DSYM_BUILD="$build_no" DSYM_KEY_ID="$TRACE_KEY_ID" DSYM_BUILD_ID="${BUILD_ID:-$TRACE_KEY_ID}" DSYM_BUNDLE="$BUILD_BUNDLE_ID" \
    DSYM_PLATFORM="$PLATFORM" DSYM_CONFIG="$CONFIG" DSYM_DEVICE="${DEVICE_NAME:-local}" /usr/bin/python3 -c '
import hashlib, json, os, pathlib, re, shutil, subprocess, tempfile, time, uuid

e = os.environ
sources = [pathlib.Path(e["DSYM_SOURCE"])] if e.get("DSYM_SOURCE") else sorted(pathlib.Path(e["DSYM_PRODUCTS"]).glob("*.dSYM"))
if not sources:
    print("WARNING: no dSYMs found; this build cannot be symbolicated")
    raise SystemExit(0)
uuid_pattern = re.compile(r"UUID: ([0-9A-Fa-f-]{36}) ")
def binary_uuids(path):
    output = subprocess.run(["dwarfdump", "--uuid", str(path)], check=True, capture_output=True, text=True).stdout
    return {item.upper() for item in uuid_pattern.findall(output)}
# The products directory can contain stale dSYMs from other targets. Keep only
# UUIDs actually present in this app and its embedded frameworks/extensions.
magic = {bytes.fromhex(value) for value in ("feedface", "cefaedfe", "feedfacf", "cffaedfe", "cafebabe", "bebafeca", "cafebabf", "bfbafeca")}
wanted = set()
for binary in pathlib.Path(e["DSYM_APP"]).rglob("*"):
    if binary.is_file():
        with binary.open("rb") as handle:
            is_macho = handle.read(4) in magic
        if is_macho:
            wanted.update(binary_uuids(binary))
build_id = e["DSYM_BUILD_ID"] or uuid.uuid4().hex
bundle = e["DSYM_BUNDLE"]
if not re.fullmatch(r"[A-Za-z0-9.-]+", bundle) or not re.fullmatch(r"[A-Za-z0-9-]+", build_id):
    raise SystemExit("invalid symbol archive identity")
root = pathlib.Path(e["DSYM_ARCHIVE"])
root.mkdir(parents=True, exist_ok=True)
slot = root / (bundle + "@" + build_id)
stage = pathlib.Path(tempfile.mkdtemp(prefix=".incoming-", dir=root))
try:
    uuids = {}
    for source in sources:
        if not binary_uuids(source).intersection(wanted):
            continue
        copied = stage / source.name
        clone = subprocess.run(["/bin/cp", "-cR", str(source), str(stage)], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        if clone.returncode:
            if copied.exists():
                shutil.rmtree(copied)
            shutil.copytree(source, copied)
        out = subprocess.run(["dwarfdump", "--uuid", str(copied)], check=True, capture_output=True, text=True).stdout
        for line in out.splitlines():
            match = re.match(r"UUID: ([0-9A-Fa-f-]{36}) \((\S+)\) (.+)$", line)
            if match:
                if match.group(1).upper() not in wanted:
                    continue
                dwarf = pathlib.Path(match.group(3)).resolve()
                relative = dwarf.relative_to(stage.resolve())
                if not dwarf.is_file():
                    raise ValueError("missing archived DWARF file")
                digest = hashlib.sha256()
                with dwarf.open("rb") as handle:
                    for chunk in iter(lambda: handle.read(1024 * 1024), b""):
                        digest.update(chunk)
                uuids.setdefault(match.group(1).upper(), {"arch": match.group(2), "dwarf": str(relative),
                                 "sha256": digest.hexdigest(), "bytes": dwarf.stat().st_size})
    if not uuids:
        raise ValueError("no binary UUIDs found in the dSYMs")
    record = {"format": "apparition-symbols/1", "buildId": build_id, "keyId": e["DSYM_KEY_ID"],
              "bundleId": bundle, "device": e["DSYM_DEVICE"], "build": e["DSYM_BUILD"],
              "platform": e["DSYM_PLATFORM"], "configuration": e["DSYM_CONFIG"],
              "archivedAt": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()), "uuids": uuids}
    (stage / "uuids.json").write_text(json.dumps(record, indent=1))
    if slot.exists():
        old = json.loads((slot / "uuids.json").read_text())
        if any(old.get(k) != record.get(k) for k in ("buildId", "bundleId", "build", "uuids")):
            raise ValueError("refusing to overwrite a different symbol set for this build id")
    else:
        stage.rename(slot)
    print("==> Kept immutable dSYMs for build " + build_id + " (" + str(len(uuids)) + " UUIDs)")
finally:
    if stage.exists():
        shutil.rmtree(stage)
'
}
