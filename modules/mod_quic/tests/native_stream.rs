//! Exercise the actual native broker against real generation-checked pool handles.
use std::{path::PathBuf, process::Command};
#[test]
fn native_stream_ownership_backpressure_and_generation_teardown() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let out = root.join("target/quic-stream-native");
    std::fs::create_dir_all(&out).unwrap();
    // Exercise the production component resolver too: stream-only consumers
    // such as mod_flash have no datagram port or hosted native table.
    let source=std::fs::read_to_string(root.join("components/xip_runtime/src/xip_runtime.c")).unwrap();
    let start=source.find("static struct xip_instance *envelope_target(int component)\n{").unwrap();
    let end=source[start..].find("\nint xip_device_info").unwrap()+start;
    std::fs::write(out.join("envelope_target.inc"),&source[start..end]).unwrap();
    let binary = out.join("stream");
    assert!(Command::new("cc")
        .args([
            "-std=gnu11",
            "-Wall",
            "-Wextra",
            "-Werror",
            "-fsanitize=address,undefined",
            "-g",
            "-I"
        ])
        .arg(&out)
        .arg("-I")
        .arg(root.join("include"))
        .arg("-I")
        .arg(root.join("components/xip_runtime/include"))
        .arg("-I")
        .arg(root.join("components/xip_runtime/src"))
        .arg(root.join("modules/mod_quic/tests/native_stream.c"))
        .arg(root.join("components/xip_runtime/src/xip_pool.c"))
        .arg(root.join("components/xip_runtime/src/xip_envelope.c"))
        .arg("-o")
        .arg(&binary)
        .status()
        .unwrap()
        .success());
    assert!(Command::new(binary).status().unwrap().success());
}

#[test]
fn pending_tcp_output_survives_readable_fin_racing_the_write(){
    let root=PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let out=root.join("target/quic-socket-native");std::fs::create_dir_all(&out).unwrap();
    let source=std::fs::read_to_string(root.join("components/xip_runtime/src/socket_hal.inc")).unwrap();
    let start=source.find("static void socket_delivered(").unwrap();let end=source[start..].find("static void socket_destroy(").unwrap()+start;
    std::fs::write(out.join("socket_delivered.inc"),&source[start..end]).unwrap();let binary=out.join("socket");
    assert!(Command::new("cc").args(["-std=gnu11","-Wall","-Wextra","-Werror","-fsanitize=address,undefined","-g","-I"]).arg(root.join("include"))
        .arg("-I").arg(&out).arg(root.join("modules/mod_quic/tests/native_socket.c")).arg("-o").arg(&binary).status().unwrap().success());
    assert!(Command::new(binary).status().unwrap().success());
}
