/*
 * In-process CA bundle declaration.
 *
 * PAIG already embeds the CA bundle in Rust (src/transport/tls.rs). The
 * gateway points the C shim at those bytes at startup, so the in-process
 * build must not embed a second 200 KiB copy. There is no standalone helper
 * build any more; zen_shim.c owns the definition.
 */
#ifndef PAIG_ZEN_INPROCESS_CA_BUNDLE_H
#define PAIG_ZEN_INPROCESS_CA_BUNDLE_H

#include <stddef.h>

extern const unsigned char *zen_ca_pem;
extern size_t zen_ca_pem_len;

#endif /* PAIG_ZEN_INPROCESS_CA_BUNDLE_H */
