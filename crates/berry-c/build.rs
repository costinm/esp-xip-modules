// SPDX-License-Identifier: Apache-2.0
// Compile the C module (modules/mod_berry: mod_berry.c, everything in port/, and the vendored
// berry/ minus the libraries port/berry_conf.h switches off) into static archives for this host.
// Same sources and flags as the device build (tools/build_c_mod.py) and as the .so builder that
// went away with main's switch to wasm; the skip list is the device builder's too, the lists are
// kept in step by hand. The host links
// libc, and the formatter is components/xip_runtime/src/xip_fmt.c under its own name, so nothing is interposed.
use std::path::Path;

fn setup(b: &mut cc::Build, root: &Path) {
    let module = root.join("modules/mod_berry");
    b.include(root.join("include"))
        .include(root.join("modules/common"))
        .include(module.join("port"))
        .include(module.join("berry"))
        .opt_level(2)
        .debug(true)
        // test binaries are position-independent executables: the objects must be too.
        .pic(true);
}

fn c_files(dir: &Path, skip: &[&str]) -> Vec<std::path::PathBuf> {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "c") && !skip.contains(&p.file_stem().unwrap().to_str().unwrap()))
        .collect();
    files.sort();
    files
}

fn main() {
    // cc's own change tracking covers only the environment, never the C sources (and headers were
    // never on the list): without these, an edit under modules/ does not rerun this script and the
    // archives keep the previous build. Cargo watches a directory line recursively.
    println!("cargo::rerun-if-changed=../../modules/mod_berry");
    println!("cargo::rerun-if-changed=../../modules/common");
    println!("cargo::rerun-if-changed=../../include");
    println!("cargo::rerun-if-changed=../../components/xip_runtime/src/xip_fmt.c");

    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");

    // Vendored third-party sources compile as they are: warnings are the upstream's business
    // (-Werror is not). The two -Wno flags silence upstream's own known cases: a type-punned float
    // read in the bytes library, and functions guarded by libraries this port switches off.
    let skip = [
        "be_jsonlib",
        "be_mathlib",
        "be_timelib",
        "be_oslib",
        "be_solidifylib",
        "be_introspectlib",
        "be_strictlib",
        "be_debuglib",
        "be_bytecode",
    ];
    let mut vendored = cc::Build::new();
    setup(&mut vendored, &root);
    vendored.std("c99").warnings(false).flag("-Wno-strict-aliasing").flag("-Wno-unused-function");
    for f in c_files(&root.join("modules/mod_berry/berry"), &skip) {
        vendored.file(f);
    }
    vendored.compile("berry_vendored");

    // Module and port code: the repository's own warning discipline.
    let mut port = cc::Build::new();
    setup(&mut port, &root);
    port.std("c11").warnings(true).flag("-Wextra").flag("-Werror");
    port.file(root.join("modules/mod_berry/mod_berry.c"));
    // the runtime's formatter (the `snprintf` of the `native` table on a device): the very file the firmware links
    port.include(root.join("components/xip_runtime/include"));
    port.file(root.join("components/xip_runtime/src/xip_fmt.c"));
    for f in c_files(&root.join("modules/mod_berry/port"), &[]) {
        port.file(f);
    }
    port.compile("berry_port");
}
