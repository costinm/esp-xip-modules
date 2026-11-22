/* SPDX-License-Identifier: Apache-2.0 */
/* The `crypto` component on the ESP: calls on pool packets, answered with the SDK's PSA. No PSA type crosses the module ABI; an operation in
 * progress is a small handle owned here and released by finish, abort or the end of the owning instance. NOT BUILT in the host tree (needs
 * ESP-IDF): the host runtime's crates/xip-runtime-host/src/crypto.rs is the reference with tests. */
#include "xip.h"
#include "sdkconfig.h"
#include "soc/soc_caps.h"
#include "psa/crypto.h"
#include "mbedtls/platform_util.h"
#include "freertos/FreeRTOS.h"
#include "freertos/semphr.h"
#if defined(CONFIG_MBEDTLS_HARDWARE_AES)
#include "aes/esp_aes.h"
#include "aes/esp_aes_gcm.h"
#endif
#include <stdlib.h>
#include <string.h>

/* xip_runtime.c: packet window / slot-relative helpers. */
extern uint8_t *xip_runtime_packet_range(uint32_t pkt, uint32_t offset, uint32_t len, bool write);
extern uint8_t *xip_runtime_packet_slot(uint32_t pkt, uint32_t slot_off, uint32_t len, bool write);

#define HASHES_MAX 4
static struct { void *owner; psa_hash_operation_t op; uint32_t id; } hashes[HASHES_MAX];
static uint32_t next_id = 1;

/* Aligned internal DRAM scratch buffers eliminate per-packet malloc/free and
 * ESP-IDF AES-DMA start-alignment bounce allocations. Keeping `out` before
 * `in` also satisfies mbedTLS/ESP-IDF's `output < input` non-overlap check. */
static struct {
    uint8_t out[XIP_PKT_MAX + 64u] __attribute__((aligned(64)));
    uint8_t in[XIP_PKT_MAX + 64u] __attribute__((aligned(64)));
    uint8_t aad[64u] __attribute__((aligned(64)));
    uint8_t nonce[16u] __attribute__((aligned(64)));
    uint8_t tag[16u] __attribute__((aligned(64)));
    uint8_t check_tag[16u] __attribute__((aligned(64)));
    uint8_t iv[16u] __attribute__((aligned(64)));
    uint8_t ctr[16u] __attribute__((aligned(64)));
    uint8_t stream[16u] __attribute__((aligned(64)));
    uint8_t block[16u] __attribute__((aligned(64)));
    uint8_t s0[16u] __attribute__((aligned(64)));
} aead_scratch __attribute__((aligned(64)));

static StaticSemaphore_t aead_lock_buf;
static SemaphoreHandle_t aead_lock_handle;
static portMUX_TYPE aead_lock_spin = portMUX_INITIALIZER_UNLOCKED;

static void aead_lock(void)
{
    if (!aead_lock_handle) {
        portENTER_CRITICAL(&aead_lock_spin);
        if (!aead_lock_handle) aead_lock_handle = xSemaphoreCreateMutexStatic(&aead_lock_buf);
        portEXIT_CRITICAL(&aead_lock_spin);
    }
    xSemaphoreTake(aead_lock_handle, portMAX_DELAY);
}

static void aead_unlock(void)
{
    xSemaphoreGive(aead_lock_handle);
}

#if defined(CONFIG_MBEDTLS_HARDWARE_AES)
#define AEAD_KEY_SLOTS 4
typedef struct {
    bool used;
    void *owner;
    uint32_t alg;
    uint32_t key_len;
    uint64_t seq;
    uint8_t key[32];
    union {
        esp_gcm_context gcm;
        esp_aes_context aes;
    } u;
} aead_key_slot_t;

static aead_key_slot_t aead_keys[AEAD_KEY_SLOTS];
static uint64_t aead_key_seq = 1;

static void aead_evict_slot(aead_key_slot_t *slot)
{
    if (!slot->used) return;
    if (slot->alg == PSA_ALG_GCM) esp_aes_gcm_free(&slot->u.gcm);
    else if (slot->alg == PSA_ALG_CCM) esp_aes_free(&slot->u.aes);
    mbedtls_platform_zeroize(slot->key, sizeof(slot->key));
    slot->used = false;
    slot->owner = NULL;
    slot->alg = 0;
    slot->key_len = 0;
    slot->seq = 0;
}

static aead_key_slot_t *aead_get_slot(void *owner, uint32_t alg, const uint8_t *key, uint32_t key_len, int *err_out)
{
    int free_slot = -1;
    int lru_slot = 0;
    for (int i = 0; i < AEAD_KEY_SLOTS; i++) {
        if (aead_keys[i].used) {
            if (aead_keys[i].owner == owner && aead_keys[i].alg == alg &&
                aead_keys[i].key_len == key_len && memcmp(aead_keys[i].key, key, key_len) == 0) {
                aead_keys[i].seq = ++aead_key_seq;
                return &aead_keys[i];
            }
            if (aead_keys[i].seq < aead_keys[lru_slot].seq) lru_slot = i;
        } else if (free_slot < 0) {
            free_slot = i;
        }
    }
    int idx = free_slot >= 0 ? free_slot : lru_slot;
    aead_evict_slot(&aead_keys[idx]);
    int rc = 0;
    if (alg == PSA_ALG_GCM) {
        esp_aes_gcm_init(&aead_keys[idx].u.gcm);
        rc = esp_aes_gcm_setkey(&aead_keys[idx].u.gcm, 0, key, (unsigned)(key_len * 8u));
        if (rc != 0) esp_aes_gcm_free(&aead_keys[idx].u.gcm);
    } else if (alg == PSA_ALG_CCM) {
        esp_aes_init(&aead_keys[idx].u.aes);
        rc = esp_aes_setkey(&aead_keys[idx].u.aes, key, (unsigned)(key_len * 8u));
        if (rc != 0) esp_aes_free(&aead_keys[idx].u.aes);
    } else {
        *err_out = PSA_ERROR_NOT_SUPPORTED;
        return NULL;
    }
    if (rc != 0) {
        *err_out = rc;
        return NULL;
    }
    aead_keys[idx].used = true;
    aead_keys[idx].owner = owner;
    aead_keys[idx].alg = alg;
    aead_keys[idx].key_len = key_len;
    memcpy(aead_keys[idx].key, key, key_len);
    aead_keys[idx].seq = ++aead_key_seq;
    return &aead_keys[idx];
}

static int aead_gcm_run(esp_gcm_context *gcm, int mode, size_t len, const uint8_t *nonce,
                        const uint8_t *aad, size_t aad_len, const uint8_t *in_ptr,
                        uint8_t *out_ptr, uint8_t tag[16])
{
#if !defined(CONFIG_MBEDTLS_HARDWARE_GCM) && defined(SOC_CACHE_INTERNAL_MEM_VIA_L1CACHE) && SOC_CACHE_INTERNAL_MEM_VIA_L1CACHE
    /* Split DMA transfers to 64-byte multiples + <=63-byte polled tail so
     * ESP-IDF's L1-cache DMA engine never allocates start/end alignment bounce buffers. */
    if (len > 128u && (len & 63u) != 0u) {
        size_t head = len & ~63u;
        size_t tail = len - head;
        size_t olen = 0;
        int rc = esp_aes_gcm_starts(gcm, mode, nonce, 12u);
        if (rc != 0) return rc;
        rc = esp_aes_gcm_update_ad(gcm, aad, aad_len);
        if (rc != 0) return rc;
        if (head > 0u) {
            rc = esp_aes_gcm_update(gcm, in_ptr, head, out_ptr, head, &olen);
            if (rc != 0) return rc;
        }
        if (tail > 0u) {
            rc = esp_aes_gcm_update(gcm, in_ptr + head, tail, out_ptr + head, tail, &olen);
            if (rc != 0) return rc;
        }
        return esp_aes_gcm_finish(gcm, out_ptr, 0u, &olen, tag, 16u);
    }
#endif
    return esp_aes_gcm_crypt_and_tag(gcm, mode, len, nonce, 12u, aad, aad_len,
                                     in_ptr, out_ptr, 16u, tag);
}

static int aead_ccm_cbc_mac(esp_aes_context *aes, const uint8_t *nonce,
                            const uint8_t *aad, size_t aad_len,
                            const uint8_t *pt_ptr, size_t pt_len, uint8_t mac_out[16])
{
    if (pt_len > 0x00ffffffu || aad_len >= 0xff00u) return PSA_ERROR_INVALID_ARGUMENT;
    memset(aead_scratch.iv, 0, 16u);
    aead_scratch.block[0] = (uint8_t)((aad_len ? 0x40u : 0x00u) | 0x38u | 0x02u);
    memcpy(aead_scratch.block + 1, nonce, 12u);
    aead_scratch.block[13] = (uint8_t)((pt_len >> 16) & 0xffu);
    aead_scratch.block[14] = (uint8_t)((pt_len >> 8) & 0xffu);
    aead_scratch.block[15] = (uint8_t)(pt_len & 0xffu);
    int rc = esp_aes_crypt_cbc(aes, ESP_AES_ENCRYPT, 16u, aead_scratch.iv, aead_scratch.block, aead_scratch.stream);
    if (rc != 0) return rc;

    if (aad_len > 0u) {
        memset(aead_scratch.block, 0, 16u);
        aead_scratch.block[0] = (uint8_t)((aad_len >> 8) & 0xffu);
        aead_scratch.block[1] = (uint8_t)(aad_len & 0xffu);
        size_t pos = 2u;
        for (size_t i = 0; i < aad_len; i++) {
            aead_scratch.block[pos++] = aad[i];
            if (pos == 16u) {
                rc = esp_aes_crypt_cbc(aes, ESP_AES_ENCRYPT, 16u, aead_scratch.iv, aead_scratch.block, aead_scratch.stream);
                if (rc != 0) return rc;
                memset(aead_scratch.block, 0, 16u);
                pos = 0u;
            }
        }
        if (pos > 0u) {
            rc = esp_aes_crypt_cbc(aes, ESP_AES_ENCRYPT, 16u, aead_scratch.iv, aead_scratch.block, aead_scratch.stream);
            if (rc != 0) return rc;
        }
    }

    size_t full = pt_len & ~15u;
    size_t rem = pt_len - full;
    if (full > 0u) {
        uint8_t *cbc_sink = (pt_ptr == aead_scratch.out) ? aead_scratch.in : aead_scratch.out;
#if defined(SOC_CACHE_INTERNAL_MEM_VIA_L1CACHE) && SOC_CACHE_INTERNAL_MEM_VIA_L1CACHE
        if (full > 128u && (full & 63u) != 0u) {
            size_t head = full & ~63u;
            size_t tail = full - head;
            rc = esp_aes_crypt_cbc(aes, ESP_AES_ENCRYPT, head, aead_scratch.iv, pt_ptr, cbc_sink);
            if (rc != 0) return rc;
            if (tail > 0u) {
                rc = esp_aes_crypt_cbc(aes, ESP_AES_ENCRYPT, tail, aead_scratch.iv, pt_ptr + head, cbc_sink);
                if (rc != 0) return rc;
            }
        } else {
            rc = esp_aes_crypt_cbc(aes, ESP_AES_ENCRYPT, full, aead_scratch.iv, pt_ptr, cbc_sink);
            if (rc != 0) return rc;
        }
#else
        rc = esp_aes_crypt_cbc(aes, ESP_AES_ENCRYPT, full, aead_scratch.iv, pt_ptr, cbc_sink);
        if (rc != 0) return rc;
#endif
    }
    if (rem > 0u) {
        memset(aead_scratch.block, 0, 16u);
        memcpy(aead_scratch.block, pt_ptr + full, rem);
        rc = esp_aes_crypt_cbc(aes, ESP_AES_ENCRYPT, 16u, aead_scratch.iv, aead_scratch.block, aead_scratch.stream);
        if (rc != 0) return rc;
    }
    memcpy(mac_out, aead_scratch.iv, 16u);
    return 0;
}

static int aead_ccm_ctr(esp_aes_context *aes, const uint8_t *nonce,
                        const uint8_t *in_ptr, uint8_t *out_ptr, size_t len)
{
    if (len == 0u) return 0;
    memset(aead_scratch.stream, 0, 16u);
    aead_scratch.ctr[0] = 0x02u;
    memcpy(aead_scratch.ctr + 1, nonce, 12u);
    aead_scratch.ctr[13] = 0u;
    aead_scratch.ctr[14] = 0u;
    aead_scratch.ctr[15] = 1u;
    size_t nc_off = 0u;
#if defined(SOC_CACHE_INTERNAL_MEM_VIA_L1CACHE) && SOC_CACHE_INTERNAL_MEM_VIA_L1CACHE
    if (len > 128u && (len & 63u) != 0u) {
        size_t head = len & ~63u;
        size_t tail = len - head;
        int rc = esp_aes_crypt_ctr(aes, head, &nc_off, aead_scratch.ctr, aead_scratch.stream, in_ptr, out_ptr);
        if (rc != 0) return rc;
        return esp_aes_crypt_ctr(aes, tail, &nc_off, aead_scratch.ctr, aead_scratch.stream, in_ptr + head, out_ptr + head);
    }
#endif
    return esp_aes_crypt_ctr(aes, len, &nc_off, aead_scratch.ctr, aead_scratch.stream, in_ptr, out_ptr);
}

static int aead_ccm_run(esp_aes_context *aes, int mode, size_t len, const uint8_t *nonce,
                        const uint8_t *aad, size_t aad_len, const uint8_t *in_ptr,
                        uint8_t *out_ptr, uint8_t tag[16])
{
    int rc = 0;
    if (mode == ESP_AES_ENCRYPT) {
        rc = aead_ccm_cbc_mac(aes, nonce, aad, aad_len, in_ptr, len, tag);
        if (rc != 0) return rc;
        rc = aead_ccm_ctr(aes, nonce, in_ptr, out_ptr, len);
        if (rc != 0) return rc;
    } else {
        rc = aead_ccm_ctr(aes, nonce, in_ptr, out_ptr, len);
        if (rc != 0) return rc;
        rc = aead_ccm_cbc_mac(aes, nonce, aad, aad_len, out_ptr, len, tag);
        if (rc != 0) return rc;
    }
    aead_scratch.block[0] = 0x02u;
    memcpy(aead_scratch.block + 1, nonce, 12u);
    aead_scratch.block[13] = 0u;
    aead_scratch.block[14] = 0u;
    aead_scratch.block[15] = 0u;
    rc = esp_aes_crypt_ecb(aes, ESP_AES_ENCRYPT, aead_scratch.block, aead_scratch.s0);
    if (rc != 0) return rc;
    for (size_t i = 0; i < 16u; i++) tag[i] ^= aead_scratch.s0[i];
    return 0;
}
#endif

static int map(psa_status_t s)
{
    switch (s) {
    case PSA_SUCCESS: return 0;
    case PSA_ERROR_INVALID_SIGNATURE: return XIP_ERR_AUTH;
    case PSA_ERROR_INVALID_ARGUMENT: return XIP_ERR_ARG;
    case PSA_ERROR_NOT_SUPPORTED: return XIP_ERR_UNSUPPORTED;
    case PSA_ERROR_INSUFFICIENT_MEMORY: return XIP_ERR_NOMEM;
    case PSA_ERROR_BUFFER_TOO_SMALL: return XIP_ERR_SIZE;
    default: return XIP_ERR_IO;
    }
}
static size_t digest_len(uint32_t alg)
{
    return alg == PSA_ALG_SHA_256 ? 32 : alg == PSA_ALG_SHA_384 ? 48 : alg == PSA_ALG_SHA_512 ? 64 : 0;
}
static int slot_of(void *owner, uint32_t id)
{
    for (int i = 0; i < HASHES_MAX; i++) if (hashes[i].id == id && hashes[i].owner == owner) return i;
    return -1;
}

static int c_hash(void *ctx, uint32_t alg, uint32_t pkt, uint32_t off, uint32_t len, uint32_t out_pkt, uint32_t out_off)
{
    (void)ctx;
    size_t n = digest_len(alg);
    if (!n) return XIP_ERR_UNSUPPORTED;
    const uint8_t *in = xip_runtime_packet_range(pkt, off, len, false);
    uint8_t *out = xip_runtime_packet_range(out_pkt, out_off, (uint32_t)n, true);
    if ((!in && len) || !out) return XIP_ERR_ARG;
    size_t written = 0;
    psa_status_t s = psa_hash_compute(alg, in, len, out, n, &written);
    return s == PSA_SUCCESS && written == n ? (int)n : map(s == PSA_SUCCESS ? PSA_ERROR_GENERIC_ERROR : s);
}
static int c_hash_start(void *ctx, uint32_t alg)
{
    if (!digest_len(alg)) return XIP_ERR_UNSUPPORTED;
    for (int i = 0; i < HASHES_MAX; i++) {
        if (hashes[i].id) continue;
        hashes[i].op = (psa_hash_operation_t)PSA_HASH_OPERATION_INIT;
        psa_status_t s = psa_hash_setup(&hashes[i].op, alg);
        if (s != PSA_SUCCESS) { psa_hash_abort(&hashes[i].op); return map(s); }
        hashes[i].owner = ctx;
        if (next_id == 0) next_id = 1;
        hashes[i].id = next_id++;
        return (int)hashes[i].id;
    }
    return XIP_ERR_BUSY;
}
static int c_hash_update(void *ctx, uint32_t handle, uint32_t pkt, uint32_t off, uint32_t len)
{
    int i = slot_of(ctx, handle);
    if (i < 0) return XIP_ERR_NOTFOUND;
    const uint8_t *in = xip_runtime_packet_range(pkt, off, len, false);
    if (!in && len) return XIP_ERR_ARG;
    return map(psa_hash_update(&hashes[i].op, in, len));
}
static int c_hash_finish(void *ctx, uint32_t handle, uint32_t out_pkt, uint32_t out_off)
{
    int i = slot_of(ctx, handle);
    if (i < 0) return XIP_ERR_NOTFOUND;
    uint8_t digest[64]; size_t n = 0;
    psa_status_t s = psa_hash_finish(&hashes[i].op, digest, sizeof(digest), &n);   /* success or failure, the operation is over */
    hashes[i].id = 0; hashes[i].owner = NULL;
    if (s != PSA_SUCCESS) return map(s);
    uint8_t *out = xip_runtime_packet_range(out_pkt, out_off, (uint32_t)n, true);
    if (!out) return XIP_ERR_ARG;
    memcpy(out, digest, n);
    mbedtls_platform_zeroize(digest, sizeof(digest));
    return (int)n;
}
static int c_hash_abort(void *ctx, uint32_t handle)
{
    int i = slot_of(ctx, handle);
    if (i >= 0) { psa_hash_abort(&hashes[i].op); hashes[i].id = 0; hashes[i].owner = NULL; }
    return 0;
}
/* The instance ended holding hashes or cached AEAD keys: release them (called by the runtime). */
void xip_crypto_release(void *owner)
{
    for (int i = 0; i < HASHES_MAX; i++)
        if (hashes[i].id && hashes[i].owner == owner) { psa_hash_abort(&hashes[i].op); hashes[i].id = 0; hashes[i].owner = NULL; }
#if defined(CONFIG_MBEDTLS_HARDWARE_AES)
    aead_lock();
    for (int i = 0; i < AEAD_KEY_SLOTS; i++) {
        if (aead_keys[i].used && (!owner || aead_keys[i].owner == owner)) aead_evict_slot(&aead_keys[i]);
    }
    aead_unlock();
#endif
}

/* Native MPI decompression of a SEC1 compressed key lives in xip_identity.c (the SDK's PSA import need not accept it). */
extern int xip_identity_expand_public(const uint8_t *in, size_t len, uint8_t out[65]);

static int c_verify(void *ctx, uint32_t alg, uint32_t pkt, uint32_t off, uint32_t key_len, uint32_t message_len, uint32_t signature_len)
{
    (void)ctx;
    if (alg != PSA_ALG_ECDSA(PSA_ALG_SHA_256)) return XIP_ERR_UNSUPPORTED;
    if (signature_len != 64 || (key_len != 33 && key_len != 65)) return XIP_ERR_ARG;
    uint64_t total = (uint64_t)key_len + message_len + signature_len;
    if (total > UINT32_MAX) return XIP_ERR_ARG;
    const uint8_t *p = xip_runtime_packet_range(pkt, off, (uint32_t)total, false);
    if (!p) return XIP_ERR_ARG;
    uint8_t expanded[65], hash[32]; size_t n = 0;
    int rc = xip_identity_expand_public(p, key_len, expanded);
    if (rc) return rc;
    psa_key_attributes_t attrs = PSA_KEY_ATTRIBUTES_INIT;
    psa_set_key_type(&attrs, PSA_KEY_TYPE_ECC_PUBLIC_KEY(PSA_ECC_FAMILY_SECP_R1));
    psa_set_key_bits(&attrs, 256); psa_set_key_usage_flags(&attrs, PSA_KEY_USAGE_VERIFY_HASH);
    psa_set_key_algorithm(&attrs, PSA_ALG_ECDSA(PSA_ALG_SHA_256));
    mbedtls_svc_key_id_t key;
    psa_status_t s = psa_import_key(&attrs, expanded, sizeof(expanded), &key);
    psa_reset_key_attributes(&attrs);
    if (s != PSA_SUCCESS) return XIP_ERR_ARG;
    s = psa_hash_compute(PSA_ALG_SHA_256, p + key_len, message_len, hash, sizeof(hash), &n);
    if (s == PSA_SUCCESS && n == sizeof(hash))
        s = psa_verify_hash(key, PSA_ALG_ECDSA(PSA_ALG_SHA_256), hash, n, p + key_len + message_len, 64);
    else s = PSA_ERROR_GENERIC_ERROR;
    psa_status_t cleanup = psa_destroy_key(key);
    if (s != PSA_SUCCESS) return map(s);
    return cleanup == PSA_SUCCESS ? 0 : XIP_ERR_IO;
}
static int c_random(void *ctx, uint32_t pkt, uint32_t off, uint32_t len)
{
    (void)ctx;
    uint8_t *out = xip_runtime_packet_range(pkt, off, len, true);
    if (!out && len) return XIP_ERR_ARG;
    return map(psa_generate_random(out, len));
}
#ifndef PSA_ALG_CCM
#define PSA_ALG_CCM ((psa_algorithm_t)0x05500100)
#endif

static int aead_dispatch(void *ctx, uint32_t alg, int mode, const uint8_t *key_bytes, uint32_t key_len,
                         const uint8_t *nonce, const uint8_t *aad_ptr, uint32_t aad_len,
                         const uint8_t *in_ptr, uint8_t *out_ptr, size_t plain_len,
                         const uint8_t *verify_tag, uint8_t out_tag[16])
{
#if defined(CONFIG_MBEDTLS_HARDWARE_AES)
    int err = 0;
    aead_key_slot_t *slot = aead_get_slot(ctx, alg, key_bytes, key_len, &err);
    if (!slot) return map((psa_status_t)err);
    uint8_t *tag_buf = (mode == 1 /* encrypt */) ? out_tag : aead_scratch.check_tag;
    if (alg == PSA_ALG_GCM) {
        err = aead_gcm_run(&slot->u.gcm, mode ? ESP_AES_ENCRYPT : ESP_AES_DECRYPT, plain_len,
                           nonce, aad_ptr, aad_len, in_ptr, out_ptr, tag_buf);
    } else {
        err = aead_ccm_run(&slot->u.aes, mode ? ESP_AES_ENCRYPT : ESP_AES_DECRYPT, plain_len,
                           nonce, aad_ptr, aad_len, in_ptr, out_ptr, tag_buf);
    }
    if (err != 0) {
        aead_evict_slot(slot);
        mbedtls_platform_zeroize(aead_scratch.check_tag, sizeof(aead_scratch.check_tag));
        return map((psa_status_t)err);
    }
    if (!mode) {
        int diff = 0;
        for (size_t i = 0; i < 16u; i++) diff |= verify_tag[i] ^ aead_scratch.check_tag[i];
        mbedtls_platform_zeroize(aead_scratch.check_tag, sizeof(aead_scratch.check_tag));
        if (diff != 0) return XIP_ERR_AUTH;
    }
    return 0;
#else
    (void)ctx;
    psa_key_attributes_t attrs = PSA_KEY_ATTRIBUTES_INIT;
    psa_set_key_type(&attrs, PSA_KEY_TYPE_AES);
    psa_set_key_bits(&attrs, (size_t)key_len * 8u);
    psa_set_key_usage_flags(&attrs, mode ? PSA_KEY_USAGE_ENCRYPT : PSA_KEY_USAGE_DECRYPT);
    psa_set_key_algorithm(&attrs, alg);
    mbedtls_svc_key_id_t key;
    psa_status_t s = psa_import_key(&attrs, key_bytes, key_len, &key);
    psa_reset_key_attributes(&attrs);
    if (s != PSA_SUCCESS) return map(s);
    size_t out_len = 0;
    if (mode) {
        uint8_t tmp[XIP_PKT_MAX + 16u];
        s = psa_aead_encrypt(key, alg, nonce, 12u, aad_ptr, aad_len, in_ptr, plain_len, tmp, sizeof(tmp), &out_len);
        psa_status_t cleanup = psa_destroy_key(key);
        if (s == PSA_SUCCESS && out_len == plain_len + 16u) {
            if (plain_len > 0u) memcpy(out_ptr, tmp, plain_len);
            memcpy(out_tag, tmp + plain_len, 16u);
        } else if (s == PSA_SUCCESS) s = PSA_ERROR_GENERIC_ERROR;
        mbedtls_platform_zeroize(tmp, sizeof(tmp));
        if (s != PSA_SUCCESS) return map(s);
        return cleanup == PSA_SUCCESS ? 0 : XIP_ERR_IO;
    } else {
        uint8_t tmp[XIP_PKT_MAX + 16u];
        if (plain_len > 0u) memcpy(tmp, in_ptr, plain_len);
        memcpy(tmp + plain_len, verify_tag, 16u);
        s = psa_aead_decrypt(key, alg, nonce, 12u, aad_ptr, aad_len, tmp, plain_len + 16u, out_ptr, plain_len, &out_len);
        psa_status_t cleanup = psa_destroy_key(key);
        mbedtls_platform_zeroize(tmp, sizeof(tmp));
        if (s != PSA_SUCCESS) return map(s);
        if (out_len != plain_len) return XIP_ERR_IO;
        return cleanup == PSA_SUCCESS ? 0 : XIP_ERR_IO;
    }
#endif
}

static int c_aead_seal(void *ctx, uint32_t alg, uint32_t pkt, uint32_t off, uint32_t key_len, uint32_t aad_len, uint32_t text_len)
{
    if (alg != PSA_ALG_GCM && alg != PSA_ALG_CCM) return XIP_ERR_UNSUPPORTED;
    if (key_len != 16u && key_len != 32u) return XIP_ERR_ARG;
    uint64_t hdr = (uint64_t)key_len + 12u + (uint64_t)aad_len;
    uint64_t total = hdr + (uint64_t)text_len + 16u;
    if (total > UINT32_MAX) return XIP_ERR_ARG;
    uint8_t *p = xip_runtime_packet_range(pkt, off, (uint32_t)total, true);
    if (!p) return XIP_ERR_ARG;
    const uint8_t *key_bytes = p;
    const uint8_t *nonce = p + key_len;
    const uint8_t *aad = nonce + 12;
    uint8_t *body = p + hdr;
    size_t out_cap = (size_t)text_len + 16u;
    if (out_cap > XIP_PKT_MAX) return XIP_ERR_NOMEM;
    aead_lock();
    memcpy(aead_scratch.nonce, nonce, 12u);
    const uint8_t *aad_ptr = NULL;
    if (aad_len > 0u) {
        if (aad_len <= sizeof(aead_scratch.aad)) { memcpy(aead_scratch.aad, aad, aad_len); aad_ptr = aead_scratch.aad; }
        else aad_ptr = aad;
    }
    if (text_len > 0u) memcpy(aead_scratch.in, body, text_len);
    int rc = aead_dispatch(ctx, alg, 1, key_bytes, key_len, aead_scratch.nonce, aad_ptr, aad_len,
                           aead_scratch.in, aead_scratch.out, text_len, NULL, aead_scratch.tag);
    if (rc == 0) {
        if (text_len > 0u) memcpy(body, aead_scratch.out, text_len);
        memcpy(body + text_len, aead_scratch.tag, 16u);
    }
    if (text_len > 0u) {
        mbedtls_platform_zeroize(aead_scratch.in, text_len);
        mbedtls_platform_zeroize(aead_scratch.out, text_len);
    }
    aead_unlock();
    return rc == 0 ? (int)out_cap : rc;
}

static int c_aead_open(void *ctx, uint32_t alg, uint32_t pkt, uint32_t off, uint32_t key_len, uint32_t aad_len, uint32_t text_len)
{
    if (alg != PSA_ALG_GCM && alg != PSA_ALG_CCM) return XIP_ERR_UNSUPPORTED;
    if ((key_len != 16u && key_len != 32u) || text_len < 16u) return XIP_ERR_ARG;
    uint64_t hdr = (uint64_t)key_len + 12u + (uint64_t)aad_len;
    uint64_t total = hdr + (uint64_t)text_len;
    if (total > UINT32_MAX) return XIP_ERR_ARG;
    uint8_t *p = xip_runtime_packet_range(pkt, off, (uint32_t)total, true);
    if (!p) return XIP_ERR_ARG;
    const uint8_t *key_bytes = p;
    const uint8_t *nonce = p + key_len;
    const uint8_t *aad = nonce + 12;
    uint8_t *body = p + hdr;
    size_t plain_cap = (size_t)(text_len - 16u);
    if (plain_cap > XIP_PKT_MAX) return XIP_ERR_NOMEM;
    aead_lock();
    memcpy(aead_scratch.nonce, nonce, 12u);
    const uint8_t *aad_ptr = NULL;
    if (aad_len > 0u) {
        if (aad_len <= sizeof(aead_scratch.aad)) { memcpy(aead_scratch.aad, aad, aad_len); aad_ptr = aead_scratch.aad; }
        else aad_ptr = aad;
    }
    if (plain_cap > 0u) memcpy(aead_scratch.in, body, plain_cap);
    memcpy(aead_scratch.tag, body + plain_cap, 16u);
    int rc = aead_dispatch(ctx, alg, 0, key_bytes, key_len, aead_scratch.nonce, aad_ptr, aad_len,
                           aead_scratch.in, aead_scratch.out, plain_cap, aead_scratch.tag, NULL);
    if (rc == 0 && plain_cap > 0u) memcpy(body, aead_scratch.out, plain_cap);
    if (plain_cap > 0u) {
        mbedtls_platform_zeroize(aead_scratch.in, plain_cap);
        mbedtls_platform_zeroize(aead_scratch.out, plain_cap);
    }
    aead_unlock();
    return rc == 0 ? (int)plain_cap : rc;
}

static int c_aead_pkt(void *ctx, uint32_t spec, uint32_t src_pkt, uint32_t src_off,
                      uint32_t dst_pkt, uint32_t dst_off, uint32_t text_len)
{
    uint32_t aad_len = spec & 0xffu;
    uint32_t key_len = (spec >> 8) & 0xffu;
    uint32_t alg_code = (spec >> 16) & 0xffu;
    bool is_open = (spec & XIP_AEAD_PKT_FLAG_OPEN) != 0u;
    uint32_t alg = alg_code == 0u ? PSA_ALG_GCM : alg_code == 1u ? PSA_ALG_CCM : 0u;
    if (!alg) return XIP_ERR_UNSUPPORTED;
    if ((key_len != 16u && key_len != 32u) || (is_open && text_len < 16u)) return XIP_ERR_ARG;

    uint64_t hdr = (uint64_t)key_len + 12u + (uint64_t)aad_len;
    if ((uint64_t)dst_off < hdr) return XIP_ERR_ARG;
    size_t plain_len = is_open ? (size_t)(text_len - 16u) : (size_t)text_len;
    size_t out_len = is_open ? plain_len : (size_t)text_len + 16u;
    if (plain_len > XIP_PKT_MAX) return XIP_ERR_NOMEM;

    const uint8_t *src_base = xip_runtime_packet_slot(src_pkt, 0u, 1u, false);
    const uint8_t *src_ptr = xip_runtime_packet_slot(src_pkt, src_off, text_len, false);
    uint8_t *dst_base = xip_runtime_packet_slot(dst_pkt, 0u, 1u, false);
    uint8_t *dst_hdr = xip_runtime_packet_slot(dst_pkt, 0u, (uint32_t)hdr, true);
    uint8_t *dst_ptr = xip_runtime_packet_slot(dst_pkt, dst_off, (uint32_t)out_len, true);
    if (!src_base || !src_ptr || !dst_base || !dst_hdr || !dst_ptr || src_base == dst_base) return XIP_ERR_ARG;

    uint8_t key_buf[32];
    memcpy(key_buf, dst_hdr, key_len);
    aead_lock();
    memcpy(aead_scratch.nonce, dst_hdr + key_len, 12u);
    const uint8_t *aad_ptr = NULL;
    if (aad_len > 0u) {
        if (aad_len <= sizeof(aead_scratch.aad)) {
            memcpy(aead_scratch.aad, dst_hdr + key_len + 12u, aad_len);
            aad_ptr = aead_scratch.aad;
        } else {
            aad_ptr = dst_hdr + key_len + 12u;
        }
    }
    mbedtls_platform_zeroize(dst_hdr, key_len + 12u);

    const uint8_t *in_ptr = aead_scratch.in;
    bool used_in_scratch = false;
    if (plain_len > 0u) {
        if (((uintptr_t)src_ptr & 63u) == 0u) {
            in_ptr = src_ptr;
        } else {
            memcpy(aead_scratch.in, src_ptr, plain_len);
            used_in_scratch = true;
        }
    }

    uint8_t *out_ptr = aead_scratch.out;
    bool used_out_direct = false;
    if (plain_len > 0u && ((uintptr_t)dst_ptr & 63u) == 0u) {
        out_ptr = dst_ptr;
        used_out_direct = true;
    }

    int rc = 0;
    if (!is_open) {
        rc = aead_dispatch(ctx, alg, 1, key_buf, key_len, aead_scratch.nonce, aad_ptr, aad_len,
                           in_ptr, out_ptr, plain_len, NULL, aead_scratch.tag);
        if (rc == 0) {
            if (!used_out_direct && plain_len > 0u) memcpy(dst_ptr, out_ptr, plain_len);
            memcpy(dst_ptr + plain_len, aead_scratch.tag, 16u);
        }
    } else {
        memcpy(aead_scratch.tag, src_ptr + plain_len, 16u);
        rc = aead_dispatch(ctx, alg, 0, key_buf, key_len, aead_scratch.nonce, aad_ptr, aad_len,
                           in_ptr, out_ptr, plain_len, aead_scratch.tag, NULL);
        if (rc == 0) {
            if (!used_out_direct && plain_len > 0u) memcpy(dst_ptr, out_ptr, plain_len);
        } else if (used_out_direct && plain_len > 0u) {
            mbedtls_platform_zeroize(dst_ptr, plain_len);
        }
    }

    if (aad_len > sizeof(aead_scratch.aad)) mbedtls_platform_zeroize(dst_hdr + key_len + 12u, aad_len);
    if (used_in_scratch && plain_len > 0u) mbedtls_platform_zeroize(aead_scratch.in, plain_len);
    if (!used_out_direct && plain_len > 0u) mbedtls_platform_zeroize(aead_scratch.out, plain_len);
    mbedtls_platform_zeroize(key_buf, sizeof(key_buf));
    aead_unlock();
    return rc == 0 ? (int)out_len : rc;
}

const xip_crypto *xip_crypto_component(void)
{
    static const xip_crypto table = {
        .version = 1, .size = sizeof(xip_crypto), .hash = c_hash, .hash_start = c_hash_start, .hash_update = c_hash_update,
        .hash_finish = c_hash_finish, .hash_abort = c_hash_abort, .verify = c_verify, .random = c_random,
        .aead_seal = c_aead_seal, .aead_open = c_aead_open, .aead_pkt = c_aead_pkt,
    };
    return &table;
}
