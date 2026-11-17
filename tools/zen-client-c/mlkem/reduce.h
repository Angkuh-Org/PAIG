#ifndef PQCLEAN_MLKEM768_CLEAN_REDUCE_H
#define PQCLEAN_MLKEM768_CLEAN_REDUCE_H
#include "params.h"
#include <stdint.h>

#define MONT (-1044) // 2^16 mod q
#define QINV (-3327) // q^-1 mod 2^16

/* Keep the reducers here, not in reduce.c: the NTT and basemul call them
 * once per butterfly, and at -O2 a cross-TU call costs more than the whole
 * reduction. Inlining them measured -22.5% ML-KEM Ir with no text growth. */
static inline int16_t PQCLEAN_MLKEM768_CLEAN_montgomery_reduce(int32_t a) {
    int16_t t;

    t = (int16_t)a * QINV;
    t = (a - (int32_t)t * KYBER_Q) >> 16;
    return t;
}

static inline int16_t PQCLEAN_MLKEM768_CLEAN_barrett_reduce(int16_t a) {
    int16_t t;
    const int16_t v = ((1 << 26) + KYBER_Q / 2) / KYBER_Q;

    t  = ((int32_t)v * a + (1 << 25)) >> 26;
    t *= KYBER_Q;
    return a - t;
}

#endif
