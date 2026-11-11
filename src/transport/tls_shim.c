/* PAIG's only C surface: a blocking TLS client over an already-connected fd.
 *
 * The Rust side owns DNS, connect timeouts, and the TcpStream. This shim owns
 * the Mbed TLS contexts and translates socket EAGAIN into a TLS timeout, which
 * is what the blocking fd + SO_RCVTIMEO/SO_SNDTIMEO setup produces.
 */
#include "tls_shim.h"

#include <errno.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <unistd.h>

#include "mbedtls/ctr_drbg.h"
#include "mbedtls/entropy.h"
#include "mbedtls/error.h"
#include "mbedtls/net_sockets.h"
#include "mbedtls/sha256.h"
#include "mbedtls/ssl.h"
#include "mbedtls/x509_crt.h"
#include "psa/crypto.h"

#ifndef MSG_NOSIGNAL
#define MSG_NOSIGNAL 0
#endif

struct PaigTls {
    mbedtls_ssl_context ssl;
    int fd;
    char last_error[512];
};

static mbedtls_ssl_config g_conf;
static mbedtls_x509_crt g_ca;
static mbedtls_entropy_context g_entropy;
static mbedtls_ctr_drbg_context g_drbg;
static int g_ready;

static void set_err(char *err, size_t errlen, const char *fmt, ...)
{
    va_list ap;

    if (err == NULL || errlen == 0) {
        return;
    }
    va_start(ap, fmt);
    vsnprintf(err, errlen, fmt, ap);
    va_end(ap);
}

static void append_info(char *buf, size_t len, const char *text)
{
    size_t used = strlen(buf);

    if (used >= len - 1) {
        return;
    }
    if (used > 0) {
        snprintf(buf + used, len - used, "; ");
        used = strlen(buf);
    }
    snprintf(buf + used, len - used, "%s", text);
}

static void format_verify(unsigned int verify, char *buf, size_t len)
{
    buf[0] = '\0';
    if (verify & MBEDTLS_X509_BADCERT_EXPIRED) {
        append_info(buf, len, "certificate has expired");
    }
    if (verify & MBEDTLS_X509_BADCERT_FUTURE) {
        append_info(buf, len, "certificate is not yet valid");
    }
    if (verify & MBEDTLS_X509_BADCERT_CN_MISMATCH) {
        append_info(buf, len, "certificate name mismatch");
    }
    if (verify & MBEDTLS_X509_BADCERT_NOT_TRUSTED) {
        append_info(buf, len, "self-signed or untrusted certificate");
    }
    if (verify & MBEDTLS_X509_BADCERT_REVOKED) {
        append_info(buf, len, "certificate revoked");
    }
    if (verify & MBEDTLS_X509_BADCERT_BAD_MD) {
        append_info(buf, len, "certificate uses a bad digest");
    }
    if (verify & MBEDTLS_X509_BADCERT_BAD_PK) {
        append_info(buf, len, "certificate uses a bad public key");
    }
    if (verify & MBEDTLS_X509_BADCERT_BAD_KEY) {
        append_info(buf, len, "certificate key is unacceptable");
    }
    if (verify & MBEDTLS_X509_BADCERT_KEY_USAGE) {
        append_info(buf, len, "certificate usage mismatch");
    }
    if (verify & MBEDTLS_X509_BADCERT_EXT_KEY_USAGE) {
        append_info(buf, len, "certificate extended usage mismatch");
    }
    if (buf[0] == '\0') {
        mbedtls_x509_crt_verify_info(buf, len, "", verify);
    }
}

static void record_error(PaigTls *tls, const char *prefix, int ret)
{
    char base[192] = {0};
    unsigned int verify;
    char info[256] = {0};

    mbedtls_strerror(ret, base, sizeof(base));
    verify = mbedtls_ssl_get_verify_result(&tls->ssl);
    if (verify != 0 && verify != 0xFFFFFFFFU) {
        format_verify(verify, info, sizeof(info));
        size_t n = strlen(info);
        while (n > 0 && (info[n - 1] == '\n' || info[n - 1] == '\r')) {
            info[--n] = '\0';
        }
        if (prefix != NULL && prefix[0] != '\0') {
            snprintf(tls->last_error, sizeof(tls->last_error),
                     "%s: %s (verify=0x%x: %s)", prefix, base, verify, info);
        } else {
            snprintf(tls->last_error, sizeof(tls->last_error),
                     "%s (verify=0x%x: %s)", base, verify, info);
        }
    } else if (prefix != NULL && prefix[0] != '\0') {
        snprintf(tls->last_error, sizeof(tls->last_error), "%s: %s", prefix, base);
    } else {
        snprintf(tls->last_error, sizeof(tls->last_error), "%s", base);
    }
}

static int bio_send(void *ctx, const unsigned char *buf, size_t len)
{
    PaigTls *tls = (PaigTls *) ctx;
    ssize_t n;

    if (len == 0) {
        return 0;
    }
    n = send(tls->fd, buf, len, MSG_NOSIGNAL);
    if (n >= 0) {
        return (int) n;
    }
    if (errno == EINTR) {
        return MBEDTLS_ERR_SSL_WANT_WRITE;
    }
    if (errno == EAGAIN || errno == EWOULDBLOCK) {
        return MBEDTLS_ERR_SSL_TIMEOUT;
    }
    return MBEDTLS_ERR_NET_SEND_FAILED;
}

static int bio_recv(void *ctx, unsigned char *buf, size_t len)
{
    PaigTls *tls = (PaigTls *) ctx;
    ssize_t n;

    if (len == 0) {
        return 0;
    }
    n = recv(tls->fd, buf, len, 0);
    if (n >= 0) {
        return (int) n;
    }
    if (errno == EINTR) {
        return MBEDTLS_ERR_SSL_WANT_READ;
    }
    if (errno == EAGAIN || errno == EWOULDBLOCK) {
        return MBEDTLS_ERR_SSL_TIMEOUT;
    }
    return MBEDTLS_ERR_NET_RECV_FAILED;
}

int paig_tls_global_init(const unsigned char *ca, size_t ca_len,
                         char *err, size_t errlen)
{
    const char *pers = "paig";
    psa_status_t status;
    int ret;

    if (g_ready) {
        return 0;
    }

    status = psa_crypto_init();
    if (status != PSA_SUCCESS) {
        set_err(err, errlen, "psa_crypto_init failed: %ld", (long) status);
        return -1;
    }

    mbedtls_ssl_config_init(&g_conf);
    mbedtls_x509_crt_init(&g_ca);
    mbedtls_entropy_init(&g_entropy);
    mbedtls_ctr_drbg_init(&g_drbg);

    ret = mbedtls_ctr_drbg_seed(&g_drbg, mbedtls_entropy_func, &g_entropy,
                                (const unsigned char *) pers, strlen(pers));
    if (ret != 0) {
        char base[192] = {0};
        mbedtls_strerror(ret, base, sizeof(base));
        set_err(err, errlen, "cannot seed DRBG: %s", base);
        return -1;
    }

    /* mbedtls_x509_crt_parse only recognizes PEM when the buffer ends in a
     * NUL byte; include_bytes! does not add one. */
    unsigned char *pem = (unsigned char *) malloc(ca_len + 1);
    if (pem == NULL) {
        set_err(err, errlen, "cannot allocate CA bundle buffer");
        return -1;
    }
    memcpy(pem, ca, ca_len);
    pem[ca_len] = '\0';
    ret = mbedtls_x509_crt_parse(&g_ca, pem, ca_len + 1);
    free(pem);
    if (ret != 0) {
        if (ret > 0) {
            set_err(err, errlen, "embedded CA bundle: %d certificate(s) failed to parse", ret);
        } else {
            char base[192] = {0};
            mbedtls_strerror(ret, base, sizeof(base));
            set_err(err, errlen, "embedded CA bundle: %s", base);
        }
        return -1;
    }

    ret = mbedtls_ssl_config_defaults(&g_conf, MBEDTLS_SSL_IS_CLIENT,
                                      MBEDTLS_SSL_TRANSPORT_STREAM,
                                      MBEDTLS_SSL_PRESET_DEFAULT);
    if (ret != 0) {
        char base[192] = {0};
        mbedtls_strerror(ret, base, sizeof(base));
        set_err(err, errlen, "SSL config defaults: %s", base);
        return -1;
    }

    mbedtls_ssl_conf_authmode(&g_conf, MBEDTLS_SSL_VERIFY_REQUIRED);
    mbedtls_ssl_conf_ca_chain(&g_conf, &g_ca, NULL);
    mbedtls_ssl_conf_rng(&g_conf, mbedtls_ctr_drbg_random, &g_drbg);
    mbedtls_ssl_conf_min_tls_version(&g_conf, MBEDTLS_SSL_VERSION_TLS1_2);

    g_ready = 1;
    return 0;
}

PaigTls *paig_tls_new(int fd, const char *host,
                      char *err, size_t errlen, unsigned int *verify)
{
    PaigTls *tls;
    int ret;

    if (verify != NULL) {
        *verify = 0;
    }
    if (!g_ready) {
        set_err(err, errlen, "TLS global state is not initialized");
        return NULL;
    }

    tls = (PaigTls *) calloc(1, sizeof(*tls));
    if (tls == NULL) {
        set_err(err, errlen, "cannot allocate TLS context");
        return NULL;
    }
    tls->fd = fd;
    mbedtls_ssl_init(&tls->ssl);

    ret = mbedtls_ssl_setup(&tls->ssl, &g_conf);
    if (ret == 0) {
        ret = mbedtls_ssl_set_hostname(&tls->ssl, host);
    }
    if (ret == 0) {
        mbedtls_ssl_set_bio(&tls->ssl, tls, bio_send, bio_recv, NULL);
        do {
            ret = mbedtls_ssl_handshake(&tls->ssl);
        } while (ret == MBEDTLS_ERR_SSL_WANT_READ ||
                 ret == MBEDTLS_ERR_SSL_WANT_WRITE);
    }

    if (ret != 0) {
        record_error(tls, "", ret);
        if (verify != NULL) {
            *verify = mbedtls_ssl_get_verify_result(&tls->ssl);
        }
        set_err(err, errlen, "%s", tls->last_error);
        mbedtls_ssl_free(&tls->ssl);
        free(tls);
        return NULL;
    }
    return tls;
}

int paig_tls_read(PaigTls *tls, unsigned char *buf, size_t len)
{
    int ret = mbedtls_ssl_read(&tls->ssl, buf, len);

    if (ret > 0) {
        return ret;
    }
    if (ret == 0 || ret == MBEDTLS_ERR_SSL_PEER_CLOSE_NOTIFY ||
        ret == MBEDTLS_ERR_SSL_CONN_EOF) {
        return 0;
    }
    if (ret == MBEDTLS_ERR_SSL_WANT_READ) {
        return PAIG_TLS_WANT_READ;
    }
    if (ret == MBEDTLS_ERR_SSL_WANT_WRITE) {
        return PAIG_TLS_WANT_WRITE;
    }
    if (ret == MBEDTLS_ERR_SSL_TIMEOUT) {
        return PAIG_TLS_TIMEOUT;
    }
    record_error(tls, "TLS read failed", ret);
    return PAIG_TLS_ERROR;
}

int paig_tls_write(PaigTls *tls, const unsigned char *buf, size_t len)
{
    int ret = mbedtls_ssl_write(&tls->ssl, buf, len);

    if (ret > 0) {
        return ret;
    }
    if (ret == MBEDTLS_ERR_SSL_WANT_READ) {
        return PAIG_TLS_WANT_READ;
    }
    if (ret == MBEDTLS_ERR_SSL_WANT_WRITE) {
        return PAIG_TLS_WANT_WRITE;
    }
    if (ret == MBEDTLS_ERR_SSL_TIMEOUT) {
        return PAIG_TLS_TIMEOUT;
    }
    record_error(tls, "TLS write failed", ret);
    return PAIG_TLS_ERROR;
}

int paig_tls_last_error(PaigTls *tls, char *buf, size_t len)
{
    size_t n;

    if (buf == NULL || len == 0) {
        return 0;
    }
    n = strlen(tls->last_error);
    if (n >= len) {
        n = len - 1;
    }
    memcpy(buf, tls->last_error, n);
    buf[n] = '\0';
    return (int) n;
}

void paig_tls_free(PaigTls *tls)
{
    struct timeval timeout = {0, 100000};

    if (tls == NULL) {
        return;
    }
    /* close_notify is best-effort: do not let a dead peer hold a worker for
     * the full upstream timeout during drop. */
    setsockopt(tls->fd, SOL_SOCKET, SO_SNDTIMEO, &timeout, sizeof(timeout));
    mbedtls_ssl_close_notify(&tls->ssl);
    mbedtls_ssl_free(&tls->ssl);
    free(tls);
}

int paig_sha256(const unsigned char *data, size_t len, unsigned char out[32])
{
    return mbedtls_sha256(data, len, out, 0) == 0 ? 0 : -1;
}

int paig_random(unsigned char *buf, size_t len)
{
    size_t offset = 0;

    while (offset < len) {
        long n = syscall(SYS_getrandom, buf + offset, len - offset, 0);
        if (n < 0) {
            if (errno == EINTR) {
                continue;
            }
            return -1;
        }
        if (n == 0) {
            return -1;
        }
        offset += (size_t) n;
    }
    return 0;
}
