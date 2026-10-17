# mod_con

The text console in Rust, with the features of `mod_console` (C): the same API (`API.md`, component 20: `parse`, `render`, and the lines of the serial port), the same
commands (`status`, `start`, `stop`, `events`, `reset`, `heap`, `nv`, `wifi`, `help`) and the same `component.method key=value` calls: a request to a module by the catalog,
or a native call through the table of a host component (`wifi.set`, `power.sleep`, ...). It runs in the same slot (`console`): install one or the other.

`src/tables.rs` is the catalog and the native layouts as Rust constants, generated from the C headers of `mod_console` by `tools/catalog_rs.py` (`scripts/gen.sh`).
`API.md` is a copy of `mod_console`'s (the catalog is still generated from that one); when `mod_console` goes, this becomes the only one.

The host-runtime runs the same cases against both: `XIP_CONSOLE=mod_con cargo test --test console` (`scripts/test-host.sh`).

Things a Rust module must avoid, found while porting: a `match` or `const` array that returns `&str`/slices becomes a table of pointers (`.data.rel.ro`, a relocation: the
image is refused), and so does a value built from literals that rustc promotes to a static (`Parsed::Local("...")`). Call per arm, or build the value at run time.
