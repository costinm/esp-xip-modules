/* SPDX-License-Identifier: Apache-2.0 */
/* The `crypto` component on the ESP: calls on pool packets, answered with the SDK's PSA. No PSA type crosses the module ABI; an operation in
 * progress is a small handle owned here and released by finish, abort or the end of the owning instance. NOT BUILT in the host tree (needs
 * ESP-IDF): the host runtime's crates/xip-runtime-host/src/crypto.rs is the reference with tests. */
#include "xip.h"
#include "psa/crypto.h"
#include "mbedtls/platform_util.h"
#include <string.h>

/* xip_runtime.c: the bytes [offset, offset+len) of the payload window of a packet the caller holds, or NULL. `write` needs it unshared. */
extern uint8_t *xip_runtime_packet_range(uint32_t pkt, uint32_t offset, uint32_t len, bool write);

#define HASHES_MAX 4
static struct { void *owner; psa_hash_operation_t op; uint32_t id; } hashes[HASHES_MAX];
static uint32_t next_id = 1;

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
/* The instance ended holding hashes: release them (called by the runtime). */
void xip_crypto_release(void *owner)
{
    for (int i = 0; i < HASHES_MAX; i++)
        if (hashes[i].id && hashes[i].owner == owner) { psa_hash_abort(&hashes[i].op); hashes[i].id = 0; hashes[i].owner = NULL; }
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

const xip_crypto *xip_crypto_component(void)
{
    static const xip_crypto table = {
        .version = 1, .size = sizeof(xip_crypto), .hash = c_hash, .hash_start = c_hash_start, .hash_update = c_hash_update,
        .hash_finish = c_hash_finish, .hash_abort = c_hash_abort, .verify = c_verify, .random = c_random,
    };
    return &table;
}
