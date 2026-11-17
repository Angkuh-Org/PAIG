/*
 * X25519 (RFC 7748) for the Zen TLS client.
 *
 * Field elements are five 51-bit limbs in 64-bit words. The Montgomery
 * ladder is the RFC 7748 one; all field operations are branch-free, the
 * ladder runs a fixed 255 iterations, and the conditional swap is mask
 * based. No heap and no bignum: this replaces the three Mbed TLS MPI
 * ladders per handshake (two key shares and one shared secret) that
 * dominated a free request.
 *
 * The 121665 constant below is RFC 7748's a24 = (486662 - 2) / 4.
 *
 * The five-limb layout and the inversion addition chain follow the
 * public-domain ref10 / curve25519-donna lineage; the implementation here is
 * an independent RFC 7748 transcription.
 */
#include "x25519.h"

#include <string.h>

typedef struct {
    uint64_t v[5];
} fe51;

#define MASK51 ((UINT64_C(1) << 51) - 1)

static uint64_t load64(const uint8_t *p)
{
    uint64_t x = 0;
    for (int i = 7; i >= 0; i--) {
        x = (x << 8) | p[i];
    }
    return x;
}

static void store64(uint8_t *p, uint64_t x)
{
    for (int i = 0; i < 8; i++) {
        p[i] = (uint8_t)x;
        x >>= 8;
    }
}

static void fe_0(fe51 *h)
{
    memset(h, 0, sizeof(*h));
}

static void fe_1(fe51 *h)
{
    fe_0(h);
    h->v[0] = 1;
}

static void fe_copy(fe51 *h, const fe51 *f)
{
    *h = *f;
}

static void fe_add(fe51 *h, const fe51 *f, const fe51 *g)
{
    for (int i = 0; i < 5; i++) {
        h->v[i] = f->v[i] + g->v[i];
    }
}

static void fe_sub(fe51 *h, const fe51 *f, const fe51 *g)
{
    const uint64_t two_p0 = (UINT64_C(1) << 52) - 38;
    const uint64_t two_pn = (UINT64_C(1) << 52) - 2;
    h->v[0] = f->v[0] + two_p0 - g->v[0];
    for (int i = 1; i < 5; i++) {
        h->v[i] = f->v[i] + two_pn - g->v[i];
    }
}

static void fe_carry(fe51 *h)
{
    uint64_t t0 = h->v[0], t1 = h->v[1], t2 = h->v[2];
    uint64_t t3 = h->v[3], t4 = h->v[4], c;

    c = t0 >> 51; t0 &= MASK51; t1 += c;
    c = t1 >> 51; t1 &= MASK51; t2 += c;
    c = t2 >> 51; t2 &= MASK51; t3 += c;
    c = t3 >> 51; t3 &= MASK51; t4 += c;
    c = t4 >> 51; t4 &= MASK51; t0 += 19 * c;
    c = t0 >> 51; t0 &= MASK51; t1 += c;

    h->v[0] = t0;
    h->v[1] = t1;
    h->v[2] = t2;
    h->v[3] = t3;
    h->v[4] = t4;
}

static void fe_mul(fe51 *h, const fe51 *f, const fe51 *g)
{
    const uint64_t f0 = f->v[0], f1 = f->v[1], f2 = f->v[2];
    const uint64_t f3 = f->v[3], f4 = f->v[4];
    const uint64_t g0 = g->v[0], g1 = g->v[1], g2 = g->v[2];
    const uint64_t g3 = g->v[3], g4 = g->v[4];

    unsigned __int128 t0 = (unsigned __int128)f0 * g0
        + 19 * ((unsigned __int128)f1 * g4 + (unsigned __int128)f2 * g3
                + (unsigned __int128)f3 * g2 + (unsigned __int128)f4 * g1);
    unsigned __int128 t1 = (unsigned __int128)f0 * g1 + (unsigned __int128)f1 * g0
        + 19 * ((unsigned __int128)f2 * g4 + (unsigned __int128)f3 * g3
                + (unsigned __int128)f4 * g2);
    unsigned __int128 t2 = (unsigned __int128)f0 * g2 + (unsigned __int128)f1 * g1
        + (unsigned __int128)f2 * g0
        + 19 * ((unsigned __int128)f3 * g4 + (unsigned __int128)f4 * g3);
    unsigned __int128 t3 = (unsigned __int128)f0 * g3 + (unsigned __int128)f1 * g2
        + (unsigned __int128)f2 * g1 + (unsigned __int128)f3 * g0
        + 19 * (unsigned __int128)f4 * g4;
    unsigned __int128 t4 = (unsigned __int128)f0 * g4 + (unsigned __int128)f1 * g3
        + (unsigned __int128)f2 * g2 + (unsigned __int128)f3 * g1
        + (unsigned __int128)f4 * g0;

    uint64_t c;
    c = (uint64_t)(t0 >> 51); t1 += c; t0 &= MASK51;
    c = (uint64_t)(t1 >> 51); t2 += c; t1 &= MASK51;
    c = (uint64_t)(t2 >> 51); t3 += c; t2 &= MASK51;
    c = (uint64_t)(t3 >> 51); t4 += c; t3 &= MASK51;
    c = (uint64_t)(t4 >> 51); t4 &= MASK51; t0 += 19 * c;
    c = (uint64_t)(t0 >> 51); t0 &= MASK51; t1 += c;

    h->v[0] = (uint64_t)t0;
    h->v[1] = (uint64_t)t1;
    h->v[2] = (uint64_t)t2;
    h->v[3] = (uint64_t)t3;
    h->v[4] = (uint64_t)t4;
}

static void fe_sq(fe51 *h, const fe51 *f)
{
    fe_mul(h, f, f);
}

static void fe_mul_a24(fe51 *h, const fe51 *f)
{
    uint64_t t0, t1, t2, t3, t4, c;

    unsigned __int128 x0 = (unsigned __int128)f->v[0] * 121665;
    unsigned __int128 x1 = (unsigned __int128)f->v[1] * 121665 + (uint64_t)(x0 >> 51);
    unsigned __int128 x2 = (unsigned __int128)f->v[2] * 121665 + (uint64_t)(x1 >> 51);
    unsigned __int128 x3 = (unsigned __int128)f->v[3] * 121665 + (uint64_t)(x2 >> 51);
    unsigned __int128 x4 = (unsigned __int128)f->v[4] * 121665 + (uint64_t)(x3 >> 51);

    t0 = (uint64_t)x0 & MASK51;
    t1 = (uint64_t)x1 & MASK51;
    t2 = (uint64_t)x2 & MASK51;
    t3 = (uint64_t)x3 & MASK51;
    t4 = (uint64_t)x4 & MASK51;
    t0 += 19 * (uint64_t)(x4 >> 51);
    c = t0 >> 51; t0 &= MASK51; t1 += c;

    h->v[0] = t0;
    h->v[1] = t1;
    h->v[2] = t2;
    h->v[3] = t3;
    h->v[4] = t4;
}

static void fe_sq_n(fe51 *out, const fe51 *in, int n)
{
    fe_sq(out, in);
    for (int i = 1; i < n; i++) {
        fe_sq(out, out);
    }
}

/* z^(p-2), the ref10 addition chain for p = 2^255 - 19. */
static void fe_invert(fe51 *out, const fe51 *z)
{
    fe51 t0, t1, t2, t3;

    fe_sq(&t0, z);                                  /* 2 */
    fe_sq(&t1, &t0); fe_sq(&t1, &t1);               /* 8 */
    fe_mul(&t1, z, &t1);                            /* 9 */
    fe_mul(&t0, &t0, &t1);                          /* 11 */
    fe_sq(&t2, &t0);                                /* 22 */
    fe_mul(&t1, &t1, &t2);                          /* 2^5 - 1 */
    fe_sq_n(&t2, &t1, 5);                           /* 2^10 - 2^5 */
    fe_mul(&t1, &t2, &t1);                          /* 2^10 - 1 */
    fe_sq_n(&t2, &t1, 10);                          /* 2^20 - 2^10 */
    fe_mul(&t2, &t2, &t1);                          /* 2^20 - 1 */
    fe_sq_n(&t3, &t2, 20);                          /* 2^40 - 2^20 */
    fe_mul(&t2, &t3, &t2);                          /* 2^40 - 1 */
    fe_sq_n(&t2, &t2, 10);                          /* 2^50 - 2^10 */
    fe_mul(&t1, &t2, &t1);                          /* 2^50 - 1 */
    fe_sq_n(&t2, &t1, 50);                          /* 2^100 - 2^50 */
    fe_mul(&t2, &t2, &t1);                          /* 2^100 - 1 */
    fe_sq_n(&t3, &t2, 100);                         /* 2^200 - 2^100 */
    fe_mul(&t2, &t3, &t2);                          /* 2^200 - 1 */
    fe_sq_n(&t2, &t2, 50);                          /* 2^250 - 2^50 */
    fe_mul(&t1, &t2, &t1);                          /* 2^250 - 1 */
    fe_sq_n(&t1, &t1, 5);                           /* 2^255 - 2^5 */
    fe_mul(out, &t1, &t0);                          /* 2^255 - 21 */
}

static void fe_frombytes(fe51 *h, const uint8_t s[32])
{
    h->v[0] = load64(s) & MASK51;
    h->v[1] = (load64(s + 6) >> 3) & MASK51;
    h->v[2] = (load64(s + 12) >> 6) & MASK51;
    h->v[3] = (load64(s + 19) >> 1) & MASK51;
    h->v[4] = (load64(s + 24) >> 12) & MASK51;
}

static void fe_tobytes(uint8_t s[32], const fe51 *h)
{
    fe51 n = *h;
    fe_carry(&n);
    fe_carry(&n);
    uint64_t t0 = n.v[0], t1 = n.v[1], t2 = n.v[2], t3 = n.v[3], t4 = n.v[4];

    uint64_t q = (t0 + 19) >> 51;
    q = (t1 + q) >> 51;
    q = (t2 + q) >> 51;
    q = (t3 + q) >> 51;
    q = (t4 + q) >> 51;
    t0 += 19 * q;

    uint64_t c = t0 >> 51; t0 &= MASK51; t1 += c;
    c = t1 >> 51; t1 &= MASK51; t2 += c;
    c = t2 >> 51; t2 &= MASK51; t3 += c;
    c = t3 >> 51; t3 &= MASK51; t4 += c;
    t4 &= MASK51;

    store64(s, t0 | (t1 << 51));
    store64(s + 8, (t1 >> 13) | (t2 << 38));
    store64(s + 16, (t2 >> 26) | (t3 << 25));
    store64(s + 24, (t3 >> 39) | (t4 << 12));
}

static void fe_cswap(fe51 *a, fe51 *b, unsigned swap)
{
    uint64_t mask = (uint64_t)0 - (uint64_t)swap;
    for (int i = 0; i < 5; i++) {
        uint64_t x = (a->v[i] ^ b->v[i]) & mask;
        a->v[i] ^= x;
        b->v[i] ^= x;
    }
}

/* TODO(perf): at -O2 this ladder has a 944-byte frame and 134 stack refs in
 * 637 instructions. The next pass is a hand-written schedule (or asm) that
 * keeps the live field elements in registers; the algorithm-level win is
 * already in place. */
static void x25519_ladder(uint8_t out[32], const uint8_t scalar[32],
                          const uint8_t point[32])
{
    uint8_t e[32];
    memcpy(e, scalar, sizeof(e));
    e[0] &= 248;
    e[31] &= 127;
    e[31] |= 64;

    fe51 x1, x2, z2, x3, z3;
    fe_frombytes(&x1, point);
    fe_1(&x2);
    fe_0(&z2);
    fe_copy(&x3, &x1);
    fe_1(&z3);

    unsigned swap = 0;
    for (int pos = 254; pos >= 0; pos--) {
        unsigned bit = (e[pos >> 3] >> (pos & 7)) & 1;
        swap ^= bit;
        fe_cswap(&x2, &x3, swap);
        fe_cswap(&z2, &z3, swap);
        swap = bit;

        fe51 a, aa, b, bb, e1, c, d, da, cb, t;
        fe_add(&a, &x2, &z2);
        fe_sq(&aa, &a);
        fe_sub(&b, &x2, &z2);
        fe_sq(&bb, &b);
        fe_sub(&e1, &aa, &bb);
        fe_add(&c, &x3, &z3);
        fe_sub(&d, &x3, &z3);
        fe_mul(&da, &d, &a);
        fe_mul(&cb, &c, &b);
        fe_add(&t, &da, &cb);
        fe_sq(&x3, &t);
        fe_sub(&t, &da, &cb);
        fe_sq(&t, &t);
        fe_mul(&z3, &x1, &t);
        fe_mul(&x2, &aa, &bb);
        fe_mul_a24(&t, &e1);
        fe_add(&t, &aa, &t);
        fe_mul(&z2, &e1, &t);
    }
    fe_cswap(&x2, &x3, swap);
    fe_cswap(&z2, &z3, swap);

    fe51 inv;
    fe_invert(&inv, &z2);
    fe_mul(&x2, &x2, &inv);
    fe_tobytes(out, &x2);

    volatile uint8_t *p = e;
    for (size_t i = 0; i < sizeof(e); i++) {
        p[i] = 0;
    }
}

void zen_x25519_scalarmult(uint8_t out[32], const uint8_t scalar[32],
                           const uint8_t point[32])
{
    x25519_ladder(out, scalar, point);
}

void zen_x25519_base(uint8_t out[32], const uint8_t scalar[32])
{
    static const uint8_t basepoint[32] = { 9 };
    x25519_ladder(out, scalar, basepoint);
}

int zen_x25519_is_zero(const uint8_t p[32])
{
    uint8_t acc = 0;
    for (int i = 0; i < 32; i++) {
        acc |= p[i];
    }
    return acc == 0;
}
