/*
 * zen-client-c: TLS 1.3 client for the OpenCode free transport.
 *
 * This is a direct port of the proven /tmp/zen-c-spike/tls_spike.c spike
 * (20/20 live handshakes). The wire shape is deliberately kept identical:
 *
 *   - record version 0x0301 for the ClientHello, 0x0303 afterwards
 *   - 17 ciphers, 13 extensions in the captured Bun 1.4.4 order
 *   - ALPN http/1.1, groups 11ec/001d/0017/0018
 *   - key shares 11ec:1216 (ML-KEM-768 pk || X25519) and 001d:32
 *   - 11 signature algorithms, supported_versions 0304/0303
 *   - a 32-byte legacy session id
 *
 * The distinct X25519 key in the standalone 0x001d share is kept: the hybrid
 * share uses its own X25519 keypair, exactly like Bun. The 0x001d keypair is
 * also used if a server ever ignores 0x11ec and selects the standalone group.
 *
 * All three TLS 1.3 suites are implemented: TLS_AES_128_GCM_SHA256 (0x1301),
 * TLS_AES_256_GCM_SHA384 (0x1302) and TLS_CHACHA20_POLY1305_SHA256 (0x1303).
 * The advertised list must stay byte-identical to Bun's ClientHello.
 *
 * PAIG's vendored Mbed TLS does not build library/hkdf.c (sources.txt), so
 * RFC 5869 HKDF is supplied here over mbedtls_md_hmac() as in the spike.
 */
#include "zen.h"

#include <errno.h>
#include <stdlib.h>
#include <string.h>
#include <sys/random.h>

#include <mbedtls/chachapoly.h>
#include <mbedtls/gcm.h>
#include <mbedtls/md.h>
#include <mbedtls/pk.h>
#include <mbedtls/rsa.h>
#include <mbedtls/sha256.h>
#include <mbedtls/sha512.h>
#include <mbedtls/x509_crt.h>

#include "kem.h"
#include "x25519.h"

/* zen.h deliberately keeps struct ztls opaque; the .c-side alias keeps the
 * internal helpers readable without changing the public declarations. */
typedef struct ztls ztls;

#define MAX_TRANSCRIPT (128 * 1024)
#define MAX_HS (128 * 1024)
#define MAX_REC 18432
#define MAX_PLAIN 16384
#define MAX_SNI 253
#define MAX_HASH 48 /* SHA-384, the largest TLS 1.3 suite hash we offer */

/* ---- exact Bun 1.4.4 ClientHello constants (from /tmp/hello_sni.out) ---- */
static const uint16_t bun_ciphers[] = {
    0x1301, 0x1302, 0x1303,
    0xc02b, 0xc02f, 0xc02c, 0xc030, 0xcca9, 0xcca8,
    0xc009, 0xc013, 0xc00a, 0xc014, 0x009c, 0x009d, 0x002f, 0x0035,
};
static const uint16_t bun_sigalgs[] = {
    0x0403, 0x0804, 0x0401, 0x0503, 0x0805, 0x0501,
    0x0806, 0x0601, 0x0807, 0x0603, 0x0201,
};
static const uint16_t bun_groups[] = { 0x11ec, 0x001d, 0x0017, 0x0018 };

#define MLKEM_PK_LEN 1184
#define MLKEM_SK_LEN 2400
#define MLKEM_CT_LEN 1088
#define MLKEM_SS_LEN 32
#define X25519_LEN 32
#define HYBRID_CLIENT_SHARE (MLKEM_PK_LEN + X25519_LEN) /* 1216 */
#define HYBRID_SERVER_SHARE (MLKEM_CT_LEN + X25519_LEN) /* 1120 */

/* ------------------------------ RNG ------------------------------ */
static int rng_fill(uint8_t *buf, size_t len)
{
    while (len > 0) {
        ssize_t n = getrandom(buf, len, 0);
        if (n < 0) {
            if (errno == EINTR) continue;
            return -1;
        }
        buf += (size_t)n;
        len -= (size_t)n;
    }
    return 0;
}

/* -------------------------- byte helpers -------------------------- */
static void put16(uint8_t *p, uint16_t v)
{
    p[0] = (uint8_t)(v >> 8);
    p[1] = (uint8_t)v;
}

static void put24(uint8_t *p, uint32_t v)
{
    p[0] = (uint8_t)(v >> 16);
    p[1] = (uint8_t)(v >> 8);
    p[2] = (uint8_t)v;
}

static uint16_t get16(const uint8_t *p)
{
    return (uint16_t)((p[0] << 8) | p[1]);
}

static uint32_t get24(const uint8_t *p)
{
    return ((uint32_t)p[0] << 16) | ((uint32_t)p[1] << 8) | p[2];
}

/* --------------------------- X25519 --------------------------- */
/* The RFC 7748 ladder lives in x25519.c. Mbed TLS ECP stays linked for
 * certificate verification only; the two client key shares and the shared
 * secret no longer go through MPI or the heap. */
typedef struct {
    uint8_t secret[X25519_LEN];
} x25519_key;

static int x25519_init(x25519_key *k)
{
    memset(k, 0, sizeof(*k));
    return 0;
}

static void x25519_free(x25519_key *k)
{
    volatile uint8_t *p = k->secret;
    for (size_t i = 0; i < sizeof(k->secret); i++) {
        p[i] = 0;
    }
}

static int x25519_generate(x25519_key *k, uint8_t pub[X25519_LEN])
{
    if (rng_fill(k->secret, X25519_LEN) != 0) {
        return -1;
    }
    zen_x25519_base(pub, k->secret);
    return 0;
}

static int x25519_shared(x25519_key *k, const uint8_t peer[X25519_LEN],
                         uint8_t out[X25519_LEN])
{
    uint8_t p[X25519_LEN];
    memcpy(p, peer, X25519_LEN);
    p[31] &= 127;
    zen_x25519_scalarmult(out, k->secret, p);
    if (zen_x25519_is_zero(out)) {
        zen_set_error("tls: X25519 low-order peer key");
        return -1;
    }
    return 0;
}

/* ---------------------------- HKDF ---------------------------- */
static const mbedtls_md_info_t *md_sha256(void)
{
    return mbedtls_md_info_from_type(MBEDTLS_MD_SHA256);
}

static const mbedtls_md_info_t *md_sha384(void)
{
    return mbedtls_md_info_from_type(MBEDTLS_MD_SHA384);
}

static const mbedtls_md_info_t *suite_md(uint16_t suite)
{
    return suite == 0x1302 ? md_sha384() : md_sha256();
}

static int hmac_md(const mbedtls_md_info_t *md, const uint8_t *key,
                   size_t key_len, const uint8_t *data, size_t data_len,
                   uint8_t *out)
{
    return mbedtls_md_hmac(md, key, key_len, data, data_len, out);
}

static int hkdf_extract(const mbedtls_md_info_t *md, const uint8_t *salt,
                        size_t salt_len, const uint8_t *ikm, size_t ikm_len,
                        uint8_t *out)
{
    uint8_t zeros[MAX_HASH] = {0};
    size_t hlen = mbedtls_md_get_size(md);
    if (hlen == 0 || hlen > MAX_HASH) return -1;
    if (salt == NULL || salt_len == 0) {
        salt = zeros;
        salt_len = hlen;
    }
    return hmac_md(md, salt, salt_len, ikm, ikm_len, out);
}

static int hkdf_expand(const mbedtls_md_info_t *md, const uint8_t *prk,
                       size_t prk_len, const uint8_t *info, size_t info_len,
                       uint8_t *out, size_t out_len)
{
    uint8_t t[MAX_HASH];
    size_t hlen = mbedtls_md_get_size(md);
    size_t t_len = 0, done = 0;
    uint8_t ctr = 1;
    if (hlen == 0 || hlen > MAX_HASH || info_len > 160) return -1;
    if (out_len > 255 * hlen) return -1;
    while (done < out_len) {
        uint8_t buf[MAX_HASH + 160 + 1];
        size_t n = 0;
        if (t_len) {
            memcpy(buf, t, t_len);
            n += t_len;
        }
        if (info_len) {
            memcpy(buf + n, info, info_len);
            n += info_len;
        }
        buf[n++] = ctr++;
        int ret = hmac_md(md, prk, prk_len, buf, n, t);
        if (ret != 0) return ret;
        t_len = hlen;
        size_t take = out_len - done < hlen ? out_len - done : hlen;
        memcpy(out + done, t, take);
        done += take;
    }
    return 0;
}

/* HKDF-Expand-Label(secret, label, context, outlen) */
static int expand_label(const mbedtls_md_info_t *md, const uint8_t *secret,
                        size_t secret_len, const char *label,
                        const uint8_t *ctx, size_t ctx_len,
                        uint8_t *out, size_t out_len)
{
    uint8_t info[2 + 1 + 6 + 64 + 1 + 64];
    size_t lab_len = strlen(label);
    size_t n = 0;
    if (lab_len > 64 || ctx_len > 64) return -1;
    put16(info + n, (uint16_t)out_len);
    n += 2;
    info[n++] = (uint8_t)(6 + lab_len);
    memcpy(info + n, "tls13 ", 6);
    n += 6;
    memcpy(info + n, label, lab_len);
    n += lab_len;
    info[n++] = (uint8_t)ctx_len;
    if (ctx_len) {
        memcpy(info + n, ctx, ctx_len);
        n += ctx_len;
    }
    return hkdf_expand(md, secret, secret_len, info, n, out, out_len);
}

static int sha256(const uint8_t *p, size_t n, uint8_t out[32])
{
    return mbedtls_sha256(p, n, out, 0);
}

static int sha384(const uint8_t *p, size_t n, uint8_t out[48])
{
    return mbedtls_sha512(p, n, out, 1);
}

static int sha512(const uint8_t *p, size_t n, uint8_t out[64])
{
    return mbedtls_sha512(p, n, out, 0);
}

/* ----------------------------- AEAD ----------------------------- */
typedef struct {
    int alg; /* 0 = AES-GCM, 1 = ChaCha20-Poly1305 */
    int key_bits;
    mbedtls_gcm_context gcm;
    mbedtls_chachapoly_context cp;
    uint8_t iv[12];
    uint64_t seq;
    int on;
} aead_ctx;

static int aead_init(aead_ctx *a, uint16_t suite, const uint8_t key[32],
                     const uint8_t iv[12])
{
    memset(a, 0, sizeof(*a));
    memcpy(a->iv, iv, 12);
    int ret;
    if (suite == 0x1301 || suite == 0x1302) {
        a->alg = 0;
        a->key_bits = (suite == 0x1301) ? 128 : 256;
        mbedtls_gcm_init(&a->gcm);
        ret = mbedtls_gcm_setkey(&a->gcm, MBEDTLS_CIPHER_ID_AES, key,
                                 (unsigned)a->key_bits);
        if (ret != 0) {
            mbedtls_gcm_free(&a->gcm);
            return ret;
        }
    } else if (suite == 0x1303) {
        a->alg = 1;
        a->key_bits = 256;
        mbedtls_chachapoly_init(&a->cp);
        ret = mbedtls_chachapoly_setkey(&a->cp, key);
        if (ret != 0) {
            mbedtls_chachapoly_free(&a->cp);
            return ret;
        }
    } else {
        return -1;
    }
    a->on = 1;
    return 0;
}

static void aead_free(aead_ctx *a)
{
    if (a->on) {
        if (a->alg == 0) mbedtls_gcm_free(&a->gcm);
        else mbedtls_chachapoly_free(&a->cp);
        a->on = 0;
    }
}

static void make_nonce(const aead_ctx *a, uint8_t nonce[12])
{
    memcpy(nonce, a->iv, 12);
    for (int i = 0; i < 8; i++)
        nonce[4 + i] ^= (uint8_t)(a->seq >> (56 - 8 * i));
}

/* Returns inner content type, or negative on failure. */
static int aead_open(aead_ctx *a, const uint8_t hdr[5], const uint8_t *ct,
                     size_t ct_len, uint8_t *out, size_t *out_len)
{
    if (!a->on || ct_len < 16) return -1;
    uint8_t nonce[12];
    make_nonce(a, nonce);
    size_t clen = ct_len - 16;
    int ret;
    if (a->alg == 0)
        ret = mbedtls_gcm_auth_decrypt(&a->gcm, clen, nonce, 12, hdr, 5,
                                       ct + clen, 16, ct, out);
    else
        ret = mbedtls_chachapoly_auth_decrypt(&a->cp, clen, nonce, hdr, 5,
                                              ct + clen, ct, out);
    if (ret != 0) return -2;
    a->seq++;
    size_t n = clen;
    while (n > 0 && out[n - 1] == 0) n--;
    if (n == 0) return -3;
    int inner = out[n - 1];
    *out_len = n - 1;
    return inner;
}

static int aead_seal(aead_ctx *a, uint8_t type, const uint8_t *pt,
                     size_t pt_len, uint8_t *rec, size_t rec_cap)
{
    size_t clen;
    uint8_t nonce[12];
    if (!a->on || pt_len > MAX_PLAIN) return -1;
    clen = pt_len + 1 + 16;
    if (rec_cap < 5 + clen) return -1;
    rec[0] = 0x17;
    rec[1] = 0x03;
    rec[2] = 0x03;
    put16(rec + 3, (uint16_t)clen);
    uint8_t *inner = rec + 5;
    memcpy(inner, pt, pt_len);
    inner[pt_len] = type;
    make_nonce(a, nonce);
    int ret;
    if (a->alg == 0)
        ret = mbedtls_gcm_crypt_and_tag(&a->gcm, MBEDTLS_GCM_ENCRYPT,
                                        pt_len + 1, nonce, 12, rec, 5,
                                        inner, inner, 16,
                                        inner + pt_len + 1);
    else
        ret = mbedtls_chachapoly_encrypt_and_tag(&a->cp, pt_len + 1, nonce,
                                                 rec, 5, inner, inner,
                                                 inner + pt_len + 1);
    if (ret != 0) return -2;
    a->seq++;
    return (int)(5 + clen);
}

/* ------------------------------ state ------------------------------ */
struct ztls {
    int fd;
    int insecure;
    int timeout_ms;
    char sni[MAX_SNI + 1];

    uint8_t session_id[32];
    uint8_t pub_mlkem[MLKEM_PK_LEN];
    uint8_t sk_mlkem[MLKEM_SK_LEN];
    uint8_t pub_x25519[X25519_LEN];   /* hybrid 0x11ec share */
    uint8_t pub_x25519_2[X25519_LEN]; /* standalone 0x001d share */
    x25519_key x1;
    x25519_key x2;

    uint8_t hello_hs[2052];           /* ClientHello handshake message */
    size_t hello_hs_len;
    uint8_t hello_record[2056];
    size_t hello_record_len;

    uint8_t hs[MAX_HS];               /* handshake reassembly buffer */
    size_t hs_len;
    size_t hs_pos;
    uint8_t app[MAX_REC];             /* one decrypted application record */
    size_t app_len;
    size_t app_pos;
    uint8_t transcript[MAX_TRANSCRIPT];
    size_t transcript_len;
    uint8_t rec[MAX_REC];             /* record scratch, encrypt + decrypt */
    uint8_t rec_out[MAX_REC];

    aead_ctx hs_dec;
    aead_ctx hs_enc;
    aead_ctx ap_dec;
    aead_ctx ap_enc;
    const mbedtls_md_info_t *md; /* suite transcript hash */
    size_t hash_len;

    mbedtls_x509_crt chain;
    mbedtls_x509_crt ca;
    const mbedtls_x509_crt *ca_shared;
    int chain_on;
    int ca_on;

    ztls_info info;
    int hello_sent;
    int handshake_done;
    int peer_closed;
    int failed;
};

/* ------------------------ ClientHello ------------------------ */
static size_t ext_begin(uint8_t *p, uint16_t type)
{
    put16(p, type);
    return 4; /* length filled by ext_end */
}

static size_t ext_end(uint8_t *p, size_t data_len)
{
    put16(p + 2, (uint16_t)data_len);
    return 4 + data_len;
}

/* Builds the exact Bun 1.4.4 ClientHello into t->hello_hs/hello_record. */
static int build_client_hello(struct ztls *t)
{
    uint8_t *b = t->hello_hs + 4;
    uint8_t random[32];
    size_t sni_len = strlen(t->sni);
    size_t n = 0;

    if (PQCLEAN_MLKEM768_CLEAN_crypto_kem_keypair(t->pub_mlkem,
                                                  t->sk_mlkem) != 0)
        return -1;
    if (x25519_generate(&t->x1, t->pub_x25519) != 0) return -1;
    /* Bun sends a distinct X25519 key in the standalone 0x001d share; the
     * hybrid share's X25519 component is not reused. */
    if (x25519_generate(&t->x2, t->pub_x25519_2) != 0) return -1;
    if (rng_fill(random, sizeof(random)) != 0) return -1;
    if (rng_fill(t->session_id, sizeof(t->session_id)) != 0) return -1;

    b[n++] = 0x03;
    b[n++] = 0x03; /* legacy_version */
    memcpy(b + n, random, 32);
    n += 32; /* random */
    b[n++] = 32; /* session_id len */
    memcpy(b + n, t->session_id, 32);
    n += 32;

    put16(b + n, (uint16_t)sizeof(bun_ciphers));
    n += 2;
    for (size_t i = 0; i < sizeof(bun_ciphers) / sizeof(bun_ciphers[0]); i++) {
        put16(b + n, bun_ciphers[i]);
        n += 2;
    }
    b[n++] = 1;
    b[n++] = 0; /* compression */

    size_t ext_total_at = n;
    n += 2;
    size_t ext_start = n;

    /* 0x0000 server_name */
    {
        size_t e = n;
        n += ext_begin(b + n, 0x0000);
        size_t d0 = n;
        put16(b + n, (uint16_t)(1 + 2 + sni_len));
        n += 2;
        b[n++] = 0;
        put16(b + n, (uint16_t)sni_len);
        n += 2;
        memcpy(b + n, t->sni, sni_len);
        n += sni_len;
        n = e + ext_end(b + e, n - d0);
    }
    /* 0x0017 extended_master_secret */
    {
        size_t e = n;
        n += ext_begin(b + n, 0x0017);
        n = e + ext_end(b + e, 0);
    }
    /* 0xff01 renegotiation_info */
    {
        size_t e = n;
        n += ext_begin(b + n, 0xff01);
        b[n++] = 0x00;
        n = e + ext_end(b + e, 1);
    }
    /* 0x000a supported_groups */
    {
        size_t e = n;
        n += ext_begin(b + n, 0x000a);
        size_t d0 = n;
        put16(b + n, (uint16_t)sizeof(bun_groups));
        n += 2;
        for (size_t i = 0; i < sizeof(bun_groups) / sizeof(bun_groups[0]);
             i++) {
            put16(b + n, bun_groups[i]);
            n += 2;
        }
        n = e + ext_end(b + e, n - d0);
    }
    /* 0x000b ec_point_formats */
    {
        size_t e = n;
        n += ext_begin(b + n, 0x000b);
        b[n++] = 1;
        b[n++] = 0;
        n = e + ext_end(b + e, 2);
    }
    /* 0x0023 session_ticket */
    {
        size_t e = n;
        n += ext_begin(b + n, 0x0023);
        n = e + ext_end(b + e, 0);
    }
    /* 0x0010 ALPN http/1.1 */
    {
        size_t e = n;
        n += ext_begin(b + n, 0x0010);
        size_t d0 = n;
        put16(b + n, 9);
        n += 2;
        b[n++] = 8;
        memcpy(b + n, "http/1.1", 8);
        n += 8;
        n = e + ext_end(b + e, n - d0);
    }
    /* 0x0005 status_request */
    {
        size_t e = n;
        n += ext_begin(b + n, 0x0005);
        const uint8_t d[5] = { 0x01, 0x00, 0x00, 0x00, 0x00 };
        memcpy(b + n, d, 5);
        n += 5;
        n = e + ext_end(b + e, 5);
    }
    /* 0x000d signature_algorithms */
    {
        size_t e = n;
        n += ext_begin(b + n, 0x000d);
        size_t d0 = n;
        put16(b + n, (uint16_t)sizeof(bun_sigalgs));
        n += 2;
        for (size_t i = 0; i < sizeof(bun_sigalgs) / sizeof(bun_sigalgs[0]);
             i++) {
            put16(b + n, bun_sigalgs[i]);
            n += 2;
        }
        n = e + ext_end(b + e, n - d0);
    }
    /* 0x0012 signed_certificate_timestamp */
    {
        size_t e = n;
        n += ext_begin(b + n, 0x0012);
        n = e + ext_end(b + e, 0);
    }
    /* 0x0033 key_share: 0x11ec = ML-KEM-768 pk || X25519, then 0x001d */
    {
        size_t e = n;
        n += ext_begin(b + n, 0x0033);
        size_t d0 = n;
        size_t list_len_at = n;
        n += 2;
        size_t list0 = n;
        put16(b + n, 0x11ec);
        n += 2;
        put16(b + n, HYBRID_CLIENT_SHARE);
        n += 2;
        memcpy(b + n, t->pub_mlkem, MLKEM_PK_LEN);
        n += MLKEM_PK_LEN;
        memcpy(b + n, t->pub_x25519, X25519_LEN);
        n += X25519_LEN;
        put16(b + n, 0x001d);
        n += 2;
        put16(b + n, X25519_LEN);
        n += 2;
        memcpy(b + n, t->pub_x25519_2, X25519_LEN);
        n += X25519_LEN;
        put16(b + list_len_at, (uint16_t)(n - list0));
        n = e + ext_end(b + e, n - d0);
    }
    /* 0x002d psk_key_exchange_modes */
    {
        size_t e = n;
        n += ext_begin(b + n, 0x002d);
        b[n++] = 1;
        b[n++] = 1;
        n = e + ext_end(b + e, 2);
    }
    /* 0x002b supported_versions */
    {
        size_t e = n;
        n += ext_begin(b + n, 0x002b);
        b[n++] = 4;
        put16(b + n, 0x0304);
        n += 2;
        put16(b + n, 0x0303);
        n += 2;
        n = e + ext_end(b + e, 5);
    }

    put16(b + ext_total_at, (uint16_t)(n - ext_start));
    if (n > sizeof(t->hello_hs) - 4) return -1;

    t->hello_hs[0] = 1;
    put24(t->hello_hs + 1, (uint32_t)n);
    t->hello_hs_len = 4 + n;

    t->hello_record[0] = 0x16;
    t->hello_record[1] = 0x03; /* record version 0x0301, like the spike */
    t->hello_record[2] = 0x01;
    put16(t->hello_record + 3, (uint16_t)t->hello_hs_len);
    memcpy(t->hello_record + 5, t->hello_hs, t->hello_hs_len);
    t->hello_record_len = 5 + t->hello_hs_len;
    return 0;
}

/* ------------------------- transcript ------------------------- */
static int transcript_add(ztls *t, const uint8_t *msg, size_t len)
{
    if (t->transcript_len + len > sizeof(t->transcript)) return -1;
    memcpy(t->transcript + t->transcript_len, msg, len);
    t->transcript_len += len;
    return 0;
}

static int transcript_hash(const ztls *t, uint8_t out[MAX_HASH])
{
    if (t->md == NULL) return -1;
    return mbedtls_md(t->md, t->transcript, t->transcript_len, out);
}

/* ------------------------- record layer ------------------------- */
/* Reads exactly n bytes. A zero-byte read with allow_eof marks a clean TCP
 * close at a record boundary; anything shorter is a truncated record. */
static int recv_exact(ztls *t, uint8_t *buf, size_t n, int allow_eof)
{
    size_t want = n;
    while (n > 0) {
        ssize_t r = zen_recv_timeout(t->fd, buf, n, t->timeout_ms);
        if (r > 0) {
            buf += (size_t)r;
            n -= (size_t)r;
            continue;
        }
        if (r == 0) {
            if (allow_eof && n == want) {
                t->peer_closed = 1;
                zen_set_error("tls: peer closed the connection");
            } else {
                zen_set_error("tls: truncated record (%zu of %zu bytes)",
                              want - n, want);
            }
            return -1;
        }
        zen_set_error("tls: recv failed after %d ms: %s", t->timeout_ms,
                      errno ? strerror(errno) : "connection error");
        return -1;
    }
    return 0;
}

/* Handles an alert payload. Returns -1; peer_closed marks a clean
 * close_notify. */
static int handle_alert(ztls *t, const uint8_t *p, size_t len)
{
    if (len < 2) {
        zen_set_error("tls: malformed alert");
        return -1;
    }
    if (p[1] == 0) {
        t->peer_closed = 1;
        zen_set_error("tls: peer sent close_notify");
    } else {
        zen_set_error("tls: peer alert level=%u description=%u",
                      (unsigned)p[0], (unsigned)p[1]);
    }
    return -1;
}

/* Pull one TLS record.
 * Returns 1 when application plaintext was queued in t->app, 0 when the record
 * was consumed (handshake bytes appended to t->hs, or CCS), -1 on error. */
static int pull_record(ztls *t, aead_ctx *dec)
{
    uint8_t hdr[5];
    if (recv_exact(t, hdr, 5, 1) != 0) return -1;
    size_t len = get16(hdr + 3);
    uint8_t type = hdr[0];
    if (len == 0 || len > MAX_REC) {
        zen_set_error("tls: invalid record type=%u len=%zu",
                      (unsigned)type, len);
        return -1;
    }
    if (type != 0x14 && type != 0x15 && type != 0x16 && type != 0x17) {
        zen_set_error("tls: unexpected record type=0x%02x len=%zu",
                      (unsigned)type, len);
        return -1;
    }
    if (recv_exact(t, t->rec, len, 0) != 0) return -1;

    if (type == 0x14) return 0; /* change_cipher_spec */
    if (type == 0x15) return handle_alert(t, t->rec, len);
    if (type == 0x16) {
        if (t->hs_len + len > sizeof(t->hs)) {
            zen_set_error("tls: handshake buffer overflow");
            return -1;
        }
        memcpy(t->hs + t->hs_len, t->rec, len);
        t->hs_len += len;
        return 0;
    }

    if (dec == NULL) {
        zen_set_error("tls: encrypted record before the key schedule");
        return -1;
    }
    size_t out_len = 0;
    int inner = aead_open(dec, hdr, t->rec, len, t->rec_out, &out_len);
    if (inner == -2) {
        zen_set_error("tls: record authentication failed");
        return -1;
    }
    if (inner < 0) {
        zen_set_error("tls: malformed encrypted record");
        return -1;
    }
    if (inner == 0x16) {
        if (t->hs_len + out_len > sizeof(t->hs)) {
            zen_set_error("tls: handshake buffer overflow");
            return -1;
        }
        memcpy(t->hs + t->hs_len, t->rec_out, out_len);
        t->hs_len += out_len;
        return 0;
    }
    if (inner == 0x15) return handle_alert(t, t->rec_out, out_len);
    if (inner == 0x17) {
        if (out_len == 0) return 0;
        if (t->app_pos < t->app_len) {
            zen_set_error("tls: application record while %zu bytes unread",
                          t->app_len - t->app_pos);
            return -1;
        }
        memcpy(t->app, t->rec_out, out_len);
        t->app_len = out_len;
        t->app_pos = 0;
        return 1;
    }
    zen_set_error("tls: unexpected inner content type=%d", inner);
    return -1;
}

/* Next complete handshake message. Returns its type, -1 on error, or -2 when
 * application plaintext was queued instead. */
static int next_handshake(ztls *t, aead_ctx *dec, uint8_t **msg,
                          size_t *msg_len)
{
    for (;;) {
        if (t->hs_pos == t->hs_len) {
            t->hs_pos = 0;
            t->hs_len = 0;
        }
        while (t->hs_len - t->hs_pos < 4) {
            int r = pull_record(t, dec);
            if (r < 0) return -1;
            if (r == 1) return -2;
        }
        size_t len = 4 + (size_t)get24(t->hs + t->hs_pos + 1);
        if (len > sizeof(t->hs)) {
            zen_set_error("tls: handshake message too large");
            return -1;
        }
        while (t->hs_len - t->hs_pos < len) {
            int r = pull_record(t, dec);
            if (r < 0) return -1;
            if (r == 1) return -2;
        }
        *msg = t->hs + t->hs_pos;
        *msg_len = len;
        t->hs_pos += len;
        return (*msg)[0];
    }
}

/* ------------------------ ServerHello ------------------------ */
typedef struct {
    uint16_t cipher;
    uint16_t group;
    const uint8_t *share;
    size_t share_len;
    uint8_t session_id_echo[32];
    size_t session_id_echo_len;
} server_hello;

static int parse_server_hello(const uint8_t *msg, size_t len, server_hello *sh)
{
    static const uint8_t hrr_random[32] = {
        0xCF, 0x21, 0xAD, 0x74, 0xE5, 0x9A, 0x61, 0x11,
        0xBE, 0x1D, 0x8C, 0x02, 0x1E, 0x65, 0xB8, 0x91,
        0xC2, 0xA2, 0x11, 0x16, 0x7A, 0xBB, 0x8C, 0x5E,
        0x07, 0x9E, 0x09, 0xE2, 0xC8, 0xA8, 0x33, 0x9C,
    };
    const uint8_t *p = msg + 4;
    const uint8_t *end = msg + len;
    if (len < 4 + 2 + 32 + 1 || p + 2 + 32 + 1 > end) {
        zen_set_error("tls: truncated ServerHello");
        return -1;
    }
    p += 2;
    if (memcmp(p, hrr_random, 32) == 0) {
        zen_set_error("tls: server sent HelloRetryRequest");
        return -2;
    }
    p += 32;
    size_t sid_len = *p++;
    if (sid_len > 32 || p + sid_len + 2 + 1 + 2 > end) {
        zen_set_error("tls: malformed ServerHello session id");
        return -1;
    }
    memcpy(sh->session_id_echo, p, sid_len);
    sh->session_id_echo_len = sid_len;
    p += sid_len;
    sh->cipher = get16(p);
    p += 2;
    if (*p++ != 0) {
        zen_set_error("tls: ServerHello compression is not null");
        return -1;
    }
    uint16_t ext_len = get16(p);
    p += 2;
    if (p + ext_len > end) {
        zen_set_error("tls: truncated ServerHello extensions");
        return -1;
    }
    const uint8_t *ext_end = p + ext_len;
    sh->group = 0;
    sh->share = NULL;
    sh->share_len = 0;
    uint16_t selected_version = 0;
    while (p + 4 <= ext_end) {
        uint16_t et = get16(p);
        uint16_t el = get16(p + 2);
        p += 4;
        if (p + el > ext_end) {
            zen_set_error("tls: malformed ServerHello extension");
            return -1;
        }
        if (et == 0x002b && el == 2) selected_version = get16(p);
        if (et == 0x0033 && el >= 4) {
            sh->group = get16(p);
            sh->share_len = get16(p + 2);
            if ((size_t)sh->share_len + 4 > el) {
                zen_set_error("tls: malformed ServerHello key share");
                return -1;
            }
            sh->share = p + 4;
        }
        p += el;
    }
    if (selected_version != 0x0304) {
        zen_set_error("tls: server did not select TLS 1.3 (0x%04x)",
                      selected_version);
        return -1;
    }
    return 0;
}

/* --------------------- Certificate parsing --------------------- */
static int parse_certificate(ztls *t, const uint8_t *msg, size_t len)
{
    if (len < 8) {
        zen_set_error("tls: truncated Certificate message");
        return -1;
    }
    const uint8_t *p = msg + 4, *end = msg + len;
    size_t ctx_len = *p++;
    if (p + ctx_len > end) {
        zen_set_error("tls: malformed Certificate request context");
        return -1;
    }
    p += ctx_len;
    uint32_t list_len = get24(p);
    p += 3;
    if (p + list_len > end) {
        zen_set_error("tls: malformed Certificate chain length");
        return -1;
    }
    const uint8_t *list_end = p + list_len;
    int count = 0;
    while (p + 3 <= list_end) {
        uint32_t clen = get24(p);
        p += 3;
        if (p + clen > list_end) {
            zen_set_error("tls: malformed Certificate entry");
            return -1;
        }
        int ret = mbedtls_x509_crt_parse_der(&t->chain, p, clen);
        if (ret != 0) {
            zen_set_error("tls: certificate parse failed (-0x%04x)",
                          (unsigned)-ret);
            return -1;
        }
        p += clen;
        if (p + 2 > list_end) {
            zen_set_error("tls: truncated Certificate entry extensions");
            return -1;
        }
        uint16_t elen = get16(p);
        p += 2;
        if (p + elen > list_end) {
            zen_set_error("tls: malformed Certificate entry extensions");
            return -1;
        }
        p += elen;
        count++;
    }
    return count;
}

/* ------------------- EncryptedExtensions ------------------- */
/* 0 ok, -1 malformed, -2 an ALPN protocol other than http/1.1 was selected. */
static int parse_encrypted_extensions(const uint8_t *msg, size_t len,
                                      ztls_info *info)
{
    if (len < 6) return -1;
    const uint8_t *p = msg + 4, *end = msg + len;
    uint16_t ext_len = get16(p);
    p += 2;
    if (p + ext_len > end) return -1;
    end = p + ext_len;
    while (p + 4 <= end) {
        uint16_t et = get16(p);
        uint16_t el = get16(p + 2);
        p += 4;
        if (p + el > end) return -1;
        if (et == 0x0010) {
            if (el < 3) return -1;
            uint16_t list_len = get16(p);
            if ((size_t)el - 2 < list_len) return -1;
            const uint8_t *q = p + 2, *qe = q + list_len;
            int picked = 0;
            while (q < qe) {
                uint8_t plen = *q++;
                if (plen == 0 || q + plen > qe) return -1;
                if (plen == 8 && memcmp(q, "http/1.1", 8) == 0) picked = 1;
                q += plen;
            }
            if (!picked) return -2;
            info->alpn_http11 = 1;
        }
        p += el;
    }
    return 0;
}

/* ------------------- CertificateVerify ------------------- */
static int digest_for_sigalg(uint16_t sigalg, mbedtls_md_type_t *md,
                             const uint8_t *data, size_t len, uint8_t *digest,
                             size_t *digest_len)
{
    switch (sigalg) {
    case 0x0403:
    case 0x0401:
    case 0x0804:
        *md = MBEDTLS_MD_SHA256;
        *digest_len = 32;
        return sha256(data, len, digest);
    case 0x0503:
    case 0x0501:
    case 0x0805:
        *md = MBEDTLS_MD_SHA384;
        *digest_len = 48;
        return sha384(data, len, digest);
    case 0x0603:
    case 0x0601:
    case 0x0806:
        *md = MBEDTLS_MD_SHA512;
        *digest_len = 64;
        return sha512(data, len, digest);
    default:
        return -1;
    }
}

static int verify_certificate_verify(ztls *t, const uint8_t *msg, size_t len,
                                     const uint8_t transcript_hash[MAX_HASH],
                                     size_t hash_len)
{
    if (len < 8 || hash_len == 0 || hash_len > MAX_HASH) {
        zen_set_error("tls: truncated CertificateVerify");
        return -1;
    }
    const uint8_t *p = msg + 4;
    uint16_t sigalg = get16(p);
    p += 2;
    uint16_t sig_len = get16(p);
    p += 2;
    if ((size_t)(msg + len - p) < sig_len) {
        zen_set_error("tls: malformed CertificateVerify signature");
        return -1;
    }
    const uint8_t *sig = p;
    t->info.cv_sigalg = sigalg;

    static const char context[] = "TLS 1.3, server CertificateVerify";
    uint8_t tbs[64 + sizeof(context) - 1 + 1 + MAX_HASH];
    size_t n = 0;
    memset(tbs, 0x20, 64);
    n += 64;
    memcpy(tbs + n, context, sizeof(context) - 1);
    n += sizeof(context) - 1;
    tbs[n++] = 0;
    memcpy(tbs + n, transcript_hash, hash_len);
    n += hash_len;

    mbedtls_md_type_t md;
    uint8_t digest[64];
    size_t digest_len = 0;
    if (digest_for_sigalg(sigalg, &md, tbs, n, digest, &digest_len) != 0) {
        zen_set_error("tls: unsupported CertificateVerify sigalg=0x%04x",
                      sigalg);
        return -2;
    }

    int ret;
    if (sigalg == 0x0804 || sigalg == 0x0805 || sigalg == 0x0806) {
        mbedtls_pk_rsassa_pss_options opts;
        memset(&opts, 0, sizeof(opts));
        opts.mgf1_hash_id = md;
        opts.expected_salt_len = MBEDTLS_RSA_SALT_LEN_ANY;
        ret = mbedtls_pk_verify_ext(MBEDTLS_PK_RSASSA_PSS, &opts,
                                    &t->chain.pk, md, digest, digest_len,
                                    sig, sig_len);
    } else {
        ret = mbedtls_pk_verify(&t->chain.pk, md, digest, digest_len,
                                sig, sig_len);
    }
    if (ret != 0) {
        zen_set_error("tls: CertificateVerify failed sigalg=0x%04x (-0x%04x)",
                      sigalg, (unsigned)-ret);
        return -1;
    }
    return 0;
}

/* -------------------- key schedule -------------------- */
typedef struct {
    uint8_t client_hs_secret[MAX_HASH];
    uint8_t server_hs_secret[MAX_HASH];
    uint8_t client_finished_key[MAX_HASH];
    uint8_t server_finished_key[MAX_HASH];
    uint8_t master_secret[MAX_HASH];
    uint8_t client_ap_secret[MAX_HASH];
    uint8_t server_ap_secret[MAX_HASH];
} key_schedule;

static int derive_handshake_secrets(const mbedtls_md_info_t *md,
                                    const uint8_t *ikm, size_t ikm_len,
                                    const uint8_t thash[MAX_HASH],
                                    key_schedule *ks)
{
    uint8_t zeros[MAX_HASH] = {0};
    uint8_t empty_hash[MAX_HASH], early[MAX_HASH], derived[MAX_HASH];
    uint8_t hs_secret[MAX_HASH];
    size_t hlen = mbedtls_md_get_size(md);
    if (hlen == 0 || hlen > MAX_HASH) return -1;
    if (mbedtls_md(md, NULL, 0, empty_hash) != 0) return -1;
    if (hkdf_extract(md, zeros, hlen, zeros, hlen, early) != 0) return -1;
    if (expand_label(md, early, hlen, "derived", empty_hash, hlen, derived,
                     hlen) != 0)
        return -1;
    if (hkdf_extract(md, derived, hlen, ikm, ikm_len, hs_secret) != 0)
        return -1;
    if (expand_label(md, hs_secret, hlen, "c hs traffic", thash, hlen,
                     ks->client_hs_secret, hlen) != 0)
        return -1;
    if (expand_label(md, hs_secret, hlen, "s hs traffic", thash, hlen,
                     ks->server_hs_secret, hlen) != 0)
        return -1;
    if (expand_label(md, ks->client_hs_secret, hlen, "finished", NULL, 0,
                     ks->client_finished_key, hlen) != 0)
        return -1;
    if (expand_label(md, ks->server_hs_secret, hlen, "finished", NULL, 0,
                     ks->server_finished_key, hlen) != 0)
        return -1;
    if (expand_label(md, hs_secret, hlen, "derived", empty_hash, hlen,
                     derived, hlen) != 0)
        return -1;
    if (hkdf_extract(md, derived, hlen, zeros, hlen, ks->master_secret) != 0)
        return -1;
    return 0;
}

static int derive_app_secrets(const mbedtls_md_info_t *md, key_schedule *ks,
                              const uint8_t thash[MAX_HASH])
{
    size_t hlen = mbedtls_md_get_size(md);
    if (hlen == 0 || hlen > MAX_HASH) return -1;
    if (expand_label(md, ks->master_secret, hlen, "c ap traffic", thash, hlen,
                     ks->client_ap_secret, hlen) != 0)
        return -1;
    if (expand_label(md, ks->master_secret, hlen, "s ap traffic", thash, hlen,
                     ks->server_ap_secret, hlen) != 0)
        return -1;
    return 0;
}

static int traffic_keys(const mbedtls_md_info_t *md,
                        const uint8_t secret[MAX_HASH], uint16_t suite,
                        uint8_t key[32], uint8_t iv[12])
{
    size_t hlen = mbedtls_md_get_size(md);
    size_t key_len = (suite == 0x1301) ? 16 : 32;
    if (hlen == 0 || hlen > MAX_HASH) return -1;
    if (expand_label(md, secret, hlen, "key", NULL, 0, key, key_len) != 0)
        return -1;
    if (expand_label(md, secret, hlen, "iv", NULL, 0, iv, 12) != 0)
        return -1;
    return 0;
}

/* ------------------------- API: construction ------------------------- */
static void ztls_free_inner(ztls *t);

static struct ztls *ztls_alloc(int fd, const char *sni, int insecure,
                               const uint8_t *ca_pem, size_t ca_len,
                               const void *ca_chain, int timeout_ms)
{
    if (fd < 0 || sni == NULL) {
        zen_set_error("tls: invalid fd or SNI");
        return NULL;
    }
    size_t sni_len = strlen(sni);
    if (sni_len == 0 || sni_len > MAX_SNI) {
        zen_set_error("tls: SNI length %zu is not usable", sni_len);
        return NULL;
    }
    if (timeout_ms < 0) timeout_ms = 0;

    ztls *t = calloc(1, sizeof(*t));
    if (t == NULL) {
        zen_set_error("tls: out of memory");
        return NULL;
    }
    t->fd = fd;
    t->insecure = insecure != 0;
    t->timeout_ms = timeout_ms;
    t->md = md_sha256();
    t->hash_len = 32;
    memcpy(t->sni, sni, sni_len + 1);

    mbedtls_x509_crt_init(&t->chain);
    t->chain_on = 1;
    mbedtls_x509_crt_init(&t->ca);
    t->ca_on = 1;

    if (!t->insecure) {
        if (ca_chain != NULL) {
            t->ca_shared = ca_chain;
        } else {
            if (ca_pem == NULL || ca_len == 0) {
                zen_set_error("tls: CA bundle is required unless insecure");
                goto fail;
            }
            uint8_t *pem = malloc(ca_len + 1);
            if (pem == NULL) {
                zen_set_error("tls: out of memory");
                goto fail;
            }
            memcpy(pem, ca_pem, ca_len);
            pem[ca_len] = 0; /* Mbed TLS PEM parsing wants a terminated buffer */
            int rc = mbedtls_x509_crt_parse(&t->ca, pem, ca_len + 1);
            free(pem);
            if (rc != 0) {
                zen_set_error("tls: CA bundle parse failed (-0x%04x)",
                              (unsigned)-rc);
                goto fail;
            }
        }
    }

    if (x25519_init(&t->x1) != 0 || x25519_init(&t->x2) != 0) {
        zen_set_error("tls: X25519 group init failed");
        goto fail;
    }
    if (build_client_hello(t) != 0) {
        zen_set_error("tls: ClientHello build failed");
        goto fail;
    }

    t->info.verify_ret = 0;
    t->info.cert_count = 0;
    return t;

fail:
    ztls_free_inner(t);
    return NULL;
}

struct ztls *ztls_new(int fd, const char *sni, int insecure,
                      const uint8_t *ca_pem, size_t ca_len, int timeout_ms)
{
    return ztls_alloc(fd, sni, insecure, ca_pem, ca_len, NULL, timeout_ms);
}

struct ztls *ztls_new_with_ca(int fd, const char *sni, int insecure,
                              const void *ca_chain, int timeout_ms)
{
    return ztls_alloc(fd, sni, insecure, NULL, 0, ca_chain, timeout_ms);
}

const ztls_info *ztls_get_info(const ztls *t)
{
    return t ? &t->info : NULL;
}

const uint8_t *ztls_hello_msg(const ztls *t, size_t *len)
{
    if (t == NULL) {
        if (len) *len = 0;
        return NULL;
    }
    if (len) *len = t->hello_hs_len;
    return t->hello_hs;
}

const uint8_t *ztls_hello_record(const ztls *t, size_t *len)
{
    if (t == NULL) {
        if (len) *len = 0;
        return NULL;
    }
    if (len) *len = t->hello_record_len;
    return t->hello_record;
}

/* ------------------------- API: handshake ------------------------- */
int ztls_send_hello(ztls *t)
{
    if (t == NULL) return -1;
    if (t->hello_sent) return 0;
    if (t->handshake_done || t->failed) {
        zen_set_error("tls: connection is not in the hello state");
        return -1;
    }
    if (zen_send_all(t->fd, t->hello_record, t->hello_record_len,
                     t->timeout_ms) != 0) {
        zen_set_error("tls: sending ClientHello failed: %s",
                      errno ? strerror(errno) : "connection error");
        t->failed = 1;
        return -1;
    }
    t->hello_sent = 1;
    return 0;
}

int ztls_handshake(ztls *t)
{
    if (t == NULL) return -1;
    if (t->handshake_done) return 0;
    if (t->failed) {
        zen_set_error("tls: connection is in a failed state");
        return -1;
    }
    if (!t->hello_sent && ztls_send_hello(t) != 0) return -1;

    uint8_t *msg = NULL;
    size_t msg_len = 0;
    int mt = next_handshake(t, NULL, &msg, &msg_len);
    if (mt != 2) {
        zen_set_error("tls: expected ServerHello, got handshake type %d", mt);
        goto fail;
    }

    server_hello sh;
    memset(&sh, 0, sizeof(sh));
    if (parse_server_hello(msg, msg_len, &sh) != 0) goto fail;
    if (sh.session_id_echo_len != 32 ||
        memcmp(sh.session_id_echo, t->session_id, 32) != 0) {
        zen_set_error("tls: ServerHello session_id echo mismatch");
        goto fail;
    }
    if (sh.cipher != 0x1301 && sh.cipher != 0x1302 && sh.cipher != 0x1303) {
        zen_set_error("tls: server selected unsupported cipher 0x%04x",
                      sh.cipher);
        goto fail;
    }
    t->info.suite = sh.cipher;
    t->info.group = sh.group;
    t->md = suite_md(sh.cipher);
    if (t->md == NULL) {
        zen_set_error("tls: no transcript hash for cipher 0x%04x", sh.cipher);
        goto fail;
    }
    t->hash_len = mbedtls_md_get_size(t->md);
    if (t->hash_len == 0 || t->hash_len > MAX_HASH) {
        zen_set_error("tls: bad transcript hash size for cipher 0x%04x",
                      sh.cipher);
        goto fail;
    }

    if (sh.group == 0x11ec) {
        if (sh.share_len != HYBRID_SERVER_SHARE) {
            zen_set_error("tls: hybrid share length %zu, expected %u",
                          sh.share_len, HYBRID_SERVER_SHARE);
            goto fail;
        }
    } else if (sh.group == 0x001d) {
        if (sh.share_len != X25519_LEN) {
            zen_set_error("tls: X25519 share length %zu, expected 32",
                          sh.share_len);
            goto fail;
        }
    } else {
        zen_set_error("tls: server selected unsupported group 0x%04x",
                      sh.group);
        goto fail;
    }

    /* shared secret = ML-KEM ss || X25519 ss for 0x11ec; X25519 only for
     * 0x001d (the standalone share's distinct keypair). */
    uint8_t mlkem_ss[MLKEM_SS_LEN];
    uint8_t x_ss[X25519_LEN];
    uint8_t ikm[MLKEM_SS_LEN + X25519_LEN];
    size_t ikm_len;
    if (sh.group == 0x11ec) {
        if (PQCLEAN_MLKEM768_CLEAN_crypto_kem_dec(mlkem_ss, sh.share,
                                                  t->sk_mlkem) != 0) {
            zen_set_error("tls: ML-KEM decapsulation failed");
            goto fail;
        }
        if (x25519_shared(&t->x1, sh.share + MLKEM_CT_LEN, x_ss) != 0) {
            zen_set_error("tls: X25519 shared secret failed");
            goto fail;
        }
        memcpy(ikm, mlkem_ss, MLKEM_SS_LEN);
        memcpy(ikm + MLKEM_SS_LEN, x_ss, X25519_LEN);
        ikm_len = sizeof(ikm);
    } else {
        if (x25519_shared(&t->x2, sh.share, x_ss) != 0) {
            zen_set_error("tls: X25519 shared secret failed");
            goto fail;
        }
        memcpy(ikm, x_ss, X25519_LEN);
        ikm_len = X25519_LEN;
    }

    if (transcript_add(t, t->hello_hs, t->hello_hs_len) != 0 ||
        transcript_add(t, msg, msg_len) != 0) {
        zen_set_error("tls: transcript overflow");
        goto fail;
    }
    uint8_t th[MAX_HASH];
    if (transcript_hash(t, th) != 0) {
        zen_set_error("tls: transcript hash failed");
        goto fail;
    }

    key_schedule ks;
    memset(&ks, 0, sizeof(ks));
    if (derive_handshake_secrets(t->md, ikm, ikm_len, th, &ks) != 0) {
        zen_set_error("tls: handshake key schedule failed");
        goto fail;
    }
    uint8_t skey[32], siv[12], ckey[32], civ[12];
    if (traffic_keys(t->md, ks.server_hs_secret, sh.cipher, skey, siv) != 0 ||
        traffic_keys(t->md, ks.client_hs_secret, sh.cipher, ckey, civ) != 0) {
        zen_set_error("tls: traffic key derivation failed");
        goto fail;
    }
    if (aead_init(&t->hs_dec, sh.cipher, skey, siv) != 0 ||
        aead_init(&t->hs_enc, sh.cipher, ckey, civ) != 0) {
        zen_set_error("tls: AEAD init failed");
        goto fail;
    }

    for (;;) {
        mt = next_handshake(t, &t->hs_dec, &msg, &msg_len);
        if (mt == -2) {
            zen_set_error("tls: application data during the handshake");
            goto fail;
        }
        if (mt < 0) goto fail;

        if (mt == 8) { /* EncryptedExtensions */
            int rc = parse_encrypted_extensions(msg, msg_len, &t->info);
            if (rc == -2) {
                zen_set_error("tls: server selected an ALPN other than "
                              "http/1.1");
                goto fail;
            }
            if (rc != 0) {
                zen_set_error("tls: malformed EncryptedExtensions");
                goto fail;
            }
            if (transcript_add(t, msg, msg_len) != 0) goto transcript_fail;
        } else if (mt == 11) { /* Certificate */
            int count = parse_certificate(t, msg, msg_len);
            if (count <= 0) goto fail;
            t->info.cert_count = count;
            if (transcript_add(t, msg, msg_len) != 0) goto transcript_fail;

            if (!t->insecure) {
                /* Mbed TLS 3.6 declares trust_ca non-const, but verification
                 * only reads the chain; the shared copy is never mutated. */
                mbedtls_x509_crt *ca =
                    (mbedtls_x509_crt *)(t->ca_shared ? t->ca_shared : &t->ca);
                uint32_t flags = 0;
                int vr = mbedtls_x509_crt_verify(&t->chain, ca, NULL,
                                                 t->sni, &flags, NULL, NULL);
                t->info.verify_performed = 1;
                t->info.verify_ret = vr;
                t->info.verify_flags = flags;
                if (vr != 0 || flags != 0) {
                    zen_set_error("tls: certificate verification failed "
                                  "ret=-0x%04x flags=0x%08x",
                                  (unsigned)-vr, flags);
                    goto fail;
                }
            }
            int n = mbedtls_x509_dn_gets(t->info.subject,
                                         sizeof(t->info.subject),
                                         &t->chain.subject);
            if (n <= 0) t->info.subject[0] = '\0';
            n = mbedtls_x509_dn_gets(t->info.issuer, sizeof(t->info.issuer),
                                     &t->chain.issuer);
            if (n <= 0) t->info.issuer[0] = '\0';
        } else if (mt == 15) { /* CertificateVerify */
            if (transcript_hash(t, th) != 0 ||
                verify_certificate_verify(t, msg, msg_len, th,
                                          t->hash_len) != 0)
                goto fail;
            if (transcript_add(t, msg, msg_len) != 0) goto transcript_fail;
        } else if (mt == 20) { /* Finished */
            uint8_t expect[MAX_HASH];
            if (transcript_hash(t, th) != 0 ||
                hmac_md(t->md, ks.server_finished_key, t->hash_len, th,
                        t->hash_len, expect) != 0) {
                zen_set_error("tls: server Finished HMAC failed");
                goto fail;
            }
            if (msg_len != 4 + t->hash_len ||
                memcmp(msg + 4, expect, t->hash_len) != 0) {
                zen_set_error("tls: server Finished verification failed");
                goto fail;
            }
            if (transcript_add(t, msg, msg_len) != 0) goto transcript_fail;
            if (transcript_hash(t, th) != 0 ||
                derive_app_secrets(t->md, &ks, th) != 0) {
                zen_set_error("tls: application key schedule failed");
                goto fail;
            }
            if (traffic_keys(t->md, ks.server_ap_secret, sh.cipher, skey,
                             siv) != 0 ||
                traffic_keys(t->md, ks.client_ap_secret, sh.cipher, ckey,
                             civ) != 0) {
                zen_set_error("tls: application traffic key failed");
                goto fail;
            }
            if (aead_init(&t->ap_dec, sh.cipher, skey, siv) != 0 ||
                aead_init(&t->ap_enc, sh.cipher, ckey, civ) != 0) {
                zen_set_error("tls: application AEAD init failed");
                goto fail;
            }

            uint8_t cfin[4 + MAX_HASH];
            cfin[0] = 20;
            put24(cfin + 1, (uint32_t)t->hash_len);
            if (hmac_md(t->md, ks.client_finished_key, t->hash_len, th,
                        t->hash_len, cfin + 4) != 0) {
                zen_set_error("tls: client Finished HMAC failed");
                goto fail;
            }
            int rl = aead_seal(&t->hs_enc, 0x16, cfin, 4 + t->hash_len,
                               t->rec, sizeof(t->rec));
            if (rl < 0) {
                zen_set_error("tls: client Finished seal failed");
                goto fail;
            }
            if (zen_send_all(t->fd, t->rec, (size_t)rl, t->timeout_ms) != 0) {
                zen_set_error("tls: client Finished send failed: %s",
                              errno ? strerror(errno) : "connection error");
                goto fail;
            }

            /* The handshake keys are dead now; the server's first flight
             * (NewSessionTicket) is consumed lazily by ztls_read(). */
            aead_free(&t->hs_dec);
            aead_free(&t->hs_enc);
            t->handshake_done = 1;
            return 0;
        } else if (mt == 13) {
            zen_set_error("tls: server requested a client certificate "
                          "(not supported)");
            goto fail;
        } else {
            zen_set_error("tls: unexpected handshake message type %d", mt);
            goto fail;
        }
    }

transcript_fail:
    zen_set_error("tls: transcript overflow");
fail:
    t->failed = 1;
    return -1;
}

/* ------------------------- API: records ------------------------- */
ssize_t ztls_read(ztls *t, void *buf, size_t cap)
{
    if (t == NULL || (buf == NULL && cap != 0)) {
        zen_set_error("tls: invalid read arguments");
        return -1;
    }
    if (cap == 0) return 0;
    if (!t->handshake_done) {
        zen_set_error("tls: read before the handshake completed");
        return -1;
    }
    if (t->failed) {
        zen_set_error("tls: connection is in a failed state");
        return -1;
    }

    for (;;) {
        if (t->app_pos < t->app_len) {
            size_t n = t->app_len - t->app_pos;
            if (n > cap) n = cap;
            memcpy(buf, t->app + t->app_pos, n);
            t->app_pos += n;
            if (t->app_pos == t->app_len) {
                t->app_pos = 0;
                t->app_len = 0;
            }
            return (ssize_t)n;
        }
        if (t->peer_closed) return 0;

        uint8_t *msg = NULL;
        size_t msg_len = 0;
        int mt = next_handshake(t, &t->ap_dec, &msg, &msg_len);
        if (mt == -2) continue; /* application data queued */
        if (mt < 0) {
            if (t->peer_closed) return 0;
            t->failed = 1;
            return -1;
        }
        if (mt == 24) {
            zen_set_error("tls: peer sent a KeyUpdate (not supported)");
            t->failed = 1;
            return -1;
        }
        /* NewSessionTicket and other post-handshake messages: no payload. */
    }
}

ssize_t ztls_write(ztls *t, const void *buf, size_t len)
{
    if (t == NULL || (buf == NULL && len != 0)) {
        zen_set_error("tls: invalid write arguments");
        return -1;
    }
    if (!t->handshake_done) {
        zen_set_error("tls: write before the handshake completed");
        return -1;
    }
    if (t->failed) {
        zen_set_error("tls: connection is in a failed state");
        return -1;
    }
    if (len == 0) return 0;

    const uint8_t *p = buf;
    size_t left = len;
    while (left > 0) {
        size_t n = left > MAX_PLAIN ? MAX_PLAIN : left;
        int rl = aead_seal(&t->ap_enc, 0x17, p, n, t->rec, sizeof(t->rec));
        if (rl < 0) {
            zen_set_error("tls: application record seal failed");
            t->failed = 1;
            return -1;
        }
        if (zen_send_all(t->fd, t->rec, (size_t)rl, t->timeout_ms) != 0) {
            zen_set_error("tls: send failed: %s",
                          errno ? strerror(errno) : "connection error");
            t->failed = 1;
            return -1;
        }
        p += n;
        left -= n;
    }
    return (ssize_t)len;
}

/* ------------------------- API: teardown ------------------------- */
static void ztls_free_inner(ztls *t)
{
    if (t == NULL) return;
    aead_free(&t->hs_dec);
    aead_free(&t->hs_enc);
    aead_free(&t->ap_dec);
    aead_free(&t->ap_enc);
    if (t->chain_on) {
        mbedtls_x509_crt_free(&t->chain);
        t->chain_on = 0;
    }
    if (t->ca_on) {
        mbedtls_x509_crt_free(&t->ca);
        t->ca_on = 0;
    }
    x25519_free(&t->x1);
    x25519_free(&t->x2);
    memset(t->sk_mlkem, 0, sizeof(t->sk_mlkem));
}

void ztls_free(ztls *t)
{
    if (t == NULL) return;
    ztls_free_inner(t);
    free(t);
}
