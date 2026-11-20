"""Read release Mach-O UUIDs and assign content-derived UUIDs before signing."""

import hashlib
import struct
import sys
from pathlib import Path

THIN = b"\xcf\xfa\xed\xfe"  # 64-bit little-endian Mach-O
FAT = b"\xca\xfe\xba\xbe"  # lipo's universal executable
LC_UUID = 0x1B
LC_CODE_SIGNATURE = 0x1D


def uuid_offsets(data, unsigned=False):
    """Find the UUID field of each supported Mach-O slice."""
    if data[:4] == THIN:
        slices = [(0, len(data))]
    elif data[:4] == FAT and len(data) >= 8:
        count = struct.unpack_from(">I", data, 4)[0]
        if not count or 8 + count * 20 > len(data):
            raise ValueError("invalid universal Mach-O header")
        slices = [struct.unpack_from(">II", data, 16 + i * 20) for i in range(count)]
    else:
        raise ValueError("expected a 64-bit Mach-O executable")

    offsets = []
    for start, size in slices:
        if size < 32 or start + size > len(data) or data[start:start + 4] != THIN:
            raise ValueError("invalid Mach-O slice")
        count, length = struct.unpack_from("<II", data, start + 16)
        end = start + 32 + length
        if end > start + size:
            raise ValueError("invalid Mach-O load commands")
        pos = start + 32
        uuid = None
        for _ in range(count):
            if pos + 8 > end:
                raise ValueError("truncated Mach-O load command")
            command, length = struct.unpack_from("<II", data, pos)
            if length < 8 or pos + length > end:
                raise ValueError("invalid Mach-O load command")
            if command == LC_UUID:
                if uuid is not None or length != 24:
                    raise ValueError("invalid Mach-O UUID")
                uuid = pos + 8
            if unsigned and command == LC_CODE_SIGNATURE:
                raise ValueError("assign release UUIDs before signing")
            pos += length
        if uuid is None or pos != end:
            raise ValueError("missing UUID or invalid Mach-O load commands")
        offsets.append(uuid)
    return offsets


def set_release_uuid(path):
    data = bytearray(path.read_bytes())
    if data[:4] != THIN:
        raise ValueError("assign release UUIDs to thin executables before running lipo")
    [offset] = uuid_offsets(data, unsigned=True)
    data[offset:offset + 16] = bytes(16)
    uuid = bytearray(hashlib.sha256(data).digest()[:16])
    uuid[6] = (uuid[6] & 0x0F) | 0x80  # Custom version 8 UUID, RFC variant.
    uuid[8] = (uuid[8] & 0x3F) | 0x80
    data[offset:offset + 16] = uuid
    path.write_bytes(data)


if __name__ == "__main__":
    if len(sys.argv) < 2:
        sys.exit("usage: release_macho.py <unsigned thin Mach-O> ...")
    for name in sys.argv[1:]:
        try:
            set_release_uuid(Path(name))
        except (OSError, ValueError) as error:
            sys.exit(f"{name}: {error}")
