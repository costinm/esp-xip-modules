//! Compile the actual C serial owner against task/driver boundary fakes.
#![cfg(not(target_os = "espidf"))]
use std::{path::PathBuf, process::Command};
#[test]
fn serial_handoff_full_duplex_and_owned_teardown() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let out = root.join("target/serial-native-tests");
    std::fs::create_dir_all(&out).unwrap();
    let source = std::fs::read_to_string(root.join("components/xip_runtime/src/xip_serial.c")).unwrap();
    std::fs::write(out.join("serial_owner.inc"), source.lines().filter(|s| !s.starts_with("#include")).collect::<Vec<_>>().join("\n")).unwrap();
    let header = std::fs::read_to_string(root.join("components/xip_runtime/include/xip_runtime.h")).unwrap();
    let api = header.split("/* Product adapters share").nth(1).unwrap().split("/* Stop the reader").next().unwrap();
    std::fs::write(out.join("serial_api.inc"), format!("/* Product adapters share{api}\nvoid xip_serial_stop(void);\n")).unwrap();
    let power = std::fs::read_to_string(root.join("components/xip_runtime/src/xip_power.c")).unwrap();
    let activity = power.split("/* BEGIN ACTIVITY WINDOW:").nth(1).unwrap().split_once("*/").unwrap().1
        .split("/* END ACTIVITY WINDOW */").next().unwrap();
    std::fs::write(out.join("activity.inc"), activity).unwrap();
    let ppp = std::fs::read_to_string(root.join("components/xip_runtime/src/xip_ppp.c")).unwrap();
    std::fs::write(out.join("ppp_codec.inc"), ppp.lines().filter(|s| !s.starts_with("#include")).collect::<Vec<_>>().join("\n")).unwrap();
    let binary = out.join("driver");
    assert!(Command::new("cc").args(["-std=gnu11", "-Wall", "-Wextra", "-Werror", "-pthread", "-I"]).arg(&out)
        .arg("-I").arg(root.join("include"))
        .arg(root.join("crates/xip-runtime-esp/tests/native/serial/driver.c")).arg("-o").arg(&binary).status().unwrap().success());
    assert!(Command::new(binary).status().unwrap().success());
    let packets = out.join("packets");
    assert!(Command::new("cc").args(["-std=gnu11", "-Wall", "-Wextra", "-Werror", "-Wno-unused-function", "-pthread", "-I"]).arg(&out)
        .arg("-I").arg(root.join("include"))
        .arg(root.join("crates/xip-runtime-esp/tests/native/serial/packets.c")).arg("-o").arg(&packets).status().unwrap().success());
    assert!(Command::new(packets).status().unwrap().success());
}

#[test]
fn heap_snapshot_never_allocates_or_prints_under_allocator_lock() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let out = root.join("target/heap-native-tests");
    std::fs::create_dir_all(&out).unwrap();
    let source = std::fs::read_to_string(root.join("components/xip_runtime/src/xip_events.c")).unwrap();
    let helper = source.split("/* BEGIN HEAP BLOCK SNAPSHOT:").nth(1).unwrap()
        .split_once("*/").unwrap().1.split("/* END HEAP BLOCK SNAPSHOT */").next().unwrap();
    std::fs::write(out.join("heap_snapshot.inc"), helper).unwrap();
    let binary = out.join("heap");
    assert!(Command::new("cc").args(["-std=gnu11", "-Wall", "-Wextra", "-Werror", "-I"]).arg(&out)
        .arg("-I").arg(root.join("include"))
        .arg(root.join("crates/xip-runtime-esp/tests/native/serial/heap.c")).arg("-o").arg(&binary).status().unwrap().success());
    assert!(Command::new(binary).status().unwrap().success());
}

#[test]
fn native_ppp_routes_quic_and_handles_controls_without_pool_allocation() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let out = root.join("target/ppp-native-tests");
    std::fs::create_dir_all(&out).unwrap();
    std::fs::write(out.join("esp_err.h"), "typedef int esp_err_t;\n").unwrap();
    let binary = out.join("ppp");
    assert!(Command::new("cc").args(["-std=gnu11", "-Wall", "-Wextra", "-Werror", "-I"])
        .arg(&out).arg("-I").arg(root.join("components/xip_runtime/include")).arg("-I").arg(root.join("include"))
        .arg(root.join("crates/xip-runtime-esp/tests/native/serial/ppp.c"))
        .arg(root.join("components/xip_runtime/src/xip_ppp.c"))
        .arg("-o").arg(&binary).status().unwrap().success());
    assert!(Command::new(binary).status().unwrap().success());
}
