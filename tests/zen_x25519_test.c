/*
 * Value-level X25519 gate. The ClientHello parity harness only checks the
 * shape of the key shares (lengths and order), and tests/integration.sh runs
 * with verification disabled, so neither would catch a wrong ladder. This
 * test pins RFC 7748 §5.2 / §6.1 and cross-checks random key generation and
 * shared secrets against the Mbed TLS ECP path the transport used before.
 */
#include "x25519.h"

#include <mbedtls/ecp.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static int hexval(int c)
{
    if (c >= '0' && c <= '9') return c - '0';
    if (c >= 'a' && c <= 'f') return c - 'a' + 10;
    if (c >= 'A' && c <= 'F') return c - 'A' + 10;
    return -1;
}

static int unhex(const char *s, uint8_t *out, size_t n)
{
    for (size_t i = 0; i < n; i++) {
        int hi = hexval(s[2 * i]), lo = hexval(s[2 * i + 1]);
        if (hi < 0 || lo < 0) return -1;
        out[i] = (uint8_t)((hi << 4) | lo);
    }
    return 0;
}

static void dump(const char *label, const uint8_t *p, size_t n)
{
    printf("%s ", label);
    for (size_t i = 0; i < n; i++) printf("%02x", p[i]);
    printf("\n");
}

static int kat(const char *scalar_hex, const char *point_hex,
               const char *want_hex)
{
    uint8_t scalar[32], point[32], want[32], got[32];
    if (unhex(scalar_hex, scalar, 32) || unhex(point_hex, point, 32)
        || unhex(want_hex, want, 32)) {
        return -1;
    }
    zen_x25519_scalarmult(got, scalar, point);
    if (memcmp(got, want, 32) != 0) {
        dump("want", want, 32);
        dump("got ", got, 32);
        return -1;
    }
    return 0;
}

static uint32_t rng_state = 0x12345678u;

static int test_rng(void *ctx, unsigned char *buf, size_t len)
{
    (void)ctx;
    for (size_t i = 0; i < len; i++) {
        rng_state ^= rng_state << 13;
        rng_state ^= rng_state >> 17;
        rng_state ^= rng_state << 5;
        buf[i] = (unsigned char)rng_state;
    }
    return 0;
}

static int mbed_pub(uint8_t out[32], const uint8_t scalar[32])
{
    mbedtls_ecp_group grp;
    mbedtls_mpi d;
    mbedtls_ecp_point q;
    size_t olen = 0;
    int ret;
    mbedtls_ecp_group_init(&grp);
    mbedtls_mpi_init(&d);
    mbedtls_ecp_point_init(&q);
    ret = mbedtls_ecp_group_load(&grp, MBEDTLS_ECP_DP_CURVE25519);
    /* X25519 scalars are little-endian; the MPI reader is big-endian. */
    if (ret == 0) ret = mbedtls_mpi_read_binary_le(&d, scalar, 32);
    if (ret == 0) ret = mbedtls_ecp_mul(&grp, &q, &d, &grp.G, test_rng, NULL);
    if (ret == 0) ret = mbedtls_ecp_point_write_binary(&grp, &q,
                                                       MBEDTLS_ECP_PF_UNCOMPRESSED,
                                                       &olen, out, 32);
    mbedtls_ecp_point_free(&q);
    mbedtls_mpi_free(&d);
    mbedtls_ecp_group_free(&grp);
    if (ret != 0 || olen != 32) return -1;
    return 0;
}

static int mbed_shared(uint8_t out[32], const uint8_t scalar[32],
                       const uint8_t peer[32])
{
    mbedtls_ecp_group grp;
    mbedtls_mpi d;
    mbedtls_ecp_point q, p;
    size_t olen = 0;
    int ret;
    mbedtls_ecp_group_init(&grp);
    mbedtls_mpi_init(&d);
    mbedtls_ecp_point_init(&q);
    mbedtls_ecp_point_init(&p);
    ret = mbedtls_ecp_group_load(&grp, MBEDTLS_ECP_DP_CURVE25519);
    if (ret == 0) ret = mbedtls_mpi_read_binary_le(&d, scalar, 32);
    if (ret == 0) ret = mbedtls_ecp_point_read_binary(&grp, &p, peer, 32);
    if (ret == 0) ret = mbedtls_ecp_mul(&grp, &q, &d, &p, test_rng, NULL);
    if (ret == 0) ret = mbedtls_ecp_point_write_binary(&grp, &q,
                                                       MBEDTLS_ECP_PF_UNCOMPRESSED,
                                                       &olen, out, 32);
    mbedtls_ecp_point_free(&p);
    mbedtls_ecp_point_free(&q);
    mbedtls_mpi_free(&d);
    mbedtls_ecp_group_free(&grp);
    if (ret != 0 || olen != 32) return -1;
    return 0;
}

static int crosscheck(int rounds)
{
    for (int i = 0; i < rounds; i++) {
        uint8_t a[32], b[32], apub[32], bpub[32];
        uint8_t ours[32], theirs[32];
        for (int j = 0; j < 32; j++) {
            a[j] = (uint8_t)rand();
            b[j] = (uint8_t)rand();
        }
        a[0] &= 248; a[31] &= 127; a[31] |= 64;
        b[0] &= 248; b[31] &= 127; b[31] |= 64;

        zen_x25519_base(apub, a);
        zen_x25519_base(bpub, b);
        if (mbed_pub(theirs, a) != 0) {
            printf("mbed_pub failed at %d\n", i);
            return -1;
        }
        if (memcmp(apub, theirs, 32) != 0) {
            printf("public key mismatch at %d\n", i);
            dump("ours", apub, 32);
            dump("mbed", theirs, 32);
            return -1;
        }

        zen_x25519_scalarmult(ours, a, bpub);
        if (mbed_shared(theirs, a, bpub) != 0) {
            printf("mbed_shared failed at %d\n", i);
            return -1;
        }
        if (memcmp(ours, theirs, 32) != 0) {
            printf("shared secret mismatch at %d\n", i);
            dump("ours", ours, 32);
            dump("mbed", theirs, 32);
            return -1;
        }
    }
    return 0;
}

int main(void)
{
    if (kat("a546e36bf0527c9d3b16154b82465edd62144c0ac1fc5a18506a2244ba449ac4",
            "e6db6867583030db3594c1a424b15f7c726624ec26b3353b10a903a6d0ab1c4c",
            "c3da55379de9c6908e94ea4df28d084f32eccf03491c71f754b4075577a28552") != 0) {
        puts("RFC 7748 vector 1 failed");
        return 1;
    }
    if (kat("4b66e9d4d1b4673c5ad22691957d6af5c11b6421e0ea01d42ca4169e7918ba0d",
            "e5210f12786811d3f4b7959d0538ae2c31dbe7106fc03c3efc4cd549c715a493",
            "95cbde9476e8907d7aade45cb4b873f88b595a68799fa152e6f8f7647aac7957") != 0) {
        puts("RFC 7748 vector 2 failed");
        return 1;
    }
    {
        uint8_t apriv[32], bpriv[32], apub[32], bpub[32], shared[32], want[32];
        if (unhex("77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a",
                  apriv, 32)
            || unhex("5dab087e624a8a4b79e17f8b83800ee66f3bb1292618b6fd1c2f8b27ff88e0eb",
                     bpriv, 32)
            || unhex("8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a",
                     want, 32)) {
            puts("RFC 7748 §6.1 decode failed");
            return 1;
        }
        zen_x25519_base(apub, apriv);
        if (memcmp(apub, want, 32) != 0) {
            puts("RFC 7748 §6.1 Alice public key mismatch");
            return 1;
        }
        if (unhex("de9edb7d7b7dc1b4d35b61c2ece435373f8343c85b78674dadfc7e146f882b4f",
                  want, 32) != 0) {
            puts("RFC 7748 §6.1 decode failed");
            return 1;
        }
        zen_x25519_base(bpub, bpriv);
        if (memcmp(bpub, want, 32) != 0) {
            puts("RFC 7748 §6.1 Bob public key mismatch");
            return 1;
        }
        if (unhex("4a5d9d5ba4ce2de1728e3bf480350f25e07e21c947d19e3376f09b3c1e161742",
                  want, 32) != 0) {
            puts("RFC 7748 §6.1 decode failed");
            return 1;
        }
        zen_x25519_scalarmult(shared, apriv, bpub);
        if (memcmp(shared, want, 32) != 0) {
            puts("RFC 7748 §6.1 shared secret mismatch");
            return 1;
        }
    }
    if (crosscheck(64) != 0) {
        puts("x25519 cross-check failed");
        return 1;
    }
    puts("zen_x25519_test: ok");
    return 0;
}
