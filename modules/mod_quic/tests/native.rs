use std::{fs, path::PathBuf, process::Command};
#[test]
fn native_endpoint_owns_exact_packets_and_rejects_stale_epochs() {
    let module = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let root = module.parent().unwrap().parent().unwrap();
    let dir = root.join("target/mod-quic-native-tests");
    fs::create_dir_all(&dir).unwrap();
    let runtime =
        fs::read_to_string(root.join("components/xip_runtime/src/xip_runtime.c")).unwrap();
    let a = runtime.find("static int deliver_checked(").unwrap();
    let b = runtime[a..].find("\nstatic int deliver(").unwrap() + a;
    fs::write(dir.join("runtime_deliver.inc"), &runtime[a..b]).unwrap();
    let bin = dir.join("endpoint");
    let output = Command::new("cc")
        .args([
            "-std=gnu11",
            "-Wall",
            "-Wextra",
            "-Werror",
            "-fsanitize=address,undefined",
            "-g",
        ])
        .arg(format!("-I{}", dir.display()))
        .arg(format!("-I{}", root.join("include").display()))
        .arg(format!(
            "-I{}",
            root.join("components/xip_runtime/include").display()
        ))
        .arg(format!(
            "-I{}",
            root.join("components/xip_runtime/src").display()
        ))
        .arg(module.join("tests/native_endpoint.c"))
        .arg(root.join("components/xip_runtime/src/xip_pool.c"))
        .arg("-o")
        .arg(&bin)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output = Command::new(bin).output().unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
