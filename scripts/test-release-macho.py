import struct
import tempfile
import unittest
from pathlib import Path

from release_macho import FAT, LC_CODE_SIGNATURE, LC_UUID, THIN, set_release_uuid, uuid_offsets


def thin(uuid, payload=b"program", signed=False):
    commands = struct.pack("<II", LC_UUID, 24) + uuid
    if signed:
        commands += struct.pack("<4I", LC_CODE_SIGNATURE, 16, 0, 0)
    return THIN + struct.pack("<7I", 0, 0, 2, 1 + signed, len(commands), 0, 0) + commands + payload


class ReleaseMachOTests(unittest.TestCase):
    def test_release_uuid_depends_on_content(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "executable"

            def rewrite(data):
                path.write_bytes(data)
                set_release_uuid(path)
                return path.read_bytes()

            first = rewrite(thin(b"a" * 16))
            self.assertEqual(first, rewrite(thin(b"b" * 16)))
            self.assertEqual(first, rewrite(first))
            self.assertNotEqual(first[40:56], rewrite(thin(b"a" * 16, b"changed"))[40:56])
            self.assertEqual(first[56:], b"program")
            self.assertEqual(first[46] >> 4, 8)
            self.assertEqual(first[48] >> 6, 2)
            with self.assertRaisesRegex(ValueError, "before signing"):
                rewrite(thin(b"a" * 16, signed=True))

    def test_read_uuid_from_each_universal_slice(self):
        first, second = thin(b"a" * 16), thin(b"b" * 16)
        data = FAT + struct.pack(">I", 2)
        data += struct.pack(">5I", 0, 0, 48, len(first), 0)
        data += struct.pack(">5I", 0, 0, 48 + len(first), len(second), 0)
        data += first + second
        self.assertEqual([data[i:i + 16] for i in uuid_offsets(data)], [b"a" * 16, b"b" * 16])
        with self.assertRaisesRegex(ValueError, "invalid Mach-O slice"):
            uuid_offsets(data[:-1])


if __name__ == "__main__":
    unittest.main()
