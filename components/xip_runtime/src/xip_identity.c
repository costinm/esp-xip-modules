/* Device identity mechanism only: wire format and announcement policy belong
 * to modules. Use the firmware's existing key; never expose it through NV. */
/* IDF 6 exposes the SDK MPI/ECP declarations behind this opt-in. Needed only
 * to normalize compressed SEC1 wire keys before standard PSA verification. */
#define MBEDTLS_DECLARE_PRIVATE_IDENTIFIERS
#include "xip.h"
#include "nvs.h"
#include "psa/crypto.h"
#include "mbedtls/platform_util.h"
#include "mbedtls/private/ecp.h"
#include "mbedtls/private/bignum.h"
#include <string.h>

static int open_key(mbedtls_svc_key_id_t *key)
{
    nvs_handle_t ns;
    if (nvs_open("sec", NVS_READWRITE, &ns) != ESP_OK) return XIP_ERR_IO;
    uint8_t private[32] = {0};
    size_t n = sizeof(private);
    esp_err_t rc = nvs_get_blob(ns, "id_p256", private, &n);
    psa_key_attributes_t attrs = PSA_KEY_ATTRIBUTES_INIT;
    psa_set_key_type(&attrs, PSA_KEY_TYPE_ECC_KEY_PAIR(PSA_ECC_FAMILY_SECP_R1));
    psa_set_key_bits(&attrs, 256);
    psa_set_key_usage_flags(&attrs, PSA_KEY_USAGE_SIGN_HASH | PSA_KEY_USAGE_EXPORT);
    psa_set_key_algorithm(&attrs, PSA_ALG_ECDSA(PSA_ALG_SHA_256));
    psa_status_t status;
    if (rc == ESP_ERR_NVS_NOT_FOUND) {
        status = psa_generate_key(&attrs, key);
        if (status == PSA_SUCCESS) {
            status = psa_export_key(*key, private, sizeof(private), &n);
            if (status == PSA_SUCCESS && n == sizeof(private)) {
                rc = nvs_set_blob(ns, "id_p256", private, n);
                if (rc == ESP_OK) rc = nvs_commit(ns);
            } else rc = ESP_FAIL;
            if (rc != ESP_OK) { psa_destroy_key(*key); status = PSA_ERROR_STORAGE_FAILURE; }
        }
    } else if (rc == ESP_OK && n == sizeof(private)) {
        status = psa_import_key(&attrs, private, n, key);
    } else status = PSA_ERROR_STORAGE_FAILURE;
    mbedtls_platform_zeroize(private, sizeof(private));
    psa_reset_key_attributes(&attrs);
    nvs_close(ns);
    return status == PSA_SUCCESS ? 0 : XIP_ERR_IO;
}
extern uint8_t *xip_runtime_packet_range(uint32_t pkt, uint32_t offset, uint32_t len, bool write);

static int public_key(void *ctx, uint32_t pkt, uint32_t off)
{
    (void)ctx;
    uint8_t *out = xip_runtime_packet_range(pkt, off, 65, true);
    if (!out) return XIP_ERR_ARG;
    mbedtls_svc_key_id_t key;
    int rc = open_key(&key);
    if (rc) return rc;
    size_t n = 0;
    psa_status_t status = psa_export_public_key(key, out, 65, &n);
    psa_status_t cleanup = psa_destroy_key(key);
    return status == PSA_SUCCESS && cleanup == PSA_SUCCESS && n == 65 ? 0 : XIP_ERR_IO;
}
static int sign_message(void *ctx, uint32_t pkt, uint32_t off, uint32_t len, uint32_t out_pkt, uint32_t out_off)
{
    (void)ctx;
    const uint8_t *data = xip_runtime_packet_range(pkt, off, len, false);
    uint8_t *signature = xip_runtime_packet_range(out_pkt, out_off, 64, true);
    if ((!data && len) || !signature) return XIP_ERR_ARG;
    mbedtls_svc_key_id_t key;
    int rc = open_key(&key);
    if (rc) return rc;
    uint8_t hash[32]; size_t n = 0;
    psa_status_t status = psa_hash_compute(PSA_ALG_SHA_256, data, len, hash, sizeof(hash), &n);
    if (status == PSA_SUCCESS && n == sizeof(hash))
        status = psa_sign_hash(key, PSA_ALG_ECDSA(PSA_ALG_SHA_256), hash, n, signature, 64, &n);
    else status = PSA_ERROR_GENERIC_ERROR;
    psa_status_t cleanup = psa_destroy_key(key);
    return status == PSA_SUCCESS && cleanup == PSA_SUCCESS && n == 64 ? 0 : XIP_ERR_IO;
}
/* Native MPI decompression: y^2=x^3-3x+B, p=3 mod 4. SDK PSA public-key
 * import need not support SEC1 compressed points, which are the DMesh wire key. */
int xip_identity_expand_public(const uint8_t *in, size_t len, uint8_t out[65])
{
    if (len == 65 && in[0] == 4) { memcpy(out, in, 65); return 0; }
    if (len != 33 || (in[0] != 2 && in[0] != 3)) return XIP_ERR_ARG;
    mbedtls_ecp_group g; mbedtls_ecp_group_init(&g);
    mbedtls_mpi x, y, rhs, exp, check;
    mbedtls_mpi_init(&x); mbedtls_mpi_init(&y); mbedtls_mpi_init(&rhs);
    mbedtls_mpi_init(&exp); mbedtls_mpi_init(&check);
    int rc = mbedtls_ecp_group_load(&g, MBEDTLS_ECP_DP_SECP256R1);
#define STEP(expr) do { if (!rc) rc = (expr); } while (0)
    STEP(mbedtls_mpi_read_binary(&x, in + 1, 32));
    if (!rc && mbedtls_mpi_cmp_mpi(&x, &g.P) >= 0) rc = -1;
    STEP(mbedtls_mpi_mul_mpi(&rhs, &x, &x));
    STEP(mbedtls_mpi_mod_mpi(&rhs, &rhs, &g.P));
    STEP(mbedtls_mpi_mul_mpi(&rhs, &rhs, &x));
    STEP(mbedtls_mpi_mul_int(&y, &x, 3));
    STEP(mbedtls_mpi_sub_mpi(&rhs, &rhs, &y));
    STEP(mbedtls_mpi_add_mpi(&rhs, &rhs, &g.B));
    STEP(mbedtls_mpi_mod_mpi(&rhs, &rhs, &g.P));
    STEP(mbedtls_mpi_add_int(&exp, &g.P, 1));
    STEP(mbedtls_mpi_shift_r(&exp, 2));
    STEP(mbedtls_mpi_exp_mod(&y, &rhs, &exp, &g.P, NULL));
    STEP(mbedtls_mpi_mul_mpi(&check, &y, &y));
    STEP(mbedtls_mpi_mod_mpi(&check, &check, &g.P));
    if (!rc && mbedtls_mpi_cmp_mpi(&check, &rhs)) rc = -1;
    if (!rc && mbedtls_mpi_get_bit(&y, 0) != (in[0] & 1)) rc = mbedtls_mpi_sub_mpi(&y, &g.P, &y);
    out[0] = 4; memcpy(out + 1, in + 1, 32);
    STEP(mbedtls_mpi_write_binary(&y, out + 33, 32));
#undef STEP
    mbedtls_mpi_free(&x); mbedtls_mpi_free(&y); mbedtls_mpi_free(&rhs);
    mbedtls_mpi_free(&exp); mbedtls_mpi_free(&check); mbedtls_ecp_group_free(&g);
    return rc ? XIP_ERR_ARG : 0;
}
const xip_identity *xip_identity_component(void)
{
    static const xip_identity table = {
        .version = 1, .size = sizeof(xip_identity), .public_key = public_key, .sign = sign_message,
    };
    return &table;
}
