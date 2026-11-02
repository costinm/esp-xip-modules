// SPDX-License-Identifier: Apache-2.0
// Compile the C module modules/mod_wasm3 (mod_wasm3.c, port/, and the vendored wasm3/) into static archives for this host:
// the same sources and flags as the device build (tools/build_c_mod.py), with the host's libc. The wasm3 configuration is the
// trimmed one of modules/mod_wasm3/UPSTREAM.md (no WASI, no threads, no optional proposals, no .data/.bss in the device image).
use std::path::Path;

/// wasm3 as the module configures it; tools/build_c_mod.py keeps the same list.
pub const W3_DEFS: &[&str] = &[
    "d_m3HasPosixHost=0", "M3_THREAD_LOCAL=", "M3_HAS_THREAD_LOCAL=0", "d_m3HasAtomics=0", "d_m3HasExceptionHandling=0",
    "d_m3HasSnapshots=0", "d_m3HasStackSwitching=0", "d_m3HasTypedRefs=0", "d_m3HasMemory64=0", "d_m3HasMultiMemory=0",
    "d_m3HasWideArithmetic=0", "d_m3HasExtendedConst=0", "d_m3HasCompactImports=0", "d_m3HasRefTypes=0", "d_m3HasGasMetering=0",
    "d_m3GuardedMemory=0", "d_m3VerboseErrorMessages=0", "d_m3MaxFunctionStackHeight=128", "d_m3MaxConstantTableSize=64",
    "d_m3CodePageAlignSize=1024", "d_m3MaxNativeStack=5120", "XIP_WASM3_ALLOC",
];

fn setup(b: &mut cc::Build, root: &Path) {
    let module = root.join("modules/mod_wasm3");
    b.include(root.join("include"))
        .include(root.join("modules/common"))
        .include(module.join("port"))
        .include(module.join("wasm3"))
        .opt_level(2)
        .debug(true)
        .pic(true);
    for d in W3_DEFS {
        match d.split_once('=') {
            Some((k, v)) => b.define(k, if v.is_empty() { None } else { Some(v) }),
            None => b.define(d, None),
        };
    }
}

fn main() {
    println!("cargo::rerun-if-changed=../../modules/mod_wasm3");
    println!("cargo::rerun-if-changed=../../modules/common");
    println!("cargo::rerun-if-changed=../../include");
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");

    // Vendored third-party sources compile as they are (upstream's warnings are upstream's business).
    let mut vendored = cc::Build::new();
    setup(&mut vendored, &root);
    vendored.std("c11").warnings(false);
    for f in ["bind", "code", "compile", "core", "env", "exec", "function", "module", "parse", "validate"] {
        vendored.file(root.join(format!("modules/mod_wasm3/wasm3/m3_{f}.c")));
    }
    vendored.compile("wasm3_vendored");

    // Module and port code: the repository's own warning discipline.
    let mut port = cc::Build::new();
    setup(&mut port, &root);
    port.std("c11").warnings(true).flag("-Wextra").flag("-Werror");
    port.file(root.join("modules/mod_wasm3/mod_wasm3.c"));
    port.file(root.join("modules/mod_wasm3/port/xip_wasm3.c"));
    port.compile("wasm3_port");
}
