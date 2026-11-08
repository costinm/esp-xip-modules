//! Callback admission must use prepared storage, within the idle RAM policy.
use std::{fs, path::PathBuf, process::Command};
#[test]
fn owner_prepares_receive_storage_and_reclaims_the_transient_burst_cache() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let out = root.join("target/packet-pool-native");
    fs::create_dir_all(&out).unwrap();
    let source = r#"
#include <assert.h>
#include "xip_pool.h"
int main(void){
    xip_pool *p=xip_pool_create(16,1704);assert(p);
    assert(xip_pool_acquire_cached(p,0)<0 && xip_pool_allocated(p)==0);
    xip_pool_set_policy(p,4,0,0);xip_pool_prepare_rx(p);
    assert(xip_pool_allocated(p)==4);
    int held[4];for(unsigned i=0;i<4;i++){held[i]=xip_pool_acquire_cached(p,0);assert(held[i]>=0);}
    assert(xip_pool_acquire_cached(p,0)<0 && xip_pool_allocated(p)==4);
    for(unsigned i=0;i<4;i++)xip_pool_release(p,held[i]);
    xip_pool_set_policy(p,2,0,0);xip_pool_trim(p);assert(xip_pool_allocated(p)==2);
    xip_pool_prepare_rx(p);assert(xip_pool_allocated(p)==2);
    xip_pool_destroy(p);
    p=xip_pool_create(64,1704);assert(p);
    xip_pool_set_policy(p,32,0,0);xip_pool_prepare_rx(p);
    assert(xip_pool_cached_available(p)==32 && xip_pool_allocated(p)==32);
    int burst[32];for(unsigned i=0;i<32;i++){burst[i]=xip_pool_acquire_cached(p,0);assert(burst[i]>=0);}
    assert(xip_pool_acquire_cached(p,0)<0);
    for(unsigned i=0;i<32;i++)xip_pool_release(p,burst[i]);
    xip_pool_set_keep(p,4);xip_pool_trim(p);
    assert(xip_pool_cached_available(p)==4 && xip_pool_allocated(p)==4);
    xip_pool_destroy(p);return 0;
}
"#;
    let file = out.join("pool.c");
    fs::write(&file, source).unwrap();
    let bin = out.join("pool");
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
        .arg(root.join("components/xip_runtime/include"))
        .arg(file)
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
    assert!(Command::new(bin).status().unwrap().success());
}
