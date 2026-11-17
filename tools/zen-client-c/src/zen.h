/*
 * zen-client-c: shared declarations for PAIG's native OpenCode free transport.
 *
 * The helper speaks PAIG's stdio shim protocol:
 *
 *   stdin : one JSON metadata line, then the raw request body
 *   stdout: one JSON status line, then the decoded response body
 *   stderr: human-readable errors (never credentials)
 *
 * Everything is single-threaded and one request per process, so the error
 * buffer is a global. That keeps every fallible function free of an errno
 * out-parameter without hiding state: zen_error() is only read after a
 * non-zero return.
 */
#ifndef ZEN_H
#define ZEN_H

#include <stddef.h>
#include <stdint.h>
#include <sys/types.h>

#define ZEN_MAX_METADATA (1u << 20)        /* one JSON line, strict cap      */
#define ZEN_MAX_BODY (64u << 20)           /* raw request body cap           */
#define ZEN_MAX_URL 8192                   /* same ceiling as the Go helper  */
#define ZEN_MAX_HEADERS 200
#define ZEN_MAX_HEADER_NAME 256
#define ZEN_MAX_HEADER_VALUE (64u << 10)
#define ZEN_MAX_HEADER_BYTES (256u << 10)
#define ZEN_MAX_ERR 4096
#define ZEN_MAX_ERROR_REPORT 4096
#define ZEN_DEFAULT_TIMEOUT_MS 180000
#define ZEN_MAX_TIMEOUT_MS (24 * 60 * 60 * 1000)

/* ---------------------------- byte buffer ---------------------------- */
typedef struct {
    uint8_t *p;
    size_t len;
    size_t cap;
} zbuf;

int zbuf_add(zbuf *b, const void *data, size_t len);
int zbuf_addstr(zbuf *b, const char *s);
void zbuf_free(zbuf *b);

/* ------------------------------ errors ------------------------------- */
void zen_set_error(const char *fmt, ...) __attribute__((format(printf, 1, 2)));
const char *zen_error(void);
void zen_clear_error(void);

/* ------------------------------ timing ------------------------------- */
int64_t zen_now_ms(void);
/* 0 when the fd is ready, -1 with errno=ETIMEDOUT on timeout. timeout_ms<=0
 * waits forever. EINTR restarts with the remaining budget, which matters on
 * the 4.9 LanceX kernel this project runs on. */
int zen_wait_fd(int fd, int for_write, int timeout_ms);
/* Cancellation fd polled alongside every zen_wait_fd(); -1 disables it.
 * The in-process gateway passes the read end of a socketpair and shuts down
 * the write end when the downstream client goes away. */
void zen_set_cancel_fd(int fd);
ssize_t zen_recv_timeout(int fd, void *buf, size_t len, int timeout_ms);
int zen_send_all(int fd, const void *buf, size_t len, int timeout_ms);
int zen_write_all(int fd, const void *buf, size_t len);

/* ------------------------------- JSON -------------------------------- */
typedef struct {
    char *name;
    char *value;
} zheader;

typedef struct {
    char *url;
    zheader headers[ZEN_MAX_HEADERS];
    int nheaders;
    int64_t timeout_ms;
} zmeta;

/* Parses the metadata object. Unknown keys are skipped (like json.Unmarshal
 * into a struct); malformed input is rejected. */
int zen_parse_metadata(const uint8_t *line, size_t len, zmeta *m);
void zen_meta_free(zmeta *m);
int zen_meta_timeout_ms(const zmeta *m);

/* ------------------------------- URL --------------------------------- */
typedef struct {
    int tls;                 /* https */
    char scheme[8];
    char host[256];          /* no brackets, even for IPv6 literals */
    int is_ipv6;
    char port[8];            /* "" means the scheme default */
    char target[ZEN_MAX_URL]; /* origin-form path[?query], starts with '/' */
} zurl;

int zen_parse_url(const char *raw, zurl *u);
/* host[:port], default port omitted and IPv6 literals bracketed */
int zen_host_header(const zurl *u, char *out, size_t cap);
/* host:port (never omitted) */
int zen_dial_address(const zurl *u, char *out, size_t cap);
int zen_is_loopback_host(const char *host);

/* ------------------------------ headers ------------------------------ */
int zen_valid_header_name(const char *name);
int zen_valid_header_value(const char *value);
/* 0 when the metadata header set is acceptable */
int zen_validate_headers(const zmeta *m);

/* Builds "METHOD target HTTP/1.1" + the proven Bun header order + extras.
 * The body is not appended; the caller writes it separately so a 64 MiB body
 * is never copied a second time. host_override is -sni / ZEN_CLIENT_SNI. */
int zen_build_request(const zurl *u, const zmeta *m, const char *host_override,
                      size_t body_len, zbuf *out);

/* ------------------------------ reader ------------------------------- */
struct ztls;
typedef struct {
    int fd;
    struct ztls *tls;       /* NULL for plain http */
    int timeout_ms;
    uint8_t buf[16384];
    size_t pos;
    size_t len;
    int eof;
} zreader;

void zen_reader_init(zreader *r, int fd, struct ztls *tls, int timeout_ms);
/* >0 bytes, 0 EOF, -1 error (zen_error set) */
ssize_t zen_reader_read(zreader *r, void *buf, size_t len);

/* ----------------------------- response ------------------------------ */
typedef struct {
    int status;
    zheader h[ZEN_MAX_HEADERS];
    int nh;
    int chunked;
    int64_t content_length;  /* -1 = EOF-delimited */
    int no_body;
} zresp;

int zen_read_head(zreader *r, zresp *resp);
void zen_resp_free(zresp *resp);
/* Streams the decoded body to out_fd, writing as each piece arrives. */
int zen_stream_body(zreader *r, const zresp *resp, int out_fd);

/* JSON status line + body, or a status-0 error line before the status. */
int zen_write_status_line(int out_fd, const zresp *resp);
int zen_write_error_line(int out_fd, const char *message);

/* ------------------------------- TLS --------------------------------- */
typedef struct {
    uint16_t suite;
    uint16_t group;
    int cert_count;
    int verify_performed;
    int verify_ret;
    uint32_t verify_flags;
    char subject[512];
    char issuer[512];
    uint16_t cv_sigalg;
    int alpn_http11;
} ztls_info;

struct ztls;

/* Builds the exact Bun 1.4.4 ClientHello for sni and parses the CA bundle
 * (skipped when insecure). ca_pem may be NULL when insecure. */
struct ztls *ztls_new(int fd, const char *sni, int insecure,
                      const uint8_t *ca_pem, size_t ca_len, int timeout_ms);

/* Same, but reuses a pre-parsed mbedtls_x509_crt chain owned by the caller.
 * The gateway parses its embedded CA once and passes it to every request. */
struct ztls *ztls_new_with_ca(int fd, const char *sni, int insecure,
                              const void *ca_chain, int timeout_ms);
int ztls_send_hello(struct ztls *t);
int ztls_handshake(struct ztls *t);
const ztls_info *ztls_get_info(const struct ztls *t);
const uint8_t *ztls_hello_msg(const struct ztls *t, size_t *len);
/* The full initial ClientHello record, including its 0x0301 record header.
 * Used by the test-only parity harness; PAIG itself never calls it. */
const uint8_t *ztls_hello_record(const struct ztls *t, size_t *len);
ssize_t ztls_read(struct ztls *t, void *buf, size_t cap);
ssize_t ztls_write(struct ztls *t, const void *buf, size_t len);
void ztls_free(struct ztls *t);

/* ------------------------------- util -------------------------------- */
int zen_parse_hostport(const char *s, char *host, size_t host_cap, char *port,
                       size_t port_cap);
int zen_resolve_cc(const char *name); /* diagnostic helper, unused in shim */

#endif /* ZEN_H */
