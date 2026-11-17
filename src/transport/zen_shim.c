/*
 * PAIG's in-process entry point for the OpenCode free transport.
 *
 * The gateway owns the CA bundle in Rust; this file exposes it to the C shim
 * and gives Rust a stable symbol that runs one request over a socketpair.
 * No process, no extra binary, and no second copy of the 200 KiB PEM bundle.
 */
#include "zen.h"
#include "zen_shim.h"

#ifdef PAIG_ZEN_EXTERNAL_CA
#include "ca_bundle.h"

#include <mbedtls/x509_crt.h>
#include <pthread.h>
#include <stdlib.h>
#include <string.h>

const unsigned char *zen_ca_pem;
size_t zen_ca_pem_len;
const void *zen_ca_chain;

static mbedtls_x509_crt g_ca;
static pthread_once_t g_ca_once = PTHREAD_ONCE_INIT;

static void parse_ca_once(void)
{
    if (zen_ca_pem == NULL || zen_ca_pem_len == 0) {
        return;
    }
    unsigned char *pem = malloc(zen_ca_pem_len + 1);
    if (pem == NULL) {
        return;
    }
    memcpy(pem, zen_ca_pem, zen_ca_pem_len);
    pem[zen_ca_pem_len] = 0;
    mbedtls_x509_crt_init(&g_ca);
    if (mbedtls_x509_crt_parse(&g_ca, pem, zen_ca_pem_len + 1) == 0) {
        zen_ca_chain = &g_ca;
    } else {
        mbedtls_x509_crt_free(&g_ca);
    }
    free(pem);
}

/* Called once from Rust before the first free request. The pointer stays
 * valid for the process lifetime (a Rust static). The 118-certificate PEM is
 * parsed once; every request reuses the chain. */
void paig_zen_set_ca(const unsigned char *ca, size_t len)
{
    zen_ca_pem = ca;
    zen_ca_pem_len = len;
    pthread_once(&g_ca_once, parse_ca_once);
}
#endif

int paig_zen_shim_run(int in_fd, int out_fd, const zen_shim_opts *opts)
{
    return zen_shim_run(in_fd, out_fd, opts);
}
