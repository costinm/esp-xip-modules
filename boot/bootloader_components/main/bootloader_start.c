/* SPDX-License-Identifier: Apache-2.0 */
/* Stage2: selects Main or the loader. See docs/boot.md.
 *
 *  - Normal boots write nothing to flash and add no delay.
 *  - Five quick resets (or five watchdog/brown-out resets in a longer window)
 *    select the loader; so does the optional entry button and the RTC request.
 *  - The QIO verdict lives in the state sector (written once). */

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <string.h>

#include "sdkconfig.h"
#include "esp_err.h"
#include "esp_log.h"
#include "esp_rom_sys.h"
#include "esp_rom_gpio.h"
#include "soc/rtc.h"
#include "soc/soc.h"
#include "soc/soc_caps.h"
#if CONFIG_IDF_TARGET_ESP32
#include "soc/spi_reg.h"
#else
#include "soc/spi_mem_reg.h"
#endif
#include "hal/gpio_ll.h"
#include "bootloader_init.h"
#include "bootloader_utility.h"
#include "bootloader_common.h"
#include "bootloader_flash_priv.h"
#include "esp_app_format.h"
#include "flash_qio_mode.h"
#include "nvs_bootloader.h"
#include "esp_wifi_loader_rtc.h"
#include "esp_wifi_loader_state.h"

#if CONFIG_ESPTOOLPY_FLASHMODE_QIO || CONFIG_ESPTOOLPY_FLASHMODE_QOUT
#error "Build Stage2 as DIO; it enables QIO for Main at runtime"
#endif

#define LOADER_INDEX FACTORY_INDEX
#define MAIN_INDEX 0 /* ota_0; factory (-1) is the loader */

/* NVS stg2:flash override (read-only): 1=DIO, 2=QIO, anything else/unset=auto. */
#define STAGE2_NAMESPACE "stg2"
#define FLASH_MODE_DIO 1u
#define FLASH_MODE_QIO 2u

/* NVS stg2:boot_target (u32), written over USB by DMesh's flash-device.py: 1 = always Main, 2 =
 * always the loader. It outranks the reset pattern and the entry button, but not the one-shot
 * RTC handoff. Unset or any other value = automatic. */
#define BOOT_TARGET_MAIN 1u
#define BOOT_TARGET_LOADER 2u


static const char *TAG = "wl-boot";

/* rtc_time_get() latches the counter first. Reading RTC_CNTL_TIME0_REG directly returns a stale value
 * on the ESP32 and ESP32-S3, where it only refreshes after a write to RTC_CNTL_TIME_UPDATE. */
static uint32_t rtc_boot_ticks(void)
{
    return (uint32_t)rtc_time_get();
}

/* Lives in the custom RTC area (32 bytes). Offsets 5..7 are the shared bytes
 * in esp_wifi_loader_rtc.h; offset 4 is unused (legacy health event). */
typedef struct {
    uint8_t magic;
    uint8_t generation;
    uint8_t quick_count;
    uint8_t abnormal_count;
    uint8_t reserved;
    uint8_t handoff;   /* WL_RTC_HANDOFF; kept across reinitialization */
    uint8_t flash;     /* WL_RTC_FLASH */
    uint8_t reason;    /* WL_RTC_REASON */
    uint32_t quick_start;
    uint32_t abnormal_start;
    uint32_t trial_start;
} boot_state_t;
_Static_assert(offsetof(boot_state_t, handoff) + WL_RTC_CUSTOM_OFFSET == WL_RTC_HANDOFF_OFFSET, "handoff offset");
_Static_assert(offsetof(boot_state_t, flash) + WL_RTC_CUSTOM_OFFSET == WL_RTC_FLASH_OFFSET, "flash offset");
_Static_assert(offsetof(boot_state_t, reason) + WL_RTC_CUSTOM_OFFSET == WL_RTC_REASON_OFFSET, "reason offset");
_Static_assert(sizeof(boot_state_t) <= 0x20, "custom RTC area");

#define STATE_MAGIC 0x57u
#define STATE_GENERATION 3u

static boot_state_t *boot_state(bool *fresh)
{
    int reset = esp_rom_get_reset_reason(0);
    rtc_retain_mem_t *retain = bootloader_common_get_rtc_retain_mem();
    boot_state_t *s = (boot_state_t *)retain->custom;
    *fresh = false;
    /* After a power-on the retain struct holds random bits and an invalid CRC. ESP-IDF's
     * bootloader_utility zeroes the WHOLE struct, our custom bytes included, the first time it
     * finds the CRC invalid, which happens right before the app starts. Clear it now and make
     * the CRC valid (the custom area is not part of it) so our bytes survive that call. */
    if (reset == RESET_REASON_CHIP_POWER_ON) {
        bootloader_common_reset_rtc_retain_mem();
    }
    if (s->magic != STATE_MAGIC || s->generation != STATE_GENERATION) {
        /* The custom RTC area is excluded from the IDF retain CRC. Keep the handoff
         * byte (Main may have just written it) except after a power-on, when RTC
         * memory holds random bits that could look like a request. */
        uint8_t handoff = reset == RESET_REASON_CHIP_POWER_ON ? WL_HANDOFF_NORMAL : s->handoff;
        memset(s, 0, sizeof(*s));
        s->magic = STATE_MAGIC;
        s->generation = STATE_GENERATION;
        s->handoff = handoff;
        *fresh = true;
    }
    bootloader_common_update_rtc_retain_mem(NULL, false); /* CRC valid; custom bytes not in it */
    return s;
}

static bool read_u32(const char *key, uint32_t *value)
{
    nvs_bootloader_read_list_t item = {
        .namespace_name = STAGE2_NAMESPACE, .key_name = key, .value_type = NVS_TYPE_U32,
    };
    esp_err_t err = nvs_bootloader_read("nvs", 1, &item);
    if (err != ESP_OK || item.result_code != ESP_OK) return false;
    *value = item.value.u32_val;
    return true;
}

/* ---- state sector access (last two sectors of the loader partition) ---- */

static uint32_t state_base;

static int st_read(void *c, uint32_t off, void *buf, size_t n)
{
    return bootloader_flash_read(state_base + off, buf, n, false) == ESP_OK ? 0 : -1;
}
static int st_write(void *c, uint32_t off, const void *buf, size_t n)
{
    return bootloader_flash_write(state_base + off, (void *)buf, n, false) == ESP_OK ? 0 : -1;
}
static int st_erase(void *c, uint32_t off)
{
    return bootloader_flash_erase_sector((state_base + off) / WL_STATE_SECTOR) == ESP_OK ? 0 : -1;
}
static const wl_flash_ops_t state_ops = {NULL, st_read, st_write, st_erase};

/* ---- reset accounting ---- */

static bool reset_is_abnormal(int reason)
{
    switch (reason) {
    case RESET_REASON_CORE_RTC_WDT:
    case RESET_REASON_CORE_MWDT0:
    case RESET_REASON_CORE_MWDT1:
    case RESET_REASON_SYS_RTC_WDT:
#if !CONFIG_IDF_TARGET_ESP32
    case RESET_REASON_SYS_SUPER_WDT:
#endif
    case RESET_REASON_SYS_BROWN_OUT:
    case RESET_REASON_CPU0_MWDT0:
    case RESET_REASON_CPU0_RTC_WDT:
        return true;
    default:
        return false;
    }
}

typedef struct { bool quick, abnormal; } reset_verdict_t;

/* Counts this boot. RTC-only: power cycles and EN resets that clear RTC are not
 * counted (the state was just reinitialized). A panic restarts through the
 * software reset path, so it is counted with the quick resets. */
static reset_verdict_t account_resets(boot_state_t *s, int reset, uint32_t now)
{
    reset_verdict_t v = {0};
    if (reset == RESET_REASON_CORE_DEEP_SLEEP || reset == RESET_REASON_CHIP_POWER_ON) {
        return v;
    }
    if (s->quick_count == 0 || now - s->quick_start > CONFIG_WL_BOOT_QUICK_WINDOW_TICKS) {
        s->quick_start = now;
        s->quick_count = 1;
    } else if (s->quick_count < 255) {
        ++s->quick_count;
    }
    if (s->quick_count >= CONFIG_WL_BOOT_QUICK_RESETS) {
        v.quick = true;
        s->quick_count = 0;
    }
    if (reset_is_abnormal(reset)) {
        if (s->abnormal_count == 0 || now - s->abnormal_start > CONFIG_WL_BOOT_ABNORMAL_WINDOW_TICKS) {
            s->abnormal_start = now;
            s->abnormal_count = 1;
        } else if (s->abnormal_count < 255) {
            ++s->abnormal_count;
        }
        if (s->abnormal_count >= CONFIG_WL_BOOT_ABNORMAL_RESETS) {
            v.abnormal = true;
            s->abnormal_count = 0;
        }
    }
    return v;
}

static bool button_pressed(void)
{
#if CONFIG_WL_BOOT_BUTTON_GPIO >= 0
    const int pin = CONFIG_WL_BOOT_BUTTON_GPIO;
    const int active = CONFIG_WL_BOOT_BUTTON_ACTIVE_LOW ? 0 : 1;
    esp_rom_gpio_pad_select_gpio(pin);
    gpio_ll_input_enable(&GPIO, pin);
    if (CONFIG_WL_BOOT_BUTTON_ACTIVE_LOW) gpio_ll_pullup_en(&GPIO, pin);
    else gpio_ll_pulldown_en(&GPIO, pin);
    esp_rom_delay_us(50);
    /* Not pressed: return at once, so no boot delay. Pressed: it must stay
     * pressed for about 300 ms. */
    for (int ms = 0; ms < 300; ++ms) {
        if (gpio_ll_get_level(&GPIO, pin) != active) return false;
        esp_rom_delay_us(1000);
    }
    return true;
#else
    return false;
#endif
}

/* ---- QIO autodetect ---- */

/* Evaluate a pending trial on the next reset.
 *  - A watchdog or brown-out reset fails it whenever it comes: a Main that hangs in QIO and is reset by the
 *    watchdog after a few seconds must not be recorded as "QIO ok".
 *  - A reset after CONFIG_WL_BOOT_TRIAL_PASS_TICKS (about 30 s) passes it.
 *  - A reset inside the quick-reset window fails it (a crash loop).
 *  - Anything in between (a person pressing reset) proves nothing: the trial is simply dropped and starts
 *    again on the next boot.
 * QIO failed: retry the same Main once in DIO. DIO stable: permanent "DIO only" record. DIO also unstable:
 * Main is at fault, nothing is recorded. A panic restarts through a software reset, so it only counts as a
 * failure while it happens inside the quick-reset window. */
static void account_trial(boot_state_t *s, wl_state_t *st, int reset, uint32_t now)
{
    uint8_t flash = s->flash & (WL_FLASH_QIO_TRIAL | WL_FLASH_DIO_TRIAL);
    s->flash = flash;
    if (!flash || reset == RESET_REASON_CHIP_POWER_ON || reset == RESET_REASON_CORE_DEEP_SLEEP) {
        return;
    }
    uint32_t elapsed = now - s->trial_start;
    bool pass = false, fail = false;
    if (reset_is_abnormal(reset)) {
        fail = true;
    } else if (elapsed >= CONFIG_WL_BOOT_TRIAL_PASS_TICKS) {
        pass = true;
    } else if (elapsed < CONFIG_WL_BOOT_QUICK_WINDOW_TICKS) {
        fail = true;
    }
    bool qio = flash & WL_FLASH_QIO_TRIAL;
    s->flash = 0;
    if (qio && fail) {
        s->flash = WL_FLASH_DIO_TRIAL;
        s->trial_start = now; /* the DIO trial is timed from this boot, not from the failed QIO one */
    } else if (pass && st->view.qio == 0) {
        uint8_t verdict = qio ? WL_QIO_OK : WL_QIO_DIO_ONLY;
        wl_state_append(st, WL_REC_QIO, &verdict, 1);
    }
}

static bool cache_reads_qio(void)
{
#if CONFIG_IDF_TARGET_ESP32
    return (REG_READ(SPI_CTRL_REG(0)) & SPI_FREAD_QIO) != 0;
#else
    return (REG_READ(SPI_MEM_CTRL_REG(0)) & SPI_MEM_FREAD_QIO) != 0;
#endif
}

static bool main_header_present(const bootloader_state_t *bs)
{
    esp_image_header_t header;
    return bs->ota[MAIN_INDEX].size != 0 &&
           bootloader_flash_read(bs->ota[MAIN_INDEX].offset, &header, sizeof(header), true) == ESP_OK &&
           header.magic == ESP_IMAGE_HEADER_MAGIC;
}

static const char *apply_flash_mode(const bootloader_state_t *bs, int partition_index,
                                    boot_state_t *s, wl_state_t *st)
{
    uint32_t mode = 0;
    (void)read_u32("flash", &mode);
    if (partition_index != MAIN_INDEX) return "dio-loader";
    if (mode == FLASH_MODE_DIO) return "dio";
    /* The loader erases Main's first sector before an update. With no header
     * the image loader falls through to the loader, which must stay in DIO. */
    if (!main_header_present(bs)) return "dio-no-main";
    bool forced = mode == FLASH_MODE_QIO;
    if (!forced) {
        if (st->view.qio == WL_QIO_DIO_ONLY) return "dio-verdict";
        if (s->flash & WL_FLASH_DIO_TRIAL) return "dio-retry";
    }
#if SOC_SPI_MEM_SUPPORT_FLASH_OPI_MODE
    if (bootloader_flash_is_octal_mode_enabled()) return "octal";
#endif
    bootloader_enable_qio_mode();
    if (!cache_reads_qio()) {
        /* The chip did not accept its quad-enable bit: a permanent answer. */
        if (!forced && st->view.qio == 0) {
            uint8_t v = WL_QIO_DIO_ONLY;
            wl_state_append(st, WL_REC_QIO, &v, 1);
        }
        return "dio-qe-failed";
    }
    s->flash |= WL_FLASH_QIO_ACTIVE;
    if (!forced && st->view.qio == 0) {
        s->flash |= WL_FLASH_QIO_TRIAL;
        s->trial_start = rtc_boot_ticks();
    }
    return "qio";
}

/* ---- selection ---- */

static int select_partition(boot_state_t *s, wl_state_t *st, bool fresh)
{
    int reset = esp_rom_get_reset_reason(0);
    uint32_t now = rtc_boot_ticks();
    uint8_t handoff = s->handoff;
    s->handoff = WL_HANDOFF_NORMAL;
    s->flash &= (uint8_t)~WL_FLASH_QIO_ACTIVE; /* describes one boot */
    s->reason = WL_REASON_NONE;
    (void)fresh;

    account_trial(s, st, reset, now);
    reset_verdict_t v = account_resets(s, reset, now);

    esp_rom_printf("WL stage2 v=%u: reset=%d handoff=%u quick=%u abn=%u flash=0x%x\n",
                   (unsigned)WL_STAGE2_VERSION, reset, (unsigned)handoff,
                   (unsigned)s->quick_count, (unsigned)s->abnormal_count, (unsigned)s->flash);

    /* The loader's "install finished" handoff outranks everything else. */
    if (handoff == WL_HANDOFF_MAIN) {
        ESP_LOGW(TAG, "select Main: loader handoff");
        return MAIN_INDEX;
    }
    uint32_t boot_target = 0;
    if (handoff != WL_HANDOFF_LOADER && read_u32("boot_target", &boot_target)) {
        if (boot_target == BOOT_TARGET_LOADER) {
            s->reason = WL_REASON_UPDATE;
            return LOADER_INDEX;
        }
        if (boot_target == BOOT_TARGET_MAIN) return MAIN_INDEX;
    }
    if (button_pressed()) {
        s->reason = WL_REASON_UPDATE;
        return LOADER_INDEX;
    }
    if (handoff == WL_HANDOFF_LOADER) {
        s->reason = WL_REASON_UPDATE;
        return LOADER_INDEX;
    }
    if (v.quick || v.abnormal) {
        s->reason = WL_REASON_CRASH;
        return LOADER_INDEX;
    }
    /* A blank or corrupt Main falls through to the loader in the image loader. */
    return MAIN_INDEX;
}

void __attribute__((noreturn)) call_start_cpu0(void)
{
    if (bootloader_init() != ESP_OK) {
        bootloader_reset();
    }

    bootloader_state_t bs = {0};
    if (!bootloader_utility_load_partition_table(&bs)) {
        ESP_LOGE(TAG, "partition table load failed");
        bootloader_reset();
    }

    bool fresh;
    boot_state_t *s = boot_state(&fresh);
    wl_state_t st;
    state_base = bs.factory.offset + bs.factory.size - WL_STATE_SIZE;
    wl_state_open(&st, &state_ops);

    int partition_index = select_partition(s, &st, fresh);
    const char *flash_mode = apply_flash_mode(&bs, partition_index, s, &st);
    esp_rom_printf("WL stage2: load partition=%d flash=%s\n", partition_index, flash_mode);
    bootloader_utility_load_boot_image(&bs, partition_index);
    bootloader_reset();
}

#if CONFIG_LIBC_NEWLIB
struct _reent *__getreent(void)
{
    return _GLOBAL_REENT;
}
#endif
