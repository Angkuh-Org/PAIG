#ifndef ZEN_X25519_H
#define ZEN_X25519_H

#include <stdint.h>

/* RFC 7748 X25519. The scalar is clamped internally; point is the
 * little-endian u-coordinate. */
void zen_x25519_scalarmult(uint8_t out[32], const uint8_t scalar[32],
                           const uint8_t point[32]);
void zen_x25519_base(uint8_t out[32], const uint8_t scalar[32]);
int zen_x25519_is_zero(const uint8_t p[32]);

#endif /* ZEN_X25519_H */
