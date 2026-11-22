use std::hint::black_box;
use std::time::Instant;

use aes_gcm::{Aes128Gcm, KeyInit as _, Nonce, Tag, aead::AeadInPlace};
use xip_psa::{ALG_GCM, aead_open_in_place, aead_seal_in_place};

#[cfg(target_os = "linux")]
#[repr(C)]
struct Timespec {
    tv_sec: i64,
    tv_nsec: i64,
}

#[cfg(target_os = "linux")]
fn thread_cpu_ns() -> u64 {
    const CLOCK_THREAD_CPUTIME_ID: i32 = 3;
    unsafe extern "C" {
        fn clock_gettime(clk_id: i32, tp: *mut Timespec) -> i32;
    }
    let mut ts = Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let rc = unsafe { clock_gettime(CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    if rc == 0 {
        (ts.tv_sec as u64) * 1_000_000_000 + (ts.tv_nsec as u64)
    } else {
        0
    }
}

#[test]
#[ignore]
fn bench_xip_psa_aead_seal_open() {
    let key = [0x42u8; 16];
    let mut nonce = [0x24u8; 12];
    let aad = [0x01u8; 11];
    let cached = Aes128Gcm::new_from_slice(&key).unwrap();

    println!("=== xip-psa AES-128-GCM benchmark (host x86_64 release) ===");
    println!(
        "{:<8} {:<26} {:>10} {:>10} {:>10} {:>10}",
        "Size", "Mode", "Wall ns/op", "CPU ns/op", "MiB/s", "Mbps"
    );

    for &sz in &[64usize, 256, 1024, 1200, 1400] {
        let iters = match sz {
            0..=256 => 100_000usize,
            257..=1200 => 40_000,
            _ => 25_000,
        };
        let mut buf = vec![0x5au8; sz];
        let mut cipher = vec![0x5au8; sz];
        nonce[8..12].copy_from_slice(&0u32.to_be_bytes());
        let fixed_tag = aead_seal_in_place(ALG_GCM, &key, &nonce, &aad, &mut cipher).unwrap();
        let mut scratch = vec![0u8; sz];

        // Warmup
        for _ in 0..500 {
            let tag = aead_seal_in_place(ALG_GCM, &key, &nonce, &aad, &mut buf).unwrap();
            assert!(aead_open_in_place(ALG_GCM, &key, &nonce, &aad, &mut buf, &tag));
        }

        let c0 = thread_cpu_ns();
        let t0 = Instant::now();
        for i in 0..iters {
            nonce[8..12].copy_from_slice(&(i as u32).to_be_bytes());
            let tag = aead_seal_in_place(ALG_GCM, &key, &nonce, &aad, black_box(&mut buf)).unwrap();
            black_box(tag);
        }
        let u_seal_w = t0.elapsed().as_nanos() as f64 / iters as f64;
        let u_seal_c = thread_cpu_ns().saturating_sub(c0) as f64 / iters as f64;

        nonce[8..12].copy_from_slice(&0u32.to_be_bytes());
        let c0 = thread_cpu_ns();
        let t0 = Instant::now();
        for _ in 0..iters {
            scratch.copy_from_slice(&cipher);
            assert!(aead_open_in_place(
                ALG_GCM,
                &key,
                &nonce,
                &aad,
                black_box(&mut scratch),
                &fixed_tag
            ));
        }
        let u_open_w = t0.elapsed().as_nanos() as f64 / iters as f64;
        let u_open_c = thread_cpu_ns().saturating_sub(c0) as f64 / iters as f64;

        let c0 = thread_cpu_ns();
        let t0 = Instant::now();
        for i in 0..iters {
            nonce[8..12].copy_from_slice(&(i as u32).to_be_bytes());
            let tag = cached
                .encrypt_in_place_detached(Nonce::from_slice(&nonce), &aad, black_box(&mut buf))
                .unwrap();
            black_box(tag);
        }
        let c_seal_w = t0.elapsed().as_nanos() as f64 / iters as f64;
        let c_seal_c = thread_cpu_ns().saturating_sub(c0) as f64 / iters as f64;

        nonce[8..12].copy_from_slice(&0u32.to_be_bytes());
        let c0 = thread_cpu_ns();
        let t0 = Instant::now();
        for _ in 0..iters {
            scratch.copy_from_slice(&cipher);
            cached
                .decrypt_in_place_detached(
                    Nonce::from_slice(&nonce),
                    &aad,
                    black_box(&mut scratch),
                    Tag::from_slice(&fixed_tag),
                )
                .unwrap();
        }
        let c_open_w = t0.elapsed().as_nanos() as f64 / iters as f64;
        let c_open_c = thread_cpu_ns().saturating_sub(c0) as f64 / iters as f64;

        for (label, w, c) in [
            ("xip_psa_seal (uncached)", u_seal_w, u_seal_c),
            ("xip_psa_open (uncached)", u_open_w, u_open_c),
            ("cached_aes_gcm_seal", c_seal_w, c_seal_c),
            ("cached_aes_gcm_open", c_open_w, c_open_c),
        ] {
            let mibs = (sz as f64 * 1e9) / (w * 1024.0 * 1024.0);
            let mbps = (sz as f64 * 8.0 * 1e9) / (w * 1e6);
            println!(
                "{:<8} {:<26} {:>10.1} {:>10.1} {:>10.1} {:>10.1}",
                sz, label, w, c, mibs, mbps
            );
        }
    }
}
