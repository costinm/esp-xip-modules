/* SPDX-License-Identifier: Apache-2.0 */
#pragma once
/* Hand-maintained native PSA table for ESP-IDF 6.0.2. Independent of ABI.md.
 * Include psa/crypto.h first in SDK code; standalone modules use the copied
 * declarations below. The host checks their layout against its real SDK.
 * No SDK function has an added host/user argument. */

/*
 PSA is used by the WPA implementation - the function in this list are added anyways
 by the WPA2, and relatively large, no need to duplicate them.
 
 PSA is similar and can be used with TPM/2 (on host). "Key ID" is the "key handle", 
 policies, exporting keys. ESP32 doesn't have a TPM AFAIK - but if they add it'll likely
 work with this API.
*/
 
#include <stddef.h>
#include <stdint.h>

/* The software SHA-512 member determines the driver's union size in this SDK
 * configuration. These private bytes are only initialized to zero by modules. */
typedef struct {
    uint64_t total[2], state[8];
    unsigned char buffer[128];
    int is384;
} espmod_psa_sha512_layout;
typedef struct {
    uint32_t alg;
    espmod_psa_sha512_layout ctx;
} espmod_psa_builtin_hash_layout;
typedef union {
    unsigned dummy;
    espmod_psa_builtin_hash_layout mbedtls_ctx;
    struct { void *sha_ctx; int sha_type; } esp_ctx;
} espmod_psa_driver_hash_layout;
typedef struct {
    unsigned id;
    espmod_psa_driver_hash_layout ctx;
} espmod_psa_hash_layout;
typedef struct { uint32_t usage, alg, alg2; } espmod_psa_key_policy_layout;
typedef struct {
    uint16_t type, bits;
    uint32_t lifetime;
    espmod_psa_key_policy_layout policy;
    uint32_t id;
} espmod_psa_key_attributes_layout;

#ifndef PSA_CRYPTO_H
typedef int32_t psa_status_t;
typedef uint32_t psa_algorithm_t;
typedef uint32_t psa_key_id_t;
typedef uint32_t mbedtls_svc_key_id_t;
typedef uint16_t psa_key_type_t;
typedef uint32_t psa_key_usage_t;
/* Multipart cipher/MAC contexts come from the native SDK header. Forward
 * declarations preserve the exact signatures without guessing their storage.
 * Standalone consumers must supply SDK-matching bindings to allocate them. */
typedef struct psa_cipher_operation_s psa_cipher_operation_t;
typedef struct psa_mac_operation_s psa_mac_operation_t;
typedef espmod_psa_hash_layout psa_hash_operation_t;
typedef espmod_psa_key_attributes_layout psa_key_attributes_t;
#define PSA_SUCCESS ((psa_status_t)0)
#define PSA_ALG_SHA_256 ((psa_algorithm_t)0x02000009)
#define PSA_ALG_ECDSA(hash) ((psa_algorithm_t)(0x06000600 | ((hash) & 0xff)))
#define PSA_ECC_FAMILY_SECP_R1 0x12
#define PSA_KEY_TYPE_ECC_PUBLIC_KEY(family) ((psa_key_type_t)(0x4100 | (family)))
#define PSA_KEY_USAGE_VERIFY_MESSAGE ((psa_key_usage_t)0x00000800)
#define PSA_KEY_USAGE_VERIFY_HASH ((psa_key_usage_t)0x00002000)
#define PSA_HASH_OPERATION_INIT {0, {0}}
#define PSA_KEY_ATTRIBUTES_INIT {0}
static inline void psa_set_key_type(psa_key_attributes_t *a, psa_key_type_t t) { a->type = t; }
static inline void psa_set_key_bits(psa_key_attributes_t *a, size_t bits) { a->bits = bits > 0xfff8 ? 0xffff : (uint16_t)bits; }
static inline void psa_set_key_usage_flags(psa_key_attributes_t *a, psa_key_usage_t usage) {
    a->policy.usage = usage | ((usage & 0x00001000) ? 0x00000400 : 0) | ((usage & PSA_KEY_USAGE_VERIFY_HASH) ? PSA_KEY_USAGE_VERIFY_MESSAGE : 0);
}
static inline void psa_set_key_algorithm(psa_key_attributes_t *a, psa_algorithm_t alg) { a->policy.alg = alg; }
#endif

#define ESPMOD_PSA_VERSION 1u
typedef struct espmod_psa {
    uint32_t version, size;
    psa_status_t (*psa_hash_compute)(psa_algorithm_t alg, const uint8_t *input, size_t input_length,
                                   uint8_t *hash, size_t hash_size, size_t *hash_length);
    psa_status_t (*psa_hash_setup)(psa_hash_operation_t *operation, psa_algorithm_t alg);
    psa_status_t (*psa_hash_update)(psa_hash_operation_t *operation, const uint8_t *input, size_t input_length);
    psa_status_t (*psa_hash_finish)(psa_hash_operation_t *operation, uint8_t *hash, size_t hash_size, size_t *hash_length);
    psa_status_t (*psa_hash_abort)(psa_hash_operation_t *operation);
    psa_status_t (*psa_import_key)(const psa_key_attributes_t *attributes, const uint8_t *data,
                                 size_t data_length, mbedtls_svc_key_id_t *key);
    psa_status_t (*psa_destroy_key)(mbedtls_svc_key_id_t key);
    psa_status_t (*psa_verify_hash)(mbedtls_svc_key_id_t key, psa_algorithm_t alg, const uint8_t *hash,
                                  size_t hash_length, const uint8_t *signature, size_t signature_length);
    psa_status_t (*psa_generate_random)(uint8_t *output,
            size_t output_size);
    psa_status_t (*psa_generate_key)(const psa_key_attributes_t *attributes,
            mbedtls_svc_key_id_t *key);
    psa_status_t (*psa_export_public_key)(mbedtls_svc_key_id_t key,
            uint8_t *data,
            size_t data_size,
            size_t *data_length);
    psa_status_t (*psa_raw_key_agreement)(psa_algorithm_t alg,
            mbedtls_svc_key_id_t private_key,
            const uint8_t *peer_key,
            size_t peer_key_length,
            uint8_t *output,
            size_t output_size,
            size_t *output_length);
    void (*psa_reset_key_attributes)(psa_key_attributes_t *attributes);
    psa_status_t (*psa_cipher_encrypt_setup)(psa_cipher_operation_t *operation,
            mbedtls_svc_key_id_t key,
            psa_algorithm_t alg);
    psa_status_t (*psa_cipher_decrypt_setup)(psa_cipher_operation_t *operation,
            mbedtls_svc_key_id_t key,
            psa_algorithm_t alg);
    psa_status_t (*psa_cipher_set_iv)(psa_cipher_operation_t *operation,
            const uint8_t *iv,
            size_t iv_length);
    psa_status_t (*psa_cipher_update)(psa_cipher_operation_t *operation,
            const uint8_t *input,
            size_t input_length,
            uint8_t *output,
            size_t output_size,
            size_t *output_length);
    psa_status_t (*psa_cipher_finish)(psa_cipher_operation_t *operation,
            uint8_t *output,
            size_t output_size,
            size_t *output_length);
    psa_status_t (*psa_cipher_abort)(psa_cipher_operation_t *operation);
    psa_status_t (*psa_mac_sign_setup)(psa_mac_operation_t *operation,
            mbedtls_svc_key_id_t key,
            psa_algorithm_t alg);
    psa_status_t (*psa_mac_update)(psa_mac_operation_t *operation,
            const uint8_t *input,
            size_t input_length);
    psa_status_t (*psa_mac_sign_finish)(psa_mac_operation_t *operation,
            uint8_t *mac,
            size_t mac_size,
            size_t *mac_length);
    psa_status_t (*psa_mac_abort)(psa_mac_operation_t *operation);
} espmod_psa;

