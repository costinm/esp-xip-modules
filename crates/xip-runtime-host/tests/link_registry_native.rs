//! Compile production C registry/monitor control with small boundary fakes.
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};
fn function(source: &str, signature: &str) -> String {
    let start = source.rfind(signature).unwrap();
    let body = start + source[start..].find('{').unwrap();
    let mut depth = 0;
    for (at, ch) in source[body..].char_indices() {
        if ch == '{' {
            depth += 1
        }
        if ch == '}' {
            depth -= 1;
            if depth == 0 {
                return source[start..=body + at].into();
            }
        }
    }
    panic!("unterminated {signature}")
}
fn compile(root: &Path, name: &str, source: String) {
    let dir = root.join("target/link-lifecycle-native");
    fs::create_dir_all(&dir).unwrap();
    let file = dir.join(format!("{name}.c"));
    let bin = dir.join(name);
    fs::write(&file, source).unwrap();
    let output = Command::new("cc")
        .args([
            "-std=gnu11",
            "-Wall",
            "-Wextra",
            "-Werror",
            "-Wno-unused-function",
            "-Wno-unused-variable",
            "-fsanitize=address,undefined",
            "-g",
            "-I",
        ])
        .arg(root.join("include"))
        .arg(&file)
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
#[test]
fn full_radio_registry_and_last_subscriber_reclaim() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let src = fs::read_to_string(root.join("components/xip_runtime/src/xip_runtime.c")).unwrap();
    let begin = src.find("#define XIP_LINKS_MAX").unwrap();
    let end = src[begin..].find("static uint32_t now_us32").unwrap() + begin;
    let mut code = String::from(
        r#"
#include <assert.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include "xip.h"
#define portENTER_CRITICAL(p) ((void)(p))
#define portEXIT_CRITICAL(p) ((void)(p))
static int registry_lock;
typedef struct xip_link_packet xip_link_packet;
typedef int (*xip_link_tx_fn)(void *, uint64_t, const xip_link_packet *);
struct xip_instance {int component;};
static struct xip_instance *I(void *ctx){return ctx;}
"#,
    );
    code.push_str(&src[begin..end]);
    for sig in [
        "static unsigned link_receivers_locked(",
        "static int c_link_open(",
        "static void subs_remove_locked(",
        "static int c_link_close(",
        "int xip_link_native_register(",
        "void xip_link_native_unregister(",
    ] {
        code.push_str(&function(&src, sig));
    }
    code.push_str(r#"
static int tx(void *ctx,uint64_t dest,const xip_link_packet *p){(void)ctx;(void)dest;(void)p;return 0;}
int main(void){
 const char *names[]={"serial","ble","udp","mc","espnow","wifi-action","promisc","ip","lora","extra1","extra2","extra3"};
 assert(XIP_LINKS_MAX>=12);
 for(int i=0;i<XIP_LINKS_MAX;i++)assert(xip_link_native_register(names[i],1500,0,tx,NULL)>=0);
 assert(xip_link_native_register("overflow",1500,0,tx,NULL)==XIP_ERR_NOMEM);
 for(int i=0;i<XIP_LINKS_MAX;i++)assert(!strcmp(links[i].name,names[i]));
 int addr=XIP_ADDR_LINK+3;xip_link_native_unregister(addr);assert(!links[3].used);
 assert(xip_link_native_register("replacement",1500,0,tx,NULL)==addr);
 struct xip_instance subscriber={.component=42};
 assert(c_link_open(&subscriber,"replacement",42,XIP_LINK_EGRESS)==addr);
 assert(link_receivers_locked(&links[3])==0 && links[3].nsubs==1);
 xip_link_native_unregister(addr);assert(links[3].used && links[3].provider==XIP_LINK_NONE);
 assert(xip_link_native_register("replacement",1500,0,tx,NULL)==addr);
 xip_link_native_unregister(addr);
 assert(!c_link_close(&subscriber,addr));
 assert(!links[3].used);assert(c_link_close(&subscriber,addr)==XIP_ERR_NOTFOUND);
 assert(xip_link_native_register("ip-reopen",1500,0,tx,NULL)==addr);
 return 0;
}
"#);
    compile(&root, "registry", code);
}
#[test]
fn promiscuous_disable_and_failed_start_release_registration() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let src =
        fs::read_to_string(root.join("components/xip_runtime/src/xip_link_promisc.c")).unwrap();
    let mut code = String::from(
        r#"
#include <assert.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdatomic.h>
#include "xip.h"
#define ESP_OK 0
#define LINK_MTU XIP_PKT_MAX
static int link_addr=-1, registered, releases, fail_enable, fail_callback;
static atomic_bool on;
static atomic_uint readers;
static int no_tx;
static void rx_cb(void){}
static int esp_wifi_set_promiscuous(bool enable){return enable&&fail_enable?-1:0;}
static int esp_wifi_set_promiscuous_rx_cb(void (*cb)(void)){(void)cb;return fail_callback?-1:0;}
static bool xip_wifi_associated(void){return false;}
static bool xip_wifi_set_channel(unsigned c){return c==6;}
static void xip_link_native_rx_prepare(void){}
static int xip_link_native_register(const char*n,unsigned mtu,unsigned flags,int cb,void*ctx){(void)n;(void)mtu;(void)flags;(void)cb;(void)ctx;assert(!registered);return registered=123;}
static int xip_link_native_set_state(int addr,unsigned state,unsigned rate){(void)state;(void)rate;assert(addr==registered);return 0;}
static void xip_link_native_unregister(int addr){assert(!atomic_load(&readers));assert(addr==registered);registered=0;releases++;}
static void vTaskDelay(unsigned n){(void)n;assert(atomic_load(&readers));atomic_fetch_sub(&readers,1);}
"#,
    );
    for sig in [
        "static void release_link(",
        "int xip_link_promisc_set(",
        "void xip_link_promisc_stop(",
    ] {
        code.push_str(&function(&src, sig));
    }
    code.push_str(r#"
int main(void){
 xip_wifi_monitor m={.size=sizeof(m),.enable=1,.channel=6};
 assert(!xip_link_promisc_set(&m));assert(registered && atomic_load(&on));
 atomic_store(&readers,1);m.enable=0;assert(!xip_link_promisc_set(&m));assert(!registered && link_addr==-1 && releases==1);
 m.enable=1;assert(!xip_link_promisc_set(&m));xip_link_promisc_stop();assert(!registered && releases==2);
 fail_enable=1;assert(xip_link_promisc_set(&m)==XIP_ERR_IO);assert(!registered && !atomic_load(&on));fail_enable=0;
 fail_callback=1;assert(xip_link_promisc_set(&m)==XIP_ERR_IO);assert(!registered);fail_callback=0;
 assert(!xip_link_promisc_set(&m));xip_link_promisc_stop();assert(!registered);
 xip_link_promisc_stop();assert(!registered);return 0;
}
"#);
    compile(&root, "promisc", code);
}
