# Frozen flash compatibility vectors

These existing vectors were moved unchanged from root tests/data. They are
historical compatibility inputs, not images to flash onto current lab boards.
The original test comments identify the frozen loader tools/C journal and IDF
partition format as producers; an exact original generation command/key is not
available in this checkout. No signing private key is included.

Run `cargo test --manifest-path modules/mod_flash/Cargo.toml` for Rust readers,
signed verification/install and interrupted journal/commit checks. Run
`python3 tests/test_tools.py Packages` for independent package/hash parsing.

`partition-4mb.bin` intentionally describes the old 0x380000/512-KiB module
partition. `state.bin` is the 8-KiB C-written journal. The two packages exercise
signed loader/module parsing. These fixtures must not be regenerated merely to
match today's partition table.

| File | SHA-256 |
|---|---|
| loader-esp32c6.pkg | `113011ea364554c62e90c11be1941521279ece6f4c75ad875758215ed1a218d7` |
| module-lora-esp32c6.pkg | `c7b83bec99f69fe268f8cb98e3b78a675285006a33b45bfc398c92fa4f6f9669` |
| partition-4mb.bin | `a5e2c6881b70e35b11b32be9a098ab7c66a621bc3ac8ee48a4c2bb01dae7f64e` |
| state.bin | `b6bb1f7b014981991c38e3bf23d1d793312bd98046444e8988cf5c98f8ed61c0` |
