//! Native headers must interoperate byte-for-byte with the retained FW codec.
use std::{fs, path::PathBuf, process::Command};
#[test]
fn ethernet_udp6_prefix_matches_fw_and_rejects_invalid_frames() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let out = root.join("target/udp6-frame-native");
    fs::create_dir_all(&out).unwrap();
    let fixture = r#"
#include <assert.h>
#include <stdio.h>
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <string.h>
/* UDP6_CODEC */
int main(void){
    const uint8_t src[6]={0x10,0xbd,0xa3,0xac,0x5a,0x20},dst[6]={2,3,4,5,6,7};
    uint8_t sip[16],dip[16];udp6_local_ip(src,sip);udp6_local_ip(dst,dip);
    for(unsigned n=0;n<=1452;n++){
        uint8_t frame[1518]={0};for(unsigned i=0;i<n;i++)frame[62+i]=(uint8_t)(i*17+n);
        udp6_prefix(frame,n,src,dst,sip,dip,3339,4444);
        assert(udp6_selected(frame,62+n,dip,4444));assert(udp6_valid(frame,62+n));
        assert(udp6_valid(frame,66+n)); /* L2 padding */
        assert(!udp6_selected(frame,62+n,sip,4444));
        assert(!udp6_selected(frame,62+n,dip,3339));
        assert(!udp6_valid(frame,61+n));
        frame[60]^=1;assert(!udp6_valid(frame,62+n));frame[60]^=1;
        frame[60]=frame[61]=0;assert(!udp6_valid(frame,62+n));
        udp6_prefix(frame,n,src,dst,sip,dip,3339,4444);
        assert(fwrite(frame,1,62+n,stdout)==62+n);
    }
    return 0;
}
"#;
    let source =
        fs::read_to_string(root.join("components/xip_runtime/src/xip_quic_udp.c")).unwrap();
    let start = source.find("/* BEGIN UDP6 CODEC:").unwrap();
    let end = source.find("/* END UDP6 CODEC */").unwrap();
    let fixture = fixture.replace("/* UDP6_CODEC */", &source[start..end]);
    let file = out.join("codec.c");
    fs::write(&file, fixture).unwrap();
    let bin = out.join("codec");
    let build = Command::new("cc")
        .args([
            "-std=gnu11",
            "-Wall",
            "-Wextra",
            "-Werror",
            "-fsanitize=address,undefined",
            "-g",
        ])
        .arg("-I")
        .arg(root.join("components/xip_runtime/src"))
        .arg("-I").arg(root.join("include"))
        .arg(file)
        .arg("-o")
        .arg(&bin)
        .output()
        .unwrap();
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    let run = Command::new(bin).output().unwrap();
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    // Golden digest generated with fw/transport/src/raw_udp6.rs::encode_udp6
    // for the same MACs, ports, lengths and body pattern above. No dependency
    // on the local-only retained FW tree in CI.
    let frames = out.join("frames.bin");
    fs::write(&frames, &run.stdout).unwrap();
    let digest = Command::new("sha256sum").arg(frames).output().unwrap();
    assert!(digest.status.success());
    assert_eq!(
        String::from_utf8(digest.stdout)
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap(),
        "76d449a15d42ac2275bb509b2eb98a19f526cd38742aa1555c2812eaa565fd7b"
    );
}

#[test]
fn native_raw_udp6_ownership_backpressure_and_teardown() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let out = root.join("target/udp6-endpoint-native");
    fs::create_dir_all(&out).unwrap();
    let source =
        fs::read_to_string(root.join("components/xip_runtime/src/xip_quic_udp.c")).unwrap();
    let start =
        source.find("#if CONFIG_XIP_QUIC_UDP\n").unwrap() + "#if CONFIG_XIP_QUIC_UDP\n".len();
    let end = source.find("\n#else\n").unwrap();
    // Multicast shares the codec but lives outside CONFIG_XIP_QUIC_UDP.
    // Include its actual registry/raw-ingress/teardown functions as well.
    let mc_state_start = source.find("static const uint8_t XIP_MC_GROUP6").unwrap();
    let mc_state_end = source[mc_state_start..].find("static bool mc_raw_rx").unwrap() + mc_state_start;
    let mc_rx_start = source.find("static bool mc_raw_rx(unsigned interface,const uint8_t *frame,size_t len)\n{").unwrap();
    let mc_rx_end = source[mc_rx_start..].find("\nstatic int udp_reply_fd").unwrap() + mc_rx_start;
    let mc_stop_start = source.find("void xip_mc_stop(void)").unwrap();
    let mc_stop_end = source[mc_stop_start..].find("\nunsigned xip_mc_sockets").unwrap() + mc_stop_start;
    let reply_start = source.find("static int udp_reply_fd(int fallback)\n{").unwrap();
    let reply_end = source[reply_start..].find("\nint h_udp_send").unwrap() + reply_start;
    let bearer = format!("{}\n{}\n{}\n{}\n{}", &source[start..end],
        &source[mc_state_start..mc_state_end], &source[reply_start..reply_end],
        &source[mc_rx_start..mc_rx_end], &source[mc_stop_start..mc_stop_end]);
    let header = fs::read_to_string(root.join("components/xip_runtime/include/xip_runtime.h")).unwrap();
    let address = header.lines().find(|line| line.ends_with("} xip_udp_address;")).unwrap();
    let fixture = fs::read_to_string(root.join("components/xip_runtime/tests/udp6_endpoint.c"))
        .unwrap()
        .replace("/* ACTUAL_UDP_BEARER */", &bearer)
        .replace("/* SOURCE_ADDRESS */", address);
    let file = out.join("endpoint.c");
    fs::write(&file, fixture).unwrap();
    let bin = out.join("endpoint");
    let build = Command::new("cc")
        .args([
            "-std=gnu11",
            "-Wall",
            "-Wextra",
            "-Werror",
            "-fsanitize=address,undefined",
            "-g",
        ])
        .arg("-I")
        .arg(root.join("components/xip_runtime/src"))
        .arg("-I").arg(root.join("include"))
        .arg(file)
        .arg("-o")
        .arg(&bin)
        .output()
        .unwrap();
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    assert!(Command::new(bin).status().unwrap().success());
}

#[test]
fn multicast_startup_before_dhcp_uses_an_up_interface_and_the_host_group() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let source = fs::read_to_string(root.join("components/xip_runtime/src/xip_quic_udp.c")).unwrap();
    let group_start = source.find("static const uint8_t XIP_MC_GROUP6").unwrap();
    let group_end = group_start + source[group_start..].find("};").unwrap() + 2;
    let start = source.find("void xip_mc_ensure(void)").unwrap();
    let end = source[start..].find("\nvoid xip_mc_stop(void)").unwrap() + start;
    let fixture = fs::read_to_string(root.join("components/xip_runtime/tests/mc_startup.c")).unwrap()
        .replace("/* ACTUAL_GROUP */", &source[group_start..group_end])
        .replace("/* ACTUAL_ENSURE */", &source[start..end]);
    let out = root.join("target/mc-startup-native");
    fs::create_dir_all(&out).unwrap();
    let file = out.join("startup.c"); fs::write(&file,fixture).unwrap();
    let bin = out.join("startup");
    let build = Command::new("cc").args(["-std=gnu11","-Wall","-Wextra","-Werror","-fsanitize=address,undefined","-g"])
        .arg(file).arg("-o").arg(&bin).output().unwrap();
    assert!(build.status.success(),"{}",String::from_utf8_lossy(&build.stderr));
    let run = Command::new(bin).output().unwrap();
    assert!(run.status.success(),"{}",String::from_utf8_lossy(&run.stderr));
}
