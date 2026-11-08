"""Serial markers must follow, and never modify, the native ESP image."""
from pathlib import Path
import struct
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'tools'))
from runtime_image import prepare


class RuntimeImage(unittest.TestCase):
    def image(self, sha):
        data = bytearray(304 + (32 if sha else 0))
        data[0:2] = bytes([0xe9, 1])
        data[23] = sha
        struct.pack_into('<I', data, 28, 256)
        struct.pack_into('<I', data, 32, 0xabcd5432)
        return bytes(data)

    def test_initial_and_recovery_keep_native_bytes_including_sha(self):
        for sha in [0, 1]:
            for marker in [0xfe, 0xfc]:
                image = self.image(sha)
                packed = prepare(image, marker)
                self.assertEqual(packed[:-4], image)
                self.assertEqual(packed[-4:], bytes([marker, 255, 255, 255]))
                self.assertEqual(len(packed) % 4, 0)

    def test_rejects_truncated_already_marked_and_non_runtime_images(self):
        image = self.image(1)
        for invalid in [b'', b'garbage', image[:-1], image + b'\xfc\xff\xff\xff']:
            with self.assertRaises(ValueError):
                prepare(invalid, 0xfe)

    def test_rejects_other_markers(self):
        with self.assertRaises(ValueError):
            prepare(self.image(1), 0xf8)

    def test_front_image_must_leave_room_for_marker(self):
        image = bytearray(0x180000)
        image[0:2] = bytes([0xe9, 1])
        struct.pack_into('<I', image, 28, len(image) - 48)
        struct.pack_into('<I', image, 32, 0xabcd5432)
        with self.assertRaisesRegex(ValueError, 'RESERVE'):
            prepare(bytes(image), 0xfc)
