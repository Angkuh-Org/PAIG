/* Spike-local randombytes for PQClean ML-KEM. Uses getrandom(2), no libc RNG. */
#include "randombytes.h"
#include <sys/random.h>
#include <errno.h>
#include <stddef.h>
#include <stdint.h>

int randombytes(uint8_t *out, size_t outlen)
{
    while (outlen > 0) {
        ssize_t n = getrandom(out, outlen, 0);
        if (n < 0) {
            if (errno == EINTR) continue;
            return -1;
        }
        out += (size_t)n;
        outlen -= (size_t)n;
    }
    return 0;
}
