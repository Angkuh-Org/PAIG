#ifndef PAIG_TLS_SHIM_H
#define PAIG_TLS_SHIM_H

#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct PaigTls PaigTls;

/* Return codes below zero are PAIG sentinels; zero or positive is a byte
 * count from the TLS layer. */
#define PAIG_TLS_TIMEOUT (-1)
#define PAIG_TLS_WANT_READ (-2)
#define PAIG_TLS_WANT_WRITE (-3)
#define PAIG_TLS_ERROR (-4)

int paig_tls_global_init(const unsigned char *ca, size_t ca_len,
                         char *err, size_t errlen);
PaigTls *paig_tls_new(int fd, const char *host,
                      char *err, size_t errlen, unsigned int *verify);
int paig_tls_read(PaigTls *tls, unsigned char *buf, size_t len);
int paig_tls_write(PaigTls *tls, const unsigned char *buf, size_t len);
int paig_tls_last_error(PaigTls *tls, char *buf, size_t len);
void paig_tls_free(PaigTls *tls);

int paig_sha256(const unsigned char *data, size_t len, unsigned char out[32]);
int paig_random(unsigned char *buf, size_t len);

#ifdef __cplusplus
}
#endif

#endif
