//! Force an OPEN/readability race against the production native socket owner.
use std::{fs, path::PathBuf, process::Command};
#[test]
fn native_socket_open_rearms_data_that_arrived_before_notification_delivery() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let out = root.join("target/socket-readiness-native-tests");
    fs::create_dir_all(&out).unwrap();
    // Only replace SDK includes; every function under test is production source.
    let src = fs::read_to_string(root.join("components/xip_runtime/src/socket_hal.inc")).unwrap();
    let body = src
        .split("static void socket_destroy")
        .next()
        .unwrap()
        .lines()
        .filter(|l| !l.starts_with("#include"))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(out.join("socket_readiness_runtime.inc"), body).unwrap();
    fs::write(out.join("esp_err.h"), "typedef int esp_err_t;\n").unwrap();
    let bin = out.join("socket-readiness");
    let build = Command::new("cc")
        .args([
            "-std=gnu11",
            "-Wall",
            "-Wextra",
            "-Werror",
            "-fsanitize=address,undefined",
            "-g",
            "-I",
        ])
        .arg(&out)
        .arg("-I")
        .arg(root.join("include"))
        .arg("-I")
        .arg(root.join("components/xip_runtime/include"))
        .arg(root.join("components/xip_runtime/tests/socket_readiness_host.c"))
        .arg("-o")
        .arg(&bin)
        .output()
        .unwrap();
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    let result = Command::new(&bin).output().unwrap();
    assert!(
        result.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}
