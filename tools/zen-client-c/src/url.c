/*
 * URL and host helpers.
 *
 * The metadata URL comes from PAIG's Rust `Url`, so it is already escaped and
 * normalized. We accept exactly the origin-form subset the helper can dial:
 *   http(s)://host[:port]/path[?query]
 * and reject userinfo, fragments, opaque forms, whitespace and invalid percent
 * escapes. The request target is passed through verbatim (Go's url.RequestURI
 * re-encodes; PAIG never sends a path that needs it, and re-encoding here would
 * create a second source of truth for the wire path).
 */
#include "zen.h"

#include <arpa/inet.h>
#include <ctype.h>
#include <stdio.h>
#include <string.h>

static int ishex(uint8_t c)
{
    return (c >= '0' && c <= '9') || (c >= 'a' && c <= 'f') || (c >= 'A' && c <= 'F');
}

static int has_bad_bytes(const char *s, size_t n)
{
    for (size_t i = 0; i < n; i++) {
        uint8_t c = (uint8_t)s[i];
        if (c <= 0x20 || c == 0x7f) {
            return 1;
        }
    }
    return 0;
}

static int valid_target(const char *s, size_t n)
{
    if (n == 0 || s[0] != '/') {
        return 0;
    }
    for (size_t i = 0; i < n; i++) {
        uint8_t c = (uint8_t)s[i];
        if (c <= 0x20 || c == 0x7f) {
            return 0;
        }
    }
    return 1;
}

static int parse_port(const char *s, size_t n, char out[8])
{
    if (n == 0 || n > 5) {
        return -1;
    }
    unsigned v = 0;
    for (size_t i = 0; i < n; i++) {
        if (s[i] < '0' || s[i] > '9') {
            return -1;
        }
        v = v * 10 + (unsigned)(s[i] - '0');
    }
    if (v == 0 || v > 65535) {
        return -1;
    }
    snprintf(out, 8, "%u", v);
    return 0;
}

int zen_parse_url(const char *raw, zurl *u)
{
    memset(u, 0, sizeof(*u));
    if (!raw || raw[0] == '\0') {
        zen_set_error("metadata url is empty");
        return -1;
    }
    size_t len = strlen(raw);
    if (len > ZEN_MAX_URL) {
        zen_set_error("metadata url exceeds %d bytes", ZEN_MAX_URL);
        return -1;
    }
    if (strchr(raw, '#') != NULL) {
        zen_set_error("metadata url must not contain a fragment");
        return -1;
    }
    for (size_t i = 0; i < len; i++) {
        uint8_t c = (uint8_t)raw[i];
        if (c < 0x21 || c == 0x7f) {
            zen_set_error("metadata url contains whitespace or a control byte");
            return -1;
        }
        if (c == '%') {
            if (i + 2 >= len || !ishex((uint8_t)raw[i + 1]) || !ishex((uint8_t)raw[i + 2])) {
                zen_set_error("metadata url has an invalid percent escape");
                return -1;
            }
        }
    }

    const char *sep = strstr(raw, "://");
    if (!sep) {
        zen_set_error("metadata url has no scheme");
        return -1;
    }
    size_t scheme_len = (size_t)(sep - raw);
    if (scheme_len == 0 || scheme_len >= sizeof(u->scheme)) {
        zen_set_error("metadata url scheme is invalid");
        return -1;
    }
    for (size_t i = 0; i < scheme_len; i++) {
        u->scheme[i] = (char)tolower((unsigned char)raw[i]);
    }
    u->scheme[scheme_len] = '\0';
    if (strcmp(u->scheme, "https") == 0) {
        u->tls = 1;
    } else if (strcmp(u->scheme, "http") == 0) {
        u->tls = 0;
    } else {
        zen_set_error("unsupported url scheme \"%s\" (want http or https)", u->scheme);
        return -1;
    }

    const char *auth = sep + 3;
    size_t auth_len = strcspn(auth, "/?");
    if (auth_len == 0) {
        zen_set_error("metadata url has no host");
        return -1;
    }
    if (memchr(auth, '@', auth_len) != NULL) {
        zen_set_error("metadata url must not contain userinfo");
        return -1;
    }

    const char *host = auth;
    size_t host_len = 0;
    const char *port = NULL;
    size_t port_len = 0;
    if (auth[0] == '[') {
        const char *close = memchr(auth, ']', auth_len);
        if (!close) {
            zen_set_error("metadata url has an unterminated IPv6 literal");
            return -1;
        }
        host = auth + 1;
        host_len = (size_t)(close - host);
        u->is_ipv6 = 1;
        size_t after = (size_t)(close + 1 - auth);
        if (after < auth_len) {
            if (auth[after] != ':') {
                zen_set_error("metadata url has junk after the IPv6 literal");
                return -1;
            }
            port = auth + after + 1;
            port_len = auth_len - after - 1;
        }
        if (host_len == 0) {
            zen_set_error("metadata url has an empty host");
            return -1;
        }
    } else {
        const char *colon = memchr(auth, ':', auth_len);
        if (colon) {
            if (memchr(colon + 1, ':', auth_len - (size_t)(colon + 1 - auth))) {
                zen_set_error("metadata url has an unbracketed IPv6 host");
                return -1;
            }
            host = auth;
            host_len = (size_t)(colon - auth);
            port = colon + 1;
            port_len = auth_len - host_len - 1;
        } else {
            host = auth;
            host_len = auth_len;
        }
        if (host_len == 0) {
            zen_set_error("metadata url has an empty host");
            return -1;
        }
    }
    if (host_len >= sizeof(u->host)) {
        zen_set_error("metadata url host exceeds %d bytes", (int)sizeof(u->host) - 1);
        return -1;
    }
    memcpy(u->host, host, host_len);
    u->host[host_len] = '\0';
    if (has_bad_bytes(u->host, host_len)) {
        zen_set_error("metadata url host contains whitespace");
        return -1;
    }
    if (port && parse_port(port, port_len, u->port) != 0) {
        zen_set_error("metadata url has an invalid port");
        return -1;
    }

    const char *target = auth + auth_len;
    size_t target_len = strlen(target);
    if (target_len == 0) {
        snprintf(u->target, sizeof(u->target), "/");
        return 0;
    }
    if (target[0] == '?') {
        /* Browsers normalize a query-only URL to "/?query". */
        if (target_len + 2 > sizeof(u->target)) {
            zen_set_error("metadata url path exceeds %d bytes", (int)sizeof(u->target) - 1);
            return -1;
        }
        u->target[0] = '/';
        memcpy(u->target + 1, target, target_len + 1);
    } else {
        if (target_len + 1 > sizeof(u->target)) {
            zen_set_error("metadata url path exceeds %d bytes", (int)sizeof(u->target) - 1);
            return -1;
        }
        memcpy(u->target, target, target_len + 1);
    }
    if (!valid_target(u->target, strlen(u->target))) {
        zen_set_error("metadata url request target is not origin-form");
        return -1;
    }
    return 0;
}

static int default_port(const zurl *u, const char **out)
{
    *out = u->tls ? "443" : "80";
    return 0;
}

int zen_host_header(const zurl *u, char *out, size_t cap)
{
    const char *def = NULL;
    default_port(u, &def);
    int used;
    const char *p = u->port[0] ? u->port : def;
    int show_port = u->port[0] != '\0' && strcmp(u->port, def) != 0;
    if (u->is_ipv6) {
        used = snprintf(out, cap, "[%s]%s%s", u->host, show_port ? ":" : "",
                        show_port ? p : "");
    } else {
        used = snprintf(out, cap, "%s%s%s", u->host, show_port ? ":" : "",
                        show_port ? p : "");
    }
    if (used < 0 || (size_t)used >= cap) {
        zen_set_error("host header is too long");
        return -1;
    }
    return 0;
}

int zen_dial_address(const zurl *u, char *out, size_t cap)
{
    const char *def = NULL;
    default_port(u, &def);
    const char *p = u->port[0] ? u->port : def;
    int used;
    if (u->is_ipv6) {
        used = snprintf(out, cap, "[%s]:%s", u->host, p);
    } else {
        used = snprintf(out, cap, "%s:%s", u->host, p);
    }
    if (used < 0 || (size_t)used >= cap) {
        zen_set_error("dial address is too long");
        return -1;
    }
    return 0;
}

int zen_parse_hostport(const char *s, char *host, size_t host_cap, char *port,
                       size_t port_cap)
{
    size_t len = strlen(s);
    if (len == 0) {
        zen_set_error("empty host:port");
        return -1;
    }
    const char *h = s;
    size_t hlen;
    const char *p = NULL;
    size_t plen = 0;
    if (s[0] == '[') {
        const char *close = strchr(s, ']');
        if (!close) {
            zen_set_error("unterminated IPv6 literal in %s", s);
            return -1;
        }
        h = s + 1;
        hlen = (size_t)(close - h);
        if (close[1] == ':') {
            p = close + 2;
            plen = len - (size_t)(p - s);
        } else if (close[1] != '\0') {
            zen_set_error("junk after IPv6 literal in %s", s);
            return -1;
        }
    } else {
        const char *colon = strchr(s, ':');
        if (colon) {
            if (strchr(colon + 1, ':') != NULL) {
                zen_set_error("unbracketed IPv6 host in %s", s);
                return -1;
            }
            hlen = (size_t)(colon - s);
            p = colon + 1;
            plen = len - hlen - 1;
        } else {
            hlen = len;
        }
    }
    if (hlen == 0 || hlen >= host_cap) {
        zen_set_error("invalid host in %s", s);
        return -1;
    }
    memcpy(host, h, hlen);
    host[hlen] = '\0';
    port[0] = '\0';
    if (p) {
        char digits[8];
        if (parse_port(p, plen, digits) != 0) {
            zen_set_error("invalid port in %s", s);
            return -1;
        }
        if (strlen(digits) >= port_cap) {
            zen_set_error("port buffer too small");
            return -1;
        }
        strcpy(port, digits);
    }
    return 0;
}

int zen_is_loopback_host(const char *host)
{
    if (strcasecmp(host, "localhost") == 0) {
        return 1;
    }
    uint8_t buf[16];
    if (inet_pton(AF_INET, host, buf) == 1) {
        return buf[0] == 127;
    }
    if (inet_pton(AF_INET6, host, buf) == 1) {
        static const uint8_t lo[16] = { 0, 0, 0, 0, 0, 0, 0, 0,
                                        0, 0, 0, 0, 0, 0, 0, 1 };
        return memcmp(buf, lo, 16) == 0;
    }
    return 0;
}
