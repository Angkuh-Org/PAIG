/*
 * HTTP/1.1 request builder, response head parser and body streaming.
 *
 * The request side follows the proven Bun 1.4.4 wire shape: the 13 headers in
 * their proven order, case-insensitive metadata lookup with conflict
 * rejection, a computed Content-Length, and caller extras appended afterwards
 * in sorted order. The response side does interim 1xx skipping, obs-fold
 * joining, lower-cased duplicate joining, and incremental
 * chunked/content-length/EOF framing. Nothing is buffered beyond one 16 KiB
 * line or one 32 KiB body piece, so an SSE stream
 * reaches stdout as it arrives.
 */
#include "zen.h"

#include <ctype.h>
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

/* --------------------------- request build --------------------------- */

typedef struct {
    const char *name;
    const char *def;
} wire_header;

/* The proven Bun 1.4.4 order. Content-Length has a NULL default because it is
 * always computed from the body length. */
static const wire_header wire_order[] = {
    { "Authorization", "" },
    { "Content-Type", "" },
    { "User-Agent", "" },
    { "x-opencode-client", "" },
    { "x-opencode-session", "" },
    { "x-opencode-session-id", "" },
    { "x-session-affinity", "" },
    { "x-session-id", "" },
    { "Connection", "keep-alive" },
    { "Accept", "*/*" },
    { "Host", "" },
    { "Accept-Encoding", "identity" },
    { "Content-Length", NULL },
};
#define WIRE_COUNT (sizeof(wire_order) / sizeof(wire_order[0]))

static int token_char(uint8_t c)
{
    if ((c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') ||
        (c >= '0' && c <= '9')) {
        return 1;
    }
    return strchr("!#$%&'*+-.^_`|~", c) != NULL;
}

/* CR and LF are dropped in place (the task's contract) so a value can never
 * split the header block; every other control byte is rejected. */
static void strip_crlf(char *s)
{
    char *w = s;
    for (char *r = s; *r; r++) {
        if (*r != '\r' && *r != '\n') {
            *w++ = *r;
        }
    }
    *w = '\0';
}

int zen_valid_header_name(const char *name)
{
    size_t n = strlen(name);
    if (n == 0 || n > ZEN_MAX_HEADER_NAME) {
        return 0;
    }
    for (size_t i = 0; i < n; i++) {
        if (!token_char((uint8_t)name[i])) {
            return 0;
        }
    }
    return 1;
}

int zen_valid_header_value(const char *value)
{
    size_t n = strlen(value);
    if (n > ZEN_MAX_HEADER_VALUE) {
        return 0;
    }
    for (size_t i = 0; i < n; i++) {
        uint8_t c = (uint8_t)value[i];
        if (c == 0 || c == 0x7f) {
            return 0;
        }
        if (c < 0x20 && c != '\t') {
            return 0;
        }
    }
    return 1;
}

int zen_validate_headers(const zmeta *m)
{
    size_t total = 0;
    for (int i = 0; i < m->nheaders; i++) {
        const char *name = m->headers[i].name;
        const char *value = m->headers[i].value;
        if (!zen_valid_header_name(name)) {
            zen_set_error("invalid header name \"%s\"", name);
            return -1;
        }
        if (!zen_valid_header_value(value)) {
            zen_set_error("header \"%s\" has an invalid value", name);
            return -1;
        }
        total += strlen(name) + strlen(value);
    }
    if (total > ZEN_MAX_HEADER_BYTES) {
        zen_set_error("metadata headers exceed %d bytes", ZEN_MAX_HEADER_BYTES);
        return -1;
    }
    return 0;
}

/* Case-insensitive lookup that refuses two different values under different
 * casings, so map/array order cannot silently change the wire request. */
static int lookup_header(const zmeta *m, const char *want, const char **out,
                         int *found)
{
    const char *value = NULL;
    int seen = 0;
    for (int i = 0; i < m->nheaders; i++) {
        if (strcasecmp(m->headers[i].name, want) != 0) {
            continue;
        }
        if (seen && strcmp(value, m->headers[i].value) != 0) {
            zen_set_error("conflicting values for header \"%s\"", want);
            return -1;
        }
        value = m->headers[i].value;
        seen = 1;
    }
    *out = value;
    *found = seen;
    return 0;
}

static int known_header(const char *name)
{
    for (size_t i = 0; i < WIRE_COUNT; i++) {
        if (strcasecmp(name, wire_order[i].name) == 0) {
            return 1;
        }
    }
    return 0;
}

static int canonical_name(const char *lower, char *out, size_t cap)
{
    size_t n = strlen(lower);
    if (n + 1 > cap) {
        return -1;
    }
    int upper = 1;
    for (size_t i = 0; i < n; i++) {
        char c = lower[i];
        if (c == '-') {
            upper = 1;
            out[i] = c;
            continue;
        }
        out[i] = upper ? (char)toupper((unsigned char)c)
                       : (char)tolower((unsigned char)c);
        upper = 0;
    }
    out[n] = '\0';
    return 0;
}

typedef struct {
    char *lower;
    char *value;
} extra_header;

static int extra_cmp(const void *a, const void *b)
{
    const extra_header *x = a;
    const extra_header *y = b;
    return strcmp(x->lower, y->lower);
}

int zen_build_request(const zurl *u, const zmeta *m, const char *host_override,
                      size_t body_len, zbuf *out)
{
    size_t target_len = strlen(u->target);
    if (target_len == 0 || u->target[0] != '/') {
        zen_set_error("request target is not origin-form");
        return -1;
    }
    for (size_t i = 0; i < target_len; i++) {
        uint8_t c = (uint8_t)u->target[i];
        if (c <= 0x20 || c == 0x7f) {
            zen_set_error("request target contains an invalid byte");
            return -1;
        }
    }
    if (zbuf_addstr(out, "POST ") != 0 || zbuf_addstr(out, u->target) != 0 ||
        zbuf_addstr(out, " HTTP/1.1\r\n") != 0) {
        zen_set_error("out of memory building the request");
        return -1;
    }

    char host_buf[300];
    if (zen_host_header(u, host_buf, sizeof(host_buf)) != 0) {
        return -1;
    }

    for (size_t i = 0; i < WIRE_COUNT; i++) {
        const wire_header *w = &wire_order[i];
        char *tmp = NULL;
        const char *value = NULL;
        if (strcasecmp(w->name, "content-length") == 0) {
            char digits[32];
            snprintf(digits, sizeof(digits), "%zu", body_len);
            if (zbuf_addstr(out, "Content-Length: ") != 0 ||
                zbuf_addstr(out, digits) != 0 || zbuf_addstr(out, "\r\n") != 0) {
                zen_set_error("out of memory building the request");
                return -1;
            }
            continue;
        }
        int found = 0;
        if (lookup_header(m, w->name, &value, &found) != 0) {
            return -1;
        }
        if (strcasecmp(w->name, "Host") == 0) {
            if (found && value[0] != '\0') {
                tmp = strdup(value);
                if (!tmp) {
                    zen_set_error("out of memory building the request");
                    return -1;
                }
                strip_crlf(tmp);
                value = tmp;
            } else if (host_override && host_override[0]) {
                value = host_override;
            } else {
                value = host_buf;
            }
        } else if (found) {
            tmp = strdup(value);
            if (!tmp) {
                zen_set_error("out of memory building the request");
                return -1;
            }
            strip_crlf(tmp);
            value = tmp;
        } else {
            value = w->def;
        }
        if (!value || value[0] == '\0') {
            free(tmp);
            continue; /* absent optional header: omit, never send "Name: " */
        }
        if (!zen_valid_header_value(value)) {
            zen_set_error("header \"%s\" has an invalid value", w->name);
            free(tmp);
            return -1;
        }
        int bad = zbuf_addstr(out, w->name) != 0 || zbuf_addstr(out, ": ") != 0 ||
                  zbuf_addstr(out, value) != 0 || zbuf_addstr(out, "\r\n") != 0;
        free(tmp);
        if (bad) {
            zen_set_error("out of memory building the request");
            return -1;
        }
    }

    /* Extra caller headers, appended after the proven list in sorted order.
     * Transfer-Encoding is dropped because the helper always frames with a
     * computed Content-Length. */
    extra_header extras[ZEN_MAX_HEADERS];
    int nextra = 0;
    for (int i = 0; i < m->nheaders; i++) {
        const char *name = m->headers[i].name;
        if (known_header(name) || strcasecmp(name, "Transfer-Encoding") == 0) {
            continue;
        }
        char lower[ZEN_MAX_HEADER_NAME + 1];
        size_t n = strlen(name);
        if (n > ZEN_MAX_HEADER_NAME) {
            zen_set_error("header name is too long");
            return -1;
        }
        for (size_t k = 0; k < n; k++) {
            lower[k] = (char)tolower((unsigned char)name[k]);
        }
        lower[n] = '\0';
        int slot = -1;
        for (int k = 0; k < nextra; k++) {
            if (strcmp(extras[k].lower, lower) == 0) {
                slot = k;
                break;
            }
        }
        if (slot >= 0) {
            if (strcmp(extras[slot].value, m->headers[i].value) != 0) {
                zen_set_error("conflicting values for header \"%s\"", name);
                goto fail_extras;
            }
            continue;
        }
        extras[nextra].lower = strdup(lower);
        extras[nextra].value = strdup(m->headers[i].value);
        if (!extras[nextra].lower || !extras[nextra].value) {
            free(extras[nextra].lower);
            free(extras[nextra].value);
            zen_set_error("out of memory building the request");
            goto fail_extras;
        }
        strip_crlf(extras[nextra].value);
        if (!zen_valid_header_value(extras[nextra].value)) {
            zen_set_error("header \"%s\" has an invalid value", name);
            goto fail_extras;
        }
        nextra++;
        continue;
    fail_extras:
        for (int k = 0; k <= nextra; k++) {
            free(extras[k].lower);
            free(extras[k].value);
        }
        return -1;
    }
    if (nextra > 1) {
        qsort(extras, (size_t)nextra, sizeof(extras[0]), extra_cmp);
    }
    for (int i = 0; i < nextra; i++) {
        char name[ZEN_MAX_HEADER_NAME + 1];
        if (canonical_name(extras[i].lower, name, sizeof(name)) != 0 ||
            zbuf_addstr(out, name) != 0 || zbuf_addstr(out, ": ") != 0 ||
            zbuf_addstr(out, extras[i].value) != 0 ||
            zbuf_addstr(out, "\r\n") != 0) {
            zen_set_error("out of memory building the request");
            for (int k = 0; k < nextra; k++) {
                free(extras[k].lower);
                free(extras[k].value);
            }
            return -1;
        }
    }
    for (int i = 0; i < nextra; i++) {
        free(extras[i].lower);
        free(extras[i].value);
    }
    if (zbuf_addstr(out, "\r\n") != 0) {
        zen_set_error("out of memory building the request");
        return -1;
    }
    return 0;
}

/* ------------------------------ reader ------------------------------- */

void zen_reader_init(zreader *r, int fd, struct ztls *tls, int timeout_ms)
{
    memset(r, 0, sizeof(*r));
    r->fd = fd;
    r->tls = tls;
    r->timeout_ms = timeout_ms;
}

ssize_t zen_reader_read(zreader *r, void *buf, size_t len)
{
    if (r->tls) {
        return ztls_read(r->tls, buf, len);
    }
    ssize_t n = zen_recv_timeout(r->fd, buf, len, r->timeout_ms);
    if (n < 0) {
        zen_set_error("response read: %s", strerror(errno));
    }
    return n;
}

/* Reads one LF-terminated line, strips CR/LF, and never buffers more than
 * cap bytes. Returns 0, or -1 with zen_error set (including EOF mid-line). */
static int rd_line(zreader *r, char *out, size_t cap, size_t *out_len)
{
    size_t n = 0;
    for (;;) {
        if (r->pos == r->len) {
            if (r->eof) {
                zen_set_error("connection closed in the middle of a line");
                return -1;
            }
            ssize_t got = zen_reader_read(r, r->buf, sizeof(r->buf));
            if (got < 0) {
                return -1;
            }
            if (got == 0) {
                r->eof = 1;
                zen_set_error("connection closed before the line ended");
                return -1;
            }
            r->pos = 0;
            r->len = (size_t)got;
        }
        uint8_t ch = r->buf[r->pos++];
        if (ch == '\n') {
            if (n > 0 && out[n - 1] == '\r') {
                n--;
            }
            out[n] = '\0';
            *out_len = n;
            return 0;
        }
        if (n + 1 >= cap) {
            zen_set_error("response line exceeds %zu bytes", cap);
            return -1;
        }
        out[n++] = (char)ch;
    }
}

static int add_resp_header(zresp *resp, const char *name, const char *value)
{
    char lower[ZEN_MAX_HEADER_NAME + 1];
    size_t n = strlen(name);
    if (n == 0 || n > ZEN_MAX_HEADER_NAME) {
        return -1;
    }
    for (size_t i = 0; i < n; i++) {
        lower[i] = (char)tolower((unsigned char)name[i]);
    }
    lower[n] = '\0';
    for (int i = 0; i < resp->nh; i++) {
        if (strcmp(resp->h[i].name, lower) == 0) {
            size_t old = strlen(resp->h[i].value);
            size_t add = strlen(value);
            char *joined = realloc(resp->h[i].value, old + 2 + add + 1);
            if (!joined) {
                return -1;
            }
            joined[old] = ',';
            joined[old + 1] = ' ';
            memcpy(joined + old + 2, value, add + 1);
            resp->h[i].value = joined;
            return 0;
        }
    }
    if (resp->nh >= ZEN_MAX_HEADERS) {
        return -1;
    }
    resp->h[resp->nh].name = strdup(lower);
    resp->h[resp->nh].value = strdup(value);
    if (!resp->h[resp->nh].name || !resp->h[resp->nh].value) {
        free(resp->h[resp->nh].name);
        free(resp->h[resp->nh].value);
        return -1;
    }
    resp->nh++;
    return 0;
}

void zen_resp_free(zresp *resp)
{
    for (int i = 0; i < resp->nh; i++) {
        free(resp->h[i].name);
        free(resp->h[i].value);
    }
    resp->nh = 0;
}

static int parse_content_length(const char *value, int64_t *out)
{
    while (*value == ' ' || *value == '\t') {
        value++;
    }
    size_t n = strlen(value);
    while (n > 0 && (value[n - 1] == ' ' || value[n - 1] == '\t')) {
        n--;
    }
    if (n == 0) {
        return -1;
    }
    int64_t v = 0;
    for (size_t i = 0; i < n; i++) {
        if (value[i] < '0' || value[i] > '9') {
            return -1;
        }
        if (v > (INT64_MAX - (value[i] - '0')) / 10) {
            return -1;
        }
        v = v * 10 + (value[i] - '0');
    }
    *out = v;
    return 0;
}

int zen_read_head(zreader *r, zresp *resp)
{
    memset(resp, 0, sizeof(*resp));
    resp->content_length = -1;
    char line[ZEN_MAX_HEADER_VALUE + 2];
    int interim = 0;
    for (;;) {
        size_t len = 0;
        if (rd_line(r, line, 8194, &len) != 0) {
            zen_set_error("status line: %s", zen_error());
            return -1;
        }
        /* HTTP/x.y SP NNN [reason] */
        if (len < 12 || strncmp(line, "HTTP/", 5) != 0) {
            zen_set_error("malformed status line");
            return -1;
        }
        char *sp = memchr(line, ' ', len);
        if (!sp) {
            zen_set_error("malformed status line");
            return -1;
        }
        const char *code = sp + 1;
        size_t code_len = len - (size_t)(code - line);
        const char *code_end = memchr(code, ' ', code_len);
        size_t digits = code_end ? (size_t)(code_end - code) : code_len;
        if (digits != 3 || code[0] < '1' || code[0] > '9' ||
            code[1] < '0' || code[1] > '9' || code[2] < '0' || code[2] > '9') {
            zen_set_error("malformed status code");
            return -1;
        }
        int status = (code[0] - '0') * 100 + (code[1] - '0') * 10 + (code[2] - '0');

        resp->nh = 0;
        size_t header_bytes = 0;
        for (;;) {
            size_t hlen = 0;
            if (rd_line(r, line, sizeof(line), &hlen) != 0) {
                zen_set_error("response headers: %s", zen_error());
                zen_resp_free(resp);
                return -1;
            }
            if (hlen == 0) {
                break;
            }
            header_bytes += hlen + 2;
            if (header_bytes > ZEN_MAX_HEADER_BYTES) {
                zen_set_error("response headers exceed %d bytes", ZEN_MAX_HEADER_BYTES);
                zen_resp_free(resp);
                return -1;
            }
            if (line[0] == ' ' || line[0] == '\t') {
                if (resp->nh == 0) {
                    zen_set_error("header continuation without a header");
                    zen_resp_free(resp);
                    return -1;
                }
                const char *cont = line;
                while (*cont == ' ' || *cont == '\t') {
                    cont++;
                }
                if (!zen_valid_header_value(cont)) {
                    zen_set_error("response header continuation is invalid");
                    zen_resp_free(resp);
                    return -1;
                }
                zheader *last = &resp->h[resp->nh - 1];
                size_t old = strlen(last->value);
                size_t add = strlen(cont);
                char *joined = realloc(last->value, old + 2 + add + 1);
                if (!joined) {
                    zen_set_error("out of memory parsing response headers");
                    zen_resp_free(resp);
                    return -1;
                }
                joined[old] = ' ';
                memcpy(joined + old + 1, cont, add + 1);
                last->value = joined;
                continue;
            }
            char *colon = memchr(line, ':', hlen);
            if (!colon || colon == line) {
                zen_set_error("malformed response header");
                zen_resp_free(resp);
                return -1;
            }
            *colon = '\0';
            if (!zen_valid_header_name(line)) {
                zen_set_error("invalid response header name");
                zen_resp_free(resp);
                return -1;
            }
            char *value = colon + 1;
            while (*value == ' ' || *value == '\t') {
                value++;
            }
            size_t vlen = strlen(value);
            while (vlen > 0 && (value[vlen - 1] == ' ' || value[vlen - 1] == '\t')) {
                value[--vlen] = '\0';
            }
            if (!zen_valid_header_value(value)) {
                zen_set_error("invalid value for response header \"%s\"", line);
                zen_resp_free(resp);
                return -1;
            }
            if (add_resp_header(resp, line, value) != 0) {
                zen_set_error("out of memory parsing response headers");
                zen_resp_free(resp);
                return -1;
            }
        }

        if (status >= 100 && status < 200 && status != 101) {
            if (++interim > 16) {
                zen_set_error("too many interim responses");
                zen_resp_free(resp);
                return -1;
            }
            continue;
        }
        resp->status = status;
        break;
    }

    if (resp->status == 204 || resp->status == 304 ||
        (resp->status >= 100 && resp->status < 200)) {
        resp->no_body = 1;
        resp->content_length = 0;
        return 0;
    }

    const char *te = NULL;
    for (int i = 0; i < resp->nh; i++) {
        if (strcmp(resp->h[i].name, "transfer-encoding") == 0) {
            te = resp->h[i].value;
            break;
        }
    }
    if (te && te[0]) {
        const char *p = te;
        while (*p) {
            const char *comma = strchr(p, ',');
            size_t n = comma ? (size_t)(comma - p) : strlen(p);
            while (n > 0 && (*p == ' ' || *p == '\t')) {
                p++;
                n--;
            }
            while (n > 0 && (p[n - 1] == ' ' || p[n - 1] == '\t')) {
                n--;
            }
            if (n != 0 && !(n == 8 && strncasecmp(p, "identity", 8) == 0)) {
                if (n == 7 && strncasecmp(p, "chunked", 7) == 0) {
                    resp->chunked = 1;
                } else {
                    zen_set_error("unsupported transfer-encoding \"%.*s\"", (int)n, p);
                    zen_resp_free(resp);
                    return -1;
                }
            }
            if (!comma) {
                break;
            }
            p = comma + 1;
        }
    }
    if (!resp->chunked) {
        for (int i = 0; i < resp->nh; i++) {
            if (strcmp(resp->h[i].name, "content-length") == 0) {
                if (parse_content_length(resp->h[i].value, &resp->content_length) != 0) {
                    zen_set_error("malformed content-length \"%s\"", resp->h[i].value);
                    zen_resp_free(resp);
                    return -1;
                }
                break;
            }
        }
    }
    return 0;
}

/* ---------------------------- body stream ---------------------------- */

static int write_out(int fd, const void *p, size_t n)
{
    if (zen_write_all(fd, p, n) != 0) {
        zen_set_error("write stdout: %s", strerror(errno));
        return -1;
    }
    return 0;
}

static int stream_exact(zreader *r, int out_fd, int64_t n)
{
    if (n < 0) {
        zen_set_error("negative body length");
        return -1;
    }
    while (n > 0) {
        if (r->pos == r->len) {
            ssize_t got = zen_reader_read(r, r->buf, sizeof(r->buf));
            if (got < 0) {
                return -1;
            }
            if (got == 0) {
                zen_set_error("unexpected EOF with %lld body bytes left",
                              (long long)n);
                return -1;
            }
            r->pos = 0;
            r->len = (size_t)got;
        }
        size_t avail = r->len - r->pos;
        size_t take = (uint64_t)avail < (uint64_t)n ? avail : (size_t)n;
        if (write_out(out_fd, r->buf + r->pos, take) != 0) {
            return -1;
        }
        r->pos += take;
        n -= (int64_t)take;
    }
    return 0;
}

static int stream_until_eof(zreader *r, int out_fd)
{
    for (;;) {
        if (r->pos < r->len) {
            if (write_out(out_fd, r->buf + r->pos, r->len - r->pos) != 0) {
                return -1;
            }
            r->pos = r->len;
        }
        ssize_t got = zen_reader_read(r, r->buf, sizeof(r->buf));
        if (got < 0) {
            return -1;
        }
        if (got == 0) {
            return 0;
        }
        r->pos = 0;
        r->len = (size_t)got;
    }
}

static int read_trailers(zreader *r)
{
    char line[ZEN_MAX_HEADER_VALUE + 2];
    int lines = 0;
    size_t total = 0;
    for (;;) {
        size_t len = 0;
        if (rd_line(r, line, sizeof(line), &len) != 0) {
            zen_set_error("trailer: %s", zen_error());
            return -1;
        }
        if (len == 0) {
            return 0;
        }
        if (++lines > 100 || (total += len + 2) > (64u << 10)) {
            zen_set_error("trailers exceed limits");
            return -1;
        }
        char *colon = memchr(line, ':', len);
        if (!colon || colon == line) {
            zen_set_error("malformed trailer");
            return -1;
        }
        *colon = '\0';
        if (!zen_valid_header_name(line)) {
            zen_set_error("invalid trailer name");
            return -1;
        }
        char *value = colon + 1;
        while (*value == ' ' || *value == '\t') {
            value++;
        }
        size_t vlen = strlen(value);
        while (vlen > 0 && (value[vlen - 1] == ' ' || value[vlen - 1] == '\t')) {
            value[--vlen] = '\0';
        }
        if (!zen_valid_header_value(value)) {
            zen_set_error("invalid trailer value");
            return -1;
        }
    }
}

static int stream_chunked(zreader *r, int out_fd)
{
    char line[16386];
    for (;;) {
        size_t len = 0;
        if (rd_line(r, line, sizeof(line), &len) != 0) {
            zen_set_error("chunk size: %s", zen_error());
            return -1;
        }
        char *semi = memchr(line, ';', len);
        size_t text_len = semi ? (size_t)(semi - line) : len;
        char *p = line;
        while (text_len > 0 && (*p == ' ' || *p == '\t')) {
            p++;
            text_len--;
        }
        while (text_len > 0 && (p[text_len - 1] == ' ' || p[text_len - 1] == '\t')) {
            text_len--;
        }
        if (text_len == 0) {
            zen_set_error("empty chunk size");
            return -1;
        }
        uint64_t size = 0;
        for (size_t i = 0; i < text_len; i++) {
            int digit;
            char c = p[i];
            if (c >= '0' && c <= '9') {
                digit = c - '0';
            } else if (c >= 'a' && c <= 'f') {
                digit = c - 'a' + 10;
            } else if (c >= 'A' && c <= 'F') {
                digit = c - 'A' + 10;
            } else {
                zen_set_error("bad chunk size \"%.*s\"", (int)text_len, p);
                return -1;
            }
            if (size > (UINT64_MAX >> 4)) {
                zen_set_error("chunk size overflows");
                return -1;
            }
            size = (size << 4) | (uint64_t)digit;
        }
        if (size > (uint64_t)INT64_MAX) {
            zen_set_error("chunk size is too large");
            return -1;
        }
        if (size == 0) {
            return read_trailers(r);
        }
        if (stream_exact(r, out_fd, (int64_t)size) != 0) {
            return -1;
        }
        uint8_t crlf[2];
        for (int i = 0; i < 2; i++) {
            if (r->pos == r->len) {
                ssize_t got = zen_reader_read(r, r->buf, sizeof(r->buf));
                if (got < 0) {
                    return -1;
                }
                if (got == 0) {
                    zen_set_error("unexpected EOF at the chunk terminator");
                    return -1;
                }
                r->pos = 0;
                r->len = (size_t)got;
            }
            crlf[i] = r->buf[r->pos++];
        }
        if (crlf[0] != '\r' || crlf[1] != '\n') {
            zen_set_error("bad chunk terminator 0x%02x 0x%02x", crlf[0], crlf[1]);
            return -1;
        }
    }
}

int zen_stream_body(zreader *r, const zresp *resp, int out_fd)
{
    if (resp->no_body) {
        return 0;
    }
    if (resp->chunked) {
        return stream_chunked(r, out_fd);
    }
    if (resp->content_length >= 0) {
        return stream_exact(r, out_fd, resp->content_length);
    }
    return stream_until_eof(r, out_fd);
}

/* --------------------------- JSON output ----------------------------- */

static void utf8_replacement(zbuf *b)
{
    zbuf_add(b, "\xef\xbf\xbd", 3);
}

/* Emits a JSON string body (no surrounding quotes). Invalid UTF-8 becomes
 * U+FFFD so PAIG's JSON parser always sees a valid document. */
static void json_escape(zbuf *b, const char *s)
{
    const uint8_t *p = (const uint8_t *)s;
    while (*p) {
        uint8_t c = *p;
        if (c == '"' || c == '\\') {
            char pair[2] = { '\\', (char)c };
            zbuf_add(b, pair, 2);
            p++;
            continue;
        }
        if (c < 0x20) {
            char esc[8];
            switch (c) {
            case '\n': zbuf_addstr(b, "\\n"); break;
            case '\r': zbuf_addstr(b, "\\r"); break;
            case '\t': zbuf_addstr(b, "\\t"); break;
            default:
                snprintf(esc, sizeof(esc), "\\u%04x", c);
                zbuf_addstr(b, esc);
                break;
            }
            p++;
            continue;
        }
        if (c < 0x80) {
            zbuf_add(b, p, 1);
            p++;
            continue;
        }
        size_t need;
        if (c >= 0xc2 && c <= 0xdf) {
            need = 1;
        } else if (c >= 0xe0 && c <= 0xef) {
            need = 2;
        } else if (c >= 0xf0 && c <= 0xf4) {
            need = 3;
        } else {
            utf8_replacement(b);
            p++;
            continue;
        }
        int ok = 1;
        for (size_t i = 1; i <= need; i++) {
            if ((p[i] & 0xc0) != 0x80) {
                ok = 0;
                break;
            }
        }
        if (ok && need == 2) {
            if ((c == 0xe0 && p[1] < 0xa0) || (c == 0xed && p[1] > 0x9f)) {
                ok = 0;
            }
        }
        if (ok && need == 3) {
            if ((c == 0xf0 && p[1] < 0x90) || (c == 0xf4 && p[1] > 0x8f)) {
                ok = 0;
            }
        }
        if (!ok) {
            utf8_replacement(b);
            p++;
            continue;
        }
        zbuf_add(b, p, need + 1);
        p += need + 1;
    }
}

static int resp_cmp(const void *a, const void *b)
{
    const zheader *x = *(const zheader *const *)a;
    const zheader *y = *(const zheader *const *)b;
    return strcmp(x->name, y->name);
}

int zen_write_status_line(int out_fd, const zresp *resp)
{
    zbuf b = { 0 };
    char head[64];
    snprintf(head, sizeof(head), "{\"status\":%d,\"headers\":{", resp->status);
    if (zbuf_addstr(&b, head) != 0) {
        zbuf_free(&b);
        return -1;
    }
    const zheader *order[ZEN_MAX_HEADERS];
    for (int i = 0; i < resp->nh; i++) {
        order[i] = &resp->h[i];
    }
    if (resp->nh > 1) {
        qsort(order, (size_t)resp->nh, sizeof(order[0]), resp_cmp);
    }
    for (int i = 0; i < resp->nh; i++) {
        if (i) {
            zbuf_addstr(&b, ",");
        }
        zbuf_addstr(&b, "\"");
        json_escape(&b, order[i]->name);
        zbuf_addstr(&b, "\":\"");
        json_escape(&b, order[i]->value);
        zbuf_addstr(&b, "\"");
    }
    if (zbuf_addstr(&b, "}}\n") != 0) {
        zbuf_free(&b);
        return -1;
    }
    int rc = zen_write_all(out_fd, b.p, b.len);
    zbuf_free(&b);
    return rc;
}

int zen_write_error_line(int out_fd, const char *message)
{
    zbuf b = { 0 };
    if (zbuf_addstr(&b, "{\"status\":0,\"error\":\"") != 0) {
        zbuf_free(&b);
        return -1;
    }
    json_escape(&b, message);
    if (zbuf_addstr(&b, "\"}\n") != 0) {
        zbuf_free(&b);
        return -1;
    }
    int rc = zen_write_all(out_fd, b.p, b.len);
    zbuf_free(&b);
    return rc;
}
