//! Native request activation uses the actual serialized resolver and initializer.
use std::{fs, path::PathBuf, process::Command};
#[test]
fn native_activation_initializes_once_resolves_aliases_and_preserves_startup_errors() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let source = fs::read_to_string(root.join("components/xip_runtime/src/xip_runtime.c")).unwrap();
    let a = source
        .find("static struct xip_instance *envelope_target(int component)\n{")
        .unwrap();
    let b = a + source[a..].find("\nint xip_device_info").unwrap();
    let c = source
        .find("static int instance_start_result(struct xip_instance *in)\n{")
        .unwrap();
    let d = c + source[c..].find("\n/* The table of `name`").unwrap();
    let fixture =
        fs::read_to_string(root.join("components/xip_runtime/tests/request_activation.c"))
            .unwrap()
            .replace("/* ACTUAL_RESOLVER */", &source[a..b])
            .replace("/* ACTUAL_INITIALIZER */", &source[c..d]);
    let out = root.join("target/request-activation-native");
    fs::create_dir_all(&out).unwrap();
    let file = out.join("activation.c");
    fs::write(&file, fixture).unwrap();
    let bin = out.join("activation");
    let output = Command::new("cc")
        .args([
            "-std=gnu11",
            "-Wall",
            "-Wextra",
            "-Werror",
            "-fsanitize=address,undefined",
            "-g",
            "-I",
        ])
        .arg(root.join("include"))
        .arg(file)
        .arg("-o")
        .arg(&bin)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(Command::new(bin).status().unwrap().success());
}
