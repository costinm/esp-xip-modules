"""Maintained layout checks; no device, SDK or local root tests/ fixtures."""
import hashlib
import struct
import tempfile
import unittest
from pathlib import Path
from partition_layout import read_layout, validate_modules, validate_image, LOADER_STATE_BYTES


class LayoutTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)

    def table(self, main_size=0x190000):
        data_base = 0xf0000 + main_size
        entries = [('loader', 0x10000, 0xe0000),
                   ('main', 0xf0000, main_size),
                   ('data', data_base, 0x400000-data_base)]
        blob = b''.join(struct.pack('<HBBII16sI', 0x50aa, 0, 0, off,
                                   size, name.encode(), 0)
                        for name, off, size in entries)
        blob += b'\xeb\xeb' + b'\xff'*14 + hashlib.md5(blob).digest()
        p = self.root/'table.bin'
        p.write_bytes(blob + b'\xff'*32)
        return p

    def test_new_and_installed_old_geometry(self):
        self.assertEqual(read_layout(self.table())['data'], (0x280000, 0x180000))
        self.assertEqual(read_layout(self.table(0x290000))['data'], (0x380000, 0x80000))

    def test_corruption_rejected(self):
        p = self.table()
        blob = bytearray(p.read_bytes()); blob[40] ^= 1; p.write_bytes(blob)
        with self.assertRaisesRegex(ValueError, 'MD5'):
            read_layout(p)

    def test_overlapping_partitions_rejected(self):
        p = self.root/'table.csv'
        p.write_text('loader,app,0,0x10000,0xe0000\nmain,app,0,0xe0000,0x190000\n')
        with self.assertRaisesRegex(ValueError, 'overlapping'):
            read_layout(p)

    def test_recovery_image_leaves_journal_untouched(self):
        layout = read_layout(self.table())
        image = self.root/'loader.bin'
        capacity = layout['loader'][1] - LOADER_STATE_BYTES
        image.write_bytes(bytes(capacity))
        validate_image(layout, 'loader', image, LOADER_STATE_BYTES)
        image.write_bytes(bytes(capacity + 1))
        with self.assertRaisesRegex(ValueError, 'loader image exceeds'):
            validate_image(layout, 'loader', image, LOADER_STATE_BYTES)

    def test_module_rounded_extent_and_capacity(self):
        layout = read_layout(self.table())
        listing = self.root/'modules.txt'
        (self.root/'quic.dmod').write_bytes(bytes(231464))
        (self.root/'other.dmod').write_bytes(bytes(100))
        listing.write_text('quic 0x140000 9\n')
        validate_modules(layout, listing, self.root)  # four pages at end
        listing.write_text('quic 0x150000 9\n')
        with self.assertRaisesRegex(ValueError, 'does not fit'):
            validate_modules(layout, listing, self.root)
        listing.write_text('quic 0 9\nother 0x30000 13\n')
        with self.assertRaisesRegex(ValueError, 'overlapping'):
            validate_modules(layout, listing, self.root)


if __name__ == '__main__':
    unittest.main()
