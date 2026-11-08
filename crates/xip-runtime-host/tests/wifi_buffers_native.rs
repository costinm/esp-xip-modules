//! Init-time buffer changes must restart the C-owned Wi-Fi driver.
use std::{fs, path::PathBuf, process::Command};

#[test]
fn wifi_driver_policy_tracks_effective_buffer_sizes_and_ba_window() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let source = fs::read_to_string(root.join("components/xip_runtime/src/xip_wifi.c")).unwrap();
    let policy = source.find("typedef struct wifi_driver_policy {").unwrap();
    let policy_end = source[policy..]
        .find("\nstatic wifi_driver_policy driver_policy")
        .unwrap()
        + policy;
    let start = source.find("static bool driver_policy_changed(").unwrap();
    let end = source[start..].find("\n/* esp_wifi_init once").unwrap() + start;
    let out = root.join("target/wifi-buffers-native");
    fs::create_dir_all(&out).unwrap();
    let fixture = format!(
        r#"
#include <assert.h>
#include <stdbool.h>
#include <stdint.h>
typedef struct {{int static_rx_buf_num,dynamic_rx_buf_num,dynamic_tx_buf_num,rx_ba_win;}} wifi_init_config_t;
typedef struct {{int unused;}} xip_wifi_tuning;
static bool driver_initialized;
{policy}
static wifi_driver_policy driver_policy;
static wifi_init_config_t effective;
static wifi_init_config_t init_config(const xip_wifi_tuning *t){{(void)t;return effective;}}
{functions}
int main(void){{
    xip_wifi_tuning tuning={{0}};
    wifi_init_config_t conservative={{10,32,32,6}};
    effective=conservative;
    remember_driver_policy(true,true,&conservative);
    driver_initialized=true;
    assert(!driver_policy_changed(true,true,&tuning));
    assert(driver_policy_changed(false,true,&tuning));
    assert(driver_policy_changed(true,false,&tuning));
    for(unsigned field=0;field<4;field++){{
        effective=conservative;
        switch(field){{
        case 0:effective.static_rx_buf_num=40;break;
        case 1:effective.dynamic_rx_buf_num=60;break;
        case 2:effective.dynamic_tx_buf_num=40;break;
        case 3:effective.rx_ba_win=32;break;
        }}
        assert(driver_policy_changed(true,true,&tuning));
        remember_driver_policy(true,true,&effective);
        assert(!driver_policy_changed(true,true,&tuning));
        effective=conservative;
        assert(driver_policy_changed(true,true,&tuning));
        remember_driver_policy(true,true,&effective);
    }}
    effective=(wifi_init_config_t){{40,60,40,32}};
    driver_initialized=false;
    assert(!driver_policy_changed(true,true,&tuning));
    remember_driver_policy(true,true,&effective);driver_initialized=true;
    assert(!driver_policy_changed(true,true,&tuning));
    effective=conservative;assert(driver_policy_changed(true,true,&tuning));
    return 0;
}}
"#,
        policy = &source[policy..policy_end],
        functions = &source[start..end]
    );
    let file = out.join("policy.c");
    fs::write(&file, fixture).unwrap();
    let binary = out.join("policy");
    let build = Command::new("cc")
        .args([
            "-std=gnu11",
            "-Wall",
            "-Wextra",
            "-Werror",
            "-fsanitize=address,undefined",
            "-g",
        ])
        .arg(file)
        .arg("-o")
        .arg(&binary)
        .output()
        .unwrap();
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    assert!(Command::new(binary).status().unwrap().success());
}

#[test]
fn wifi_event_owner_follows_recreated_sdk_handlers_without_duplicates() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let source = fs::read_to_string(root.join("components/xip_runtime/src/xip_wifi.c")).unwrap();
    let start = source
        .find("static bool register_event_handlers(void)")
        .unwrap();
    let end = start + source[start..].find("\n/* The platform setup").unwrap();
    let out = root.join("target/wifi-events-native");
    fs::create_dir_all(&out).unwrap();
    let fixture = format!(
        r#"
#include <assert.h>
#include <stdbool.h>
#include <stddef.h>
#define WIFI_EVENT 1
#define IP_EVENT 2
#define ESP_EVENT_ANY_ID -1
#define IP_EVENT_STA_GOT_IP 3
#define ESP_OK 0
static bool handlers_registered,fail_ip;
static unsigned clock,default_order,wifi_order,ip_order,wifi_count,ip_count;
static void on_wifi_event(void){{}}
static int esp_event_handler_register(int base,int event,void (*fn)(void),void *arg){{
    (void)event;(void)fn;(void)arg;
    if(base==IP_EVENT && fail_ip)return -1;
    if(base==WIFI_EVENT){{wifi_count++;wifi_order=++clock;}}
    else {{ip_count++;ip_order=++clock;}}
    return 0;
}}
static int esp_event_handler_unregister(int base,int event,void (*fn)(void)){{
    (void)event;(void)fn;
    if(base==WIFI_EVENT){{assert(wifi_count==1);wifi_count--;}}
    else {{assert(ip_count==1);ip_count--;}}
    return 0;
}}
{functions}
int main(void){{
    for(unsigned epoch=0;epoch<5;epoch++){{
        default_order=++clock; /* IDF netif/glue created again. */
        assert(register_event_handlers());
        assert(wifi_count==1 && ip_count==1);
        assert(wifi_order>default_order && ip_order>default_order);
    }}
    fail_ip=true;assert(!register_event_handlers());
    assert(!handlers_registered && wifi_count==0 && ip_count==0);
    fail_ip=false;assert(register_event_handlers());
    assert(wifi_count==1 && ip_count==1);
    return 0;
}}
"#,
        functions = &source[start..end]
    );
    let file = out.join("events.c");
    fs::write(&file, fixture).unwrap();
    let bin = out.join("events");
    let build = Command::new("cc")
        .args([
            "-std=gnu11",
            "-Wall",
            "-Wextra",
            "-Werror",
            "-fsanitize=address,undefined",
            "-g",
        ])
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
fn wifi_ethernet_callback_transfers_sdk_allocation_to_exactly_one_owner() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let source = fs::read_to_string(root.join("components/xip_runtime/src/xip_wifi.c")).unwrap();
    let start = source.find("static esp_err_t wifi_eth_rx(").unwrap();
    let end = start + source[start..].find("\nstatic void on_wifi_event").unwrap();
    let out = root.join("target/wifi-rx-owner-native");
    fs::create_dir_all(&out).unwrap();
    let fixture = format!(
        r#"
#include <assert.h>
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
typedef int esp_err_t;
typedef struct {{int driver;}} esp_netif_t;
#define ESP_OK 0
#define WIFI_IF_STA 0
static bool claimed;
static unsigned freed,forwarded,registered;
static uint8_t frame[80];static int allocation;static esp_netif_t netif={{3}};
static bool xip_quic_udp_raw_rx(unsigned interface,const uint8_t *p,size_t n){{
    assert(interface==0 && p==frame && n==sizeof(frame));return claimed;
}}
static void esp_wifi_internal_free_rx_buffer(void *eb){{assert(eb==&allocation);freed++;}}
static int esp_netif_receive(void *n,void *p,size_t len,void *eb){{
    assert(n==&netif && p==frame && len==sizeof(frame) && eb==&allocation);forwarded++;return -123;
}}
static void *esp_netif_get_io_driver(esp_netif_t *n){{assert(n==&netif);return &n->driver;}}
static int esp_wifi_register_if_rxcb(void *driver,int (*fn)(esp_netif_t *,void *,size_t,void *),void *arg){{
    assert(driver==&netif.driver && arg==&netif);registered++;
    assert(fn(&netif,frame,sizeof(frame),&allocation)==(claimed?0:-123));return 0;
}}
{functions}
int main(void){{
    wifi_eth_rx_install(&netif);assert(forwarded==1 && freed==0);
    claimed=true;wifi_eth_rx_install(&netif);assert(forwarded==1 && freed==1);
    wifi_eth_rx_install(NULL);assert(registered==2);return 0;
}}
"#,
        functions = &source[start..end]
    );
    let file = out.join("owner.c");
    fs::write(&file, fixture).unwrap();
    let bin = out.join("owner");
    let build = Command::new("cc")
        .args([
            "-std=gnu11",
            "-Wall",
            "-Wextra",
            "-Werror",
            "-fsanitize=address,undefined",
            "-g",
        ])
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
