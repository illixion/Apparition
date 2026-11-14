#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Give an unsigned Mach-O a content-derived LC_UUID in reserved header padding.

Apple's linker requires a UUID when clients link this dylib. Generate it from
content instead of permitting build-directory paths to affect the linker's UUID.
No sections or linkedit offsets move.
"""
import hashlib
from pathlib import Path
import struct
import sys

path = Path(sys.argv[1])
data = bytearray(path.read_bytes())
magic, = struct.unpack_from('<I', data)
if magic != 0xfeedfacf:
    raise ValueError('expected a thin 64-bit Mach-O')
count, size = struct.unpack_from('<II', data, 16)
at = 32
first_section = len(data)
for _ in range(count):
    cmd, length = struct.unpack_from('<II', data, at)
    if cmd == 0x1b:
        raise ValueError('UUID already exists')
    if cmd == 0x19:
        nsects, = struct.unpack_from('<I', data, at + 64)
        for section in range(nsects):
            offset, = struct.unpack_from('<I', data, at + 72 + section*80 + 48)
            if offset:
                first_section = min(first_section, offset)
    at += length
if at != 32 + size or at + 24 > first_section or any(data[at:at + 24]):
    raise ValueError('no reserved header space for UUID')
digest = bytearray(hashlib.sha256(data).digest()[:16])
digest[6] = (digest[6] & 15) | 0x80  # UUIDv8, application-specific hash
_digest_variant = (digest[8] & 63) | 0x80
digest[8] = _digest_variant
struct.pack_into('<II16s', data, at, 0x1b, 24, bytes(digest))
struct.pack_into('<II', data, 16, count + 1, size + 24)
path.write_bytes(data)
