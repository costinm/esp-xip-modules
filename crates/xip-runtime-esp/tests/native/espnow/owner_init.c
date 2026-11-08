/* Verify the actual Rust-facing C initialization selects one radio owner. */
#include <assert.h>
#include "xip_runtime.h"
static unsigned installed;
static xip_host_hooks_t captured;
static int owner_set(const xip_wifi_intent *intent,uint32_t *gen) { (void)intent; (void)gen; return 0; }
static int owner_get(xip_wifi_intent *intent,uint32_t *gen) { (void)intent; (void)gen; return 0; }
static void owner_reconcile(void) {}
static int nv(const char *key,size_t key_len,const uint8_t *value,size_t value_len,uint8_t *out,size_t cap) {
    (void)key; (void)key_len; (void)value; (void)value_len; (void)out; (void)cap; return 0;
}
void xip_wifi_install_hooks(xip_host_hooks_t *hooks) {
    installed++; hooks->wifi_reconcile=owner_reconcile;
    if(!hooks->nv)hooks->nv=nv;
}
esp_err_t xip_host_init(const xip_host_hooks_t *hooks) { captured=*hooks; return 17; }
#include "wifi_install_simple.inc"
#include "runtime_init.inc"
void test_owner_init(void) {
    xip_host_ext_t ext={0};
    xip_wifi_install_simple(&ext);
    assert(xip_host_init_simple(&ext)==17 && installed==1 && captured.wifi_reconcile==owner_reconcile);
    ext=(xip_host_ext_t){.nv=nv};
    xip_wifi_install_simple(&ext);
    assert(xip_host_init_simple(&ext)==17 && installed==2 && captured.nv==nv);
    // Product startup supplies callbacks without invoking the C installer.
    ext=(xip_host_ext_t){.wifi_set=owner_set,.wifi_get=owner_get,.nv=nv};
    assert(xip_host_init_simple(&ext)==17 && installed==2);
    assert(captured.wifi_set==owner_set && captured.wifi_get==owner_get);
    assert(!captured.wifi_reconcile && !captured.wifi_state && !captured.wifi_scan && !captured.wifi_monitor);
}

/* Exercise the shared C store, replacing the removed Rust-only store tests. */
#include <string.h>
#include "freertos/FreeRTOS.h"
typedef int nvs_handle_t;
#define NVS_READONLY 0
static bool saved_settings, legacy_settings;
static unsigned changed_count;
static uint32_t changed_generation;
void xip_host_wifi_changed(uint32_t generation) { changed_count++;changed_generation=generation; }
static int nvs_open(const char *name,int mode,nvs_handle_t *handle) {
    (void)mode;
    if(!strcmp(name,"wl")){if(!legacy_settings)return -1;*handle=3;return ESP_OK;}
    if(!saved_settings)return -1;
    *handle=!strcmp(name,"dmesh")?1:2;return ESP_OK;
}
static int nvs_get_str(nvs_handle_t handle,const char *key,char *out,size_t *size) {
    assert((handle==1 && !strcmp(key,"sta_ssid")) || (handle==2 && !strcmp(key,"sta")) ||
           (handle==3 && (!strcmp(key,"ssid") || !strcmp(key,"psk"))));
    const char *value=handle==3 ? (!strcmp(key,"ssid")?"legacy":"legacy-test-pass") : handle==1?"home":"test-only-pass";
    size_t n=strlen(value)+1;assert(*size>=n);memcpy(out,value,n);*size=n;return ESP_OK;
}
static void nvs_close(nvs_handle_t handle) { (void)handle; }
#include "wifi_intent.inc"
static int owner_state(xip_wifi_state *state) { (void)state;return 0; }
void test_c_wifi_intent(void) {
    xip_host_hooks_t hooks={0};
    xip_wifi_intent_install(&hooks);
    xip_wifi_intent intent={.size=sizeof(intent)}, back={.size=sizeof(back)};
    uint32_t gen=99;
    assert(hooks.wifi_get(&back,&gen)==0 && gen==0 && !back.radio);
    intent.radio=1;intent.sta=1;intent.sta_ssid_len=4;memcpy(intent.sta_ssid,"home",4);
    assert(hooks.wifi_set(&intent,&gen)==0 && gen==1 && changed_count==1);
    intent.reserved[0]=7;
    assert(hooks.wifi_set(&intent,&gen)==0 && gen==1 && changed_count==1);
    intent.channel=15;
    assert(hooks.wifi_set(&intent,&gen)==XIP_ERR_ARG && changed_count==1);
    intent.channel=0;intent.size--;
    assert(hooks.wifi_set(&intent,&gen)==XIP_ERR_ARG);
    assert(hooks.wifi_get(&back,&gen)==0 && gen==1 && !memcmp(back.sta_ssid,"home",4));
    xip_wifi_tuning t={.size=sizeof(t),.ampdu=XIP_TUNE_OFF,.rx_static=12,.lr=XIP_TUNE_ON};
    assert(hooks.wifi_tuning_set(&t)==0 && changed_count==2 && changed_generation==2);
    xip_wifi_tuning got={.size=sizeof(got)};
    assert(hooks.wifi_tuning_get(&got)==0 && got.ampdu==XIP_TUNE_OFF && got.rx_static==12 && got.lr==XIP_TUNE_ON);
    t.raw_rx=XIP_TUNE_ON;
    assert(hooks.wifi_tuning_set(&t)==0 && changed_count==3 && changed_generation==3);
    assert(hooks.wifi_tuning_get(&got)==0 && got.raw_rx==XIP_TUNE_ON);
    t.raw_rx=XIP_TUNE_OFF;
    assert(hooks.wifi_tuning_set(&t)==XIP_ERR_ARG && changed_count==3);
    t.raw_rx=XIP_TUNE_DEFAULT;t.driver_tx=XIP_TUNE_ON;
    assert(hooks.wifi_tuning_set(&t)==XIP_ERR_UNSUPPORTED && changed_count==3);
    // Generic Rust adopts C's saved-credentials behavior, including NOW.
    desired=(xip_wifi_intent){0};generation=0;saved_settings=true;
    hooks=(xip_host_hooks_t){.wifi_state=owner_state};
    xip_wifi_intent_install(&hooks);
    assert(hooks.wifi_get(&back,&gen)==0 && back.radio && back.sta && back.now);
    assert(back.sta_ssid_len==4 && !memcmp(back.sta_ssid,"home",4));
    hooks=(xip_host_hooks_t){.wifi_set=owner_set,.wifi_get=owner_get};
    xip_wifi_intent_install(&hooks);
    assert(hooks.wifi_set==owner_set && hooks.wifi_get==owner_get);
}

static unsigned migrated;
int xip_wifi_get_intent(xip_wifi_intent *intent,uint32_t *gen) { return get(intent,gen); }
int xip_wifi_set_intent(const xip_wifi_intent *intent,uint32_t *gen) { return set(intent,gen); }
int xip_wifi_nv(const char *key,size_t key_len,const uint8_t *value,size_t len,uint8_t *out,size_t cap) {
    assert(!out && !cap);
    if(key_len==8 && !memcmp(key,"sta_ssid",8))assert(len==6 && !memcmp(value,"legacy",6));
    else {assert(key_len==7 && !memcmp(key,"sec:sta",7));assert(len==16 && !memcmp(value,"legacy-test-pass",16));}
    migrated++;return 0;
}
#include "wifi_boot_network.inc"
void test_boot_network(void) {
    // An existing shared boot intent wins over legacy loader credentials.
    legacy_settings=true;
    xip_wifi_boot_network();
    assert(desired.sta_ssid_len==4 && !memcmp(desired.sta_ssid,"home",4) && !migrated);
    desired=(xip_wifi_intent){.size=sizeof(desired)};generation=0;
    xip_wifi_boot_network();
    assert(desired.radio && desired.sta && desired.now && migrated==2);
    assert(desired.sta_ssid_len==6 && !memcmp(desired.sta_ssid,"legacy",6));
    assert(desired.sta_psk_len==16 && !memcmp(desired.sta_psk,"legacy-test-pass",16));
    desired=(xip_wifi_intent){.size=sizeof(desired)};legacy_settings=false;
    xip_wifi_boot_network();
    assert(!desired.radio && migrated==2);
}
