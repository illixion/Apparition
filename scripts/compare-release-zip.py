#!/usr/bin/env python3
"""Compare a release zip CI built with the same zip rebuilt here (scripts/audit-release.sh).

  scripts/compare-release-zip.py <ci.zip> <rebuilt.zip>

They match when their bytes do, or else when they hold the same entries: the same names, the same
kinds (file, folder, symbolic link), the same Unix modes and the same contents. Only the zip's own
framing (dates, compression, entry order) may differ. Exits 0 when they match, 1 when they don't,
listing every entry that differs. For a Mach-O file it also checks whether the differences are
confined to the signature and build UUID. These diagnostics do not change the comparison result.
"""

import hashlib
import stat
import subprocess
import sys
import tempfile
import zipfile
from pathlib import Path

from release_macho import uuid_offsets

MACHO = {b"\xca\xfe\xba\xbe", b"\xcf\xfa\xed\xfe", b"\xce\xfa\xed\xfe", b"\xfe\xed\xfa\xcf"}


def sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def entries(path):
    """name -> (kind, mode, sha256 of the contents) for every entry of a zip."""
    out = {}
    with zipfile.ZipFile(path) as z:
        for info in z.infolist():
            mode = info.external_attr >> 16
            if info.is_dir() or stat.S_ISDIR(mode):
                kind = "folder"
            elif stat.S_ISLNK(mode):
                kind = "symlink"
            else:
                kind = "file"
            name = info.filename.rstrip("/")
            if name in out:
                sys.exit(f"error: {path} has {name} twice")
            out[name] = (kind, stat.S_IMODE(mode), hashlib.sha256(z.read(info)).hexdigest() if kind != "folder" else "")
    return out


def macho_digests(data, work):
    """Hash a Mach-O without its signature, then without its UUIDs as well."""
    path = Path(work) / "macho"
    path.write_bytes(data)
    result = subprocess.run(["codesign", "--remove-signature", str(path)], capture_output=True)
    if result.returncode != 0:
        return None, None
    unsigned = bytearray(path.read_bytes())
    digest = hashlib.sha256(unsigned).digest()
    try:
        for offset in uuid_offsets(unsigned):
            unsigned[offset:offset + 16] = bytes(16)
    except ValueError:
        return digest, None
    return digest, hashlib.sha256(unsigned).digest()


def main():
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    ci, rebuilt = Path(sys.argv[1]), Path(sys.argv[2])
    if sha256(ci) == sha256(rebuilt):
        print(f"    {ci.name}: identical bytes")
        return 0
    a, b = entries(ci), entries(rebuilt)
    problems = []
    for name in sorted(a.keys() | b.keys()):
        if name not in b:
            problems.append(f"{name}: only in CI's zip")
        elif name not in a:
            problems.append(f"{name}: only in the rebuilt zip")
        elif a[name][0] != b[name][0]:
            problems.append(f"{name}: a {a[name][0]} in CI's zip, a {b[name][0]} rebuilt")
        elif a[name][1] != b[name][1]:
            problems.append(f"{name}: mode {a[name][1]:o} in CI's zip, {b[name][1]:o} rebuilt")
        elif a[name][2] != b[name][2]:
            note = ""
            with zipfile.ZipFile(ci) as za, zipfile.ZipFile(rebuilt) as zb, tempfile.TemporaryDirectory() as work:
                da = za.read(next(i for i in za.infolist() if i.filename.rstrip("/") == name))
                db = zb.read(next(i for i in zb.infolist() if i.filename.rstrip("/") == name))
                if da[:4] in MACHO and db[:4] in MACHO:
                    ua, uuid_free_a = macho_digests(da, work)
                    ub, uuid_free_b = macho_digests(db, work)
                    if ua and ua == ub:
                        note = " (only the code signature differs)"
                    elif uuid_free_a and uuid_free_a == uuid_free_b:
                        note = " (difference confined to the Mach-O UUID and signature)"
                    else:
                        note = " (Mach-O contents differ)"
            problems.append(f"{name}: different contents{note}")
    if problems:
        print(f"    {ci.name}: DIFFERS from the rebuild")
        for p in problems:
            print(f"      {p}")
        return 1
    print(f"    {ci.name}: same entries, names, modes and contents ({len(a)}); only the zip framing differs")
    return 0


if __name__ == "__main__":
    sys.exit(main())
