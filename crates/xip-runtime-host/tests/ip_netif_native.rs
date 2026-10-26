//! Compile the actual C IP link with a deterministic stack and the real pool.
use std::{fs, path::PathBuf, process::Command};
#[test]
fn virtual_ip_preserves_pool_ownership_and_tears_down_without_module_pointers() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let out = root.join("target/ip-netif-native-tests");
    fs::create_dir_all(&out).unwrap();
    // Only replace SDK includes; every function under test is production source.
    let src = fs::read_to_string(root.join("components/xip_runtime/src/xip_ip_netif.c")).unwrap();
    let body = src
        .lines()
        .filter(|l| !l.starts_with("#include"))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(out.join("ip_netif_runtime.inc"), body).unwrap();
    fs::write(out.join("esp_err.h"), "typedef int esp_err_t;\n").unwrap();
    let bin = out.join("ip-netif");
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
        .arg(root.join("components/xip_runtime/tests/ip_netif_host.c"))
        .arg(root.join("components/xip_runtime/src/xip_pool.c"))
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
