/*
 * zen-client-c transport body: the in-process shim used by PAIG.
 *
 * PAIG compiles this file together with tls.c/http.c/json.c/url.c/util.c and
 * mlkem/ into build/zen/libzen.a. The standalone CLI/spec/live modes are gone;
 * the only entry point is zen_shim_run() over a socketpair.
 */
#include "zen.h"
#include "zen_shim.h"
#include "ca_bundle.h"

#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <netdb.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

#ifndef MSG_NOSIGNAL
#define MSG_NOSIGNAL 0
#endif

/* ------------------------------- errors ------------------------------ */

/* One JSON status-0 line on out_fd (the Rust side reads the error there) and
 * a human line on stderr. CR/LF are flattened so the message cannot forge a
 * protocol line; the body is never started before the status line. */
static int report(int out_fd, const char *fmt, ...) __attribute__((format(printf, 2, 3)));

static int report(int out_fd, const char *fmt, ...)
{
    char msg[ZEN_MAX_ERROR_REPORT];
    va_list ap;
    va_start(ap, fmt);
    int n = vsnprintf(msg, sizeof(msg), fmt, ap);
    va_end(ap);
    if (n < 0) {
        snprintf(msg, sizeof(msg), "unknown error");
    }
    for (char *p = msg; *p; p++) {
        if (*p == '\r' || *p == '\n') {
            *p = ' ';
        }
    }
    fprintf(stderr, "zen-client: %s\n", msg);
    zen_write_error_line(out_fd, msg);
    return 1;
}

/* ---------------------------- stdin protocol ------------------------- */

typedef struct {
    int fd;
    uint8_t buf[65536];
    size_t pos;
    size_t len;
    int eof;
} stdreader;

static ssize_t stdin_fill(stdreader *r)
{
    for (;;) {
        ssize_t n = read(r->fd, r->buf, sizeof(r->buf));
        if (n >= 0) {
            if (n == 0) {
                r->eof = 1;
            } else {
                r->pos = 0;
                r->len = (size_t)n;
            }
            return n;
        }
        if (errno != EINTR) {
            return -1;
        }
    }
}

/* -1 EOF, -2 read error (errno set). */
static int stdin_getc(stdreader *r)
{
    if (r->pos == r->len) {
        if (r->eof) {
            return -1;
        }
        if (stdin_fill(r) < 0) {
            return -2;
        }
        if (r->eof) {
            return -1;
        }
    }
    return r->buf[r->pos++];
}

static int read_metadata_line(stdreader *r, zbuf *line, int *too_long)
{
    *too_long = 0;
    for (;;) {
        int ch = stdin_getc(r);
        if (ch == -2) {
            zen_set_error("read stdin: %s", strerror(errno));
            return -1;
        }
        if (ch == -1) {
            if (line->len == 0 && !*too_long) {
                zen_set_error("metadata line is empty");
            } else if (*too_long) {
                zen_set_error("metadata line exceeds %u bytes",
                              (unsigned)ZEN_MAX_METADATA);
            } else {
                zen_set_error("metadata line is not newline-terminated");
            }
            return -1;
        }
        if (ch == '\n') {
            return 0;
        }
        if (line->len >= ZEN_MAX_METADATA) {
            *too_long = 1; /* drain to the newline, never grow past the cap */
            continue;
        }
        uint8_t byte = (uint8_t)ch;
        if (zbuf_add(line, &byte, 1) != 0) {
            return -1;
        }
    }
}

static int read_body(stdreader *r, zbuf *body)
{
    /* Bytes already buffered past the metadata newline belong to the body. */
    if (r->pos < r->len) {
        size_t avail = r->len - r->pos;
        if (avail > (size_t)ZEN_MAX_BODY - body->len) {
            zen_set_error("request body exceeds %u bytes", (unsigned)ZEN_MAX_BODY);
            return -1;
        }
        if (zbuf_add(body, r->buf + r->pos, avail) != 0) {
            return -1;
        }
        r->pos = r->len;
    }
    while (!r->eof) {
        ssize_t n = stdin_fill(r);
        if (n < 0) {
            zen_set_error("read stdin: %s", strerror(errno));
            return -1;
        }
        if (n == 0) {
            break;
        }
        size_t avail = (size_t)n;
        if (avail > (size_t)ZEN_MAX_BODY - body->len) {
            zen_set_error("request body exceeds %u bytes", (unsigned)ZEN_MAX_BODY);
            return -1;
        }
        if (zbuf_add(body, r->buf, avail) != 0) {
            return -1;
        }
        r->pos = r->len;
    }
    return 0;
}

static int line_is_blank(const zbuf *line)
{
    for (size_t i = 0; i < line->len; i++) {
        uint8_t c = line->p[i];
        if (c != ' ' && c != '\t' && c != '\r') {
            return 0;
        }
    }
    return 1;
}

/* ------------------------------- dial -------------------------------- */

static int tcp_dial(const char *host, const char *port, int timeout_ms,
                    char *err, size_t errcap)
{
    struct addrinfo hints;
    memset(&hints, 0, sizeof(hints));
    hints.ai_family = AF_UNSPEC;
    hints.ai_socktype = SOCK_STREAM;
    struct addrinfo *res = NULL;
    int gai = getaddrinfo(host, port, &hints, &res);
    if (gai != 0) {
        snprintf(err, errcap, "resolve %s: %s", host, gai_strerror(gai));
        return -1;
    }
    int64_t deadline = 0;
    if (timeout_ms > 0) {
        deadline = zen_now_ms() + timeout_ms;
    }
    int last = 0;
    int fd = -1;
    for (struct addrinfo *ai = res; ai; ai = ai->ai_next) {
        fd = socket(ai->ai_family, ai->ai_socktype, ai->ai_protocol);
        if (fd < 0) {
            last = errno;
            continue;
        }
        (void)fcntl(fd, F_SETFD, FD_CLOEXEC);
        int flags = fcntl(fd, F_GETFL, 0);
        if (flags < 0 || fcntl(fd, F_SETFL, flags | O_NONBLOCK) < 0) {
            last = errno;
            close(fd);
            fd = -1;
            continue;
        }
        int rc = connect(fd, ai->ai_addr, ai->ai_addrlen);
        if (rc != 0 && (errno == EINPROGRESS || errno == EINTR ||
                        errno == EALREADY)) {
            int wait = -1;
            if (timeout_ms > 0) {
                int64_t left = deadline - zen_now_ms();
                if (left <= 0) {
                    last = ETIMEDOUT;
                    close(fd);
                    fd = -1;
                    break; /* the shared budget is gone for every address */
                }
                wait = left > INT_MAX ? INT_MAX : (int)left;
            }
            if (zen_wait_fd(fd, 1, wait) != 0) {
                last = errno;
                close(fd);
                fd = -1;
                continue;
            }
            int soerr = 0;
            socklen_t slen = sizeof(soerr);
            if (getsockopt(fd, SOL_SOCKET, SO_ERROR, &soerr, &slen) != 0) {
                last = errno;
                close(fd);
                fd = -1;
                continue;
            }
            if (soerr != 0) {
                last = soerr;
                close(fd);
                fd = -1;
                continue;
            }
        } else if (rc != 0) {
            last = errno;
            close(fd);
            fd = -1;
            continue;
        }
        /* Back to blocking mode: the TLS module and zreader expect it. */
        flags = fcntl(fd, F_GETFL, 0);
        if (flags < 0 || fcntl(fd, F_SETFL, flags & ~O_NONBLOCK) < 0) {
            last = errno;
            close(fd);
            fd = -1;
            continue;
        }
        break;
    }
    freeaddrinfo(res);
    if (fd >= 0) {
        return fd;
    }
    snprintf(err, errcap, "%s", strerror(last ? last : ECONNREFUSED));
    return -1;
}

/* Split a dial override into host/port. zen_parse_hostport() enforces a
 * numeric port; an absent port is rejected because there is no scheme here
 * to supply the default. */
static int dial_parts(const char *spec, char *host, size_t host_cap, char *port,
                      size_t port_cap)
{
    if (zen_parse_hostport(spec, host, host_cap, port, port_cap) != 0) {
        return -1;
    }
    if (port[0] == '\0') {
        zen_set_error("dial address \"%s\" needs an explicit port", spec);
        return -1;
    }
    return 0;
}

/* ------------------------------- TLS -------------------------------- */

static int valid_sni(const char *sni)
{
    size_t n = strlen(sni);
    if (n == 0 || n > 255) {
        return 0;
    }
    for (size_t i = 0; i < n; i++) {
        uint8_t c = (uint8_t)sni[i];
        if (c <= 0x20 || c == 0x7f || c == '/') {
            return 0;
        }
    }
    return 1;
}

static int tls_write_all(struct ztls *t, const void *buf, size_t len)
{
    const uint8_t *p = buf;
    size_t off = 0;
    while (off < len) {
        ssize_t n = ztls_write(t, p + off, len - off);
        if (n > 0) {
            off += (size_t)n;
            continue;
        }
        if (zen_error()[0] == '\0') {
            zen_set_error("TLS write made no progress");
        }
        return -1;
    }
    return 0;
}

/* ------------------------------- shim -------------------------------- */

static int run_shim(int out_fd, zmeta *meta, const uint8_t *body, size_t body_len,
                    const zen_shim_opts *opts)
{
    zurl u;
    zbuf req = { 0 };
    zresp resp;
    memset(&resp, 0, sizeof(resp));
    struct ztls *tls = NULL;
    int fd = -1;
    int rc = 1;
    int timeout = zen_meta_timeout_ms(meta);
    char dial_buf[330];
    char dial_host[300];
    char dial_port[8];
    char dial_err[512];
    const char *dial_spec = NULL;
    const char *sni = NULL;
    int insecure = opts->insecure;

    if (zen_parse_url(meta->url, &u) != 0) {
        report(out_fd, "%s", zen_error());
        goto out;
    }
    if (zen_validate_headers(meta) != 0) {
        report(out_fd, "metadata headers: %s", zen_error());
        goto out;
    }

    if (insecure && !zen_is_loopback_host(u.host)) {
        report(out_fd, "refusing -insecure for non-loopback host \"%s\"", u.host);
        goto out;
    }

    sni = (opts->sni && opts->sni[0]) ? opts->sni : u.host;
    if (!valid_sni(sni)) {
        report(out_fd, "SNI \"%s\" contains invalid characters", sni);
        goto out;
    }

    /* Build the whole request before dialing: a 64 MiB body is streamed out
     * once and a bad header set never opens a socket. */
    if (zen_build_request(&u, meta, (opts->sni && opts->sni[0]) ? opts->sni : NULL,
                          body_len, &req) != 0) {
        report(out_fd, "%s", zen_error());
        goto out;
    }

    dial_spec = (opts->dial && opts->dial[0]) ? opts->dial : NULL;
    if (!dial_spec) {
        if (zen_dial_address(&u, dial_buf, sizeof(dial_buf)) != 0) {
            report(out_fd, "%s", zen_error());
            goto out;
        }
        dial_spec = dial_buf;
    }
    if (dial_parts(dial_spec, dial_host, sizeof(dial_host), dial_port,
                   sizeof(dial_port)) != 0) {
        report(out_fd, "%s", zen_error());
        goto out;
    }
    /* A loopback URL plus a remote -dial must not become an unverified
     * connection to that remote host. */
    if (insecure && !zen_is_loopback_host(dial_host)) {
        report(out_fd, "refusing -insecure for non-loopback dial host \"%s\"", dial_host);
        goto out;
    }
    if (insecure) {
        fprintf(stderr, "zen-client: WARNING: TLS certificate verification disabled (-insecure)\n");
    }

    fd = tcp_dial(dial_host, dial_port, timeout, dial_err, sizeof(dial_err));
    if (fd < 0) {
        report(out_fd, "dial %s: %s", dial_spec, dial_err);
        goto out;
    }

    if (u.tls) {
        const uint8_t *ca = NULL;
        size_t ca_len = 0;
        if (!insecure) {
            if (zen_ca_pem_len == 0) {
                report(out_fd, "CA bundle is empty; refusing to connect without certificate verification");
                goto out;
            }
            ca = zen_ca_pem;
            ca_len = zen_ca_pem_len;
        }
#ifdef PAIG_ZEN_EXTERNAL_CA
        if (zen_ca_chain != NULL) {
            tls = ztls_new_with_ca(fd, sni, insecure, zen_ca_chain, timeout);
        } else {
            tls = ztls_new(fd, sni, insecure, ca, ca_len, timeout);
        }
#else
        tls = ztls_new(fd, sni, insecure, ca, ca_len, timeout);
#endif
        if (!tls) {
            report(out_fd, "TLS setup: %s", zen_error());
            goto out;
        }
        if (ztls_send_hello(tls) != 0 || ztls_handshake(tls) != 0) {
            report(out_fd, "TLS handshake with %s: %s", sni, zen_error());
            goto out;
        }
    }

    if (tls) {
        if (tls_write_all(tls, req.p, req.len) != 0 ||
            (body_len > 0 && tls_write_all(tls, body, body_len) != 0)) {
            report(out_fd, "write request: %s", zen_error());
            goto out;
        }
    } else {
        if (zen_send_all(fd, req.p, req.len, timeout) != 0 ||
            (body_len > 0 && zen_send_all(fd, body, body_len, timeout) != 0)) {
            report(out_fd, "write request: %s", strerror(errno));
            goto out;
        }
    }

    zreader reader;
    zen_reader_init(&reader, fd, tls, timeout);
    if (zen_read_head(&reader, &resp) != 0) {
        report(out_fd, "read response: %s", zen_error());
        goto out;
    }

    if (zen_write_status_line(out_fd, &resp) != 0) {
        fprintf(stderr, "zen-client: cannot write status line: %s\n",
                strerror(errno));
        goto out;
    }
    if (zen_stream_body(&reader, &resp, out_fd) != 0) {
        fprintf(stderr, "zen-client: response stream: %s\n", zen_error());
        goto out;
    }
    rc = 0;

out:
    if (tls) {
        ztls_free(tls);
    }
    if (fd >= 0) {
        close(fd);
    }
    zen_resp_free(&resp);
    zbuf_free(&req);
    return rc;
}

/* --------------------------- in-process shim ------------------------- */

int zen_shim_run(int in_fd, int out_fd, const zen_shim_opts *opts)
{
    zen_shim_opts defaults = { 0, -1, NULL, NULL };
    if (!opts) {
        opts = &defaults;
    }

    stdreader in;
    memset(&in, 0, sizeof(in));
    in.fd = in_fd;
    zbuf line = { 0 };
    zbuf body = { 0 };
    int too_long = 0;
    int line_rc = read_metadata_line(&in, &line, &too_long);
    int body_rc = read_body(&in, &body);

    if (line_rc != 0) {
        int rc = report(out_fd, "%s", zen_error());
        zbuf_free(&line);
        zbuf_free(&body);
        return rc;
    }
    if (too_long) {
        int rc = report(out_fd, "metadata line exceeds %u bytes",
                        (unsigned)ZEN_MAX_METADATA);
        zbuf_free(&line);
        zbuf_free(&body);
        return rc;
    }
    if (body_rc != 0) {
        int rc = report(out_fd, "%s", zen_error());
        zbuf_free(&line);
        zbuf_free(&body);
        return rc;
    }
    if (line_is_blank(&line)) {
        int rc = report(out_fd, "metadata line is empty");
        zbuf_free(&line);
        zbuf_free(&body);
        return rc;
    }

    zmeta meta;
    if (zen_parse_metadata(line.p, line.len, &meta) != 0) {
        int rc = report(out_fd, "%s", zen_error());
        zbuf_free(&line);
        zbuf_free(&body);
        return rc;
    }

    zen_set_cancel_fd(opts->cancel_fd);
    int rc = run_shim(out_fd, &meta, body.p, body.len, opts);
    zen_set_cancel_fd(-1);
    zen_meta_free(&meta);
    zbuf_free(&line);
    zbuf_free(&body);
    return rc;
}
