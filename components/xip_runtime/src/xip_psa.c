/* SPDX-License-Identifier: Apache-2.0 */
#include <stddef.h>
#include "psa/crypto.h"
#include "xip_psa.h"

/* Fail the normal firmware build if copied module declarations no longer match
 * the SDK. No guessed context capacity or runtime buffer size negotiation. */
_Static_assert(sizeof(psa_hash_operation_t) == sizeof(xip_psa_hash_layout), "PSA hash layout changed");
_Static_assert(_Alignof(psa_hash_operation_t) == _Alignof(xip_psa_hash_layout), "PSA hash alignment changed");
_Static_assert(offsetof(psa_hash_operation_t, MBEDTLS_PRIVATE(ctx)) == offsetof(xip_psa_hash_layout, ctx), "PSA hash offset changed");
_Static_assert(sizeof(psa_key_attributes_t) == sizeof(xip_psa_key_attributes_layout), "PSA attributes changed");
_Static_assert(_Alignof(psa_key_attributes_t) == _Alignof(xip_psa_key_attributes_layout), "PSA attributes alignment changed");
_Static_assert(offsetof(psa_key_attributes_t, MBEDTLS_PRIVATE(policy)) == offsetof(xip_psa_key_attributes_layout, policy), "PSA policy offset changed");
_Static_assert(offsetof(psa_key_attributes_t, MBEDTLS_PRIVATE(id)) == offsetof(xip_psa_key_attributes_layout, id), "PSA key id offset changed");
_Static_assert(sizeof(mbedtls_svc_key_id_t) == sizeof(uint32_t), "PSA key id encoding changed");

/* ESP-IDF initializes PSA during startup. Lookup only returns this table. */
const xip_psa *xip_psa_component(void)
{
    static const xip_psa table = {
        .version = XIP_PSA_VERSION, .size = sizeof(xip_psa),
        .psa_hash_compute = psa_hash_compute, .psa_hash_setup = psa_hash_setup,
        .psa_hash_update = psa_hash_update, .psa_hash_finish = psa_hash_finish,
        .psa_hash_abort = psa_hash_abort, .psa_import_key = psa_import_key,
        .psa_destroy_key = psa_destroy_key, .psa_verify_hash = psa_verify_hash,
        .psa_generate_random = psa_generate_random,
        .psa_generate_key = psa_generate_key,
        .psa_export_public_key = psa_export_public_key,
        .psa_raw_key_agreement = psa_raw_key_agreement,
        .psa_reset_key_attributes = psa_reset_key_attributes,
        .psa_cipher_encrypt_setup = psa_cipher_encrypt_setup,
        .psa_cipher_decrypt_setup = psa_cipher_decrypt_setup,
        .psa_cipher_set_iv = psa_cipher_set_iv,
        .psa_cipher_update = psa_cipher_update,
        .psa_cipher_finish = psa_cipher_finish,
        .psa_cipher_abort = psa_cipher_abort,
        .psa_mac_sign_setup = psa_mac_sign_setup,
        .psa_mac_update = psa_mac_update,
        .psa_mac_sign_finish = psa_mac_sign_finish,
        .psa_mac_abort = psa_mac_abort,
    };
    return &table;
}
