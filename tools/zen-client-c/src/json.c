/*
 * Minimal JSON reader/writer for the stdio protocol.
 *
 * Only the metadata object is parsed, and only string -> string maps are
 * accepted for headers. Unknown top-level keys are skipped exactly like
 * Go's json.Unmarshal into a struct, so PAIG can add metadata fields without
 * breaking older helpers.
 *
 * Strings are unescaped to UTF-8 (including \uXXXX surrogate pairs). Control
 * characters that arrive through escapes are rejected later by the header
 * validator, which is where a CR/LF injection becomes a hard error instead of
 * a wire split.
 */
#include "zen.h"

#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define JSON_MAX_DEPTH 32

typedef struct {
    const uint8_t *p;
    const uint8_t *end;
} jcur;

static int jstring(jcur *c, char **out);

static void jws(jcur *c)
{
    while (c->p < c->end) {
        uint8_t ch = *c->p;
        if (ch == ' ' || ch == '\t' || ch == '\n' || ch == '\r') {
            c->p++;
            continue;
        }
        break;
    }
}

static int jexpect(jcur *c, uint8_t want)
{
    if (c->p >= c->end || *c->p != want) {
        return -1;
    }
    c->p++;
    return 0;
}

static int jhex4(jcur *c, uint32_t *out)
{
    uint32_t v = 0;
    if ((size_t)(c->end - c->p) < 4) {
        return -1;
    }
    for (int i = 0; i < 4; i++) {
        uint8_t ch = c->p[i];
        v <<= 4;
        if (ch >= '0' && ch <= '9') {
            v |= (uint32_t)(ch - '0');
        } else if (ch >= 'a' && ch <= 'f') {
            v |= (uint32_t)(ch - 'a' + 10);
        } else if (ch >= 'A' && ch <= 'F') {
            v |= (uint32_t)(ch - 'A' + 10);
        } else {
            return -1;
        }
    }
    c->p += 4;
    *out = v;
    return 0;
}

static size_t utf8_put(uint32_t cp, uint8_t *out)
{
    if (cp < 0x80) {
        out[0] = (uint8_t)cp;
        return 1;
    }
    if (cp < 0x800) {
        out[0] = (uint8_t)(0xc0 | (cp >> 6));
        out[1] = (uint8_t)(0x80 | (cp & 0x3f));
        return 2;
    }
    if (cp < 0x10000) {
        out[0] = (uint8_t)(0xe0 | (cp >> 12));
        out[1] = (uint8_t)(0x80 | ((cp >> 6) & 0x3f));
        out[2] = (uint8_t)(0x80 | (cp & 0x3f));
        return 3;
    }
    out[0] = (uint8_t)(0xf0 | (cp >> 18));
    out[1] = (uint8_t)(0x80 | ((cp >> 12) & 0x3f));
    out[2] = (uint8_t)(0x80 | ((cp >> 6) & 0x3f));
    out[3] = (uint8_t)(0x80 | (cp & 0x3f));
    return 4;
}

/* Decodes one JSON string. The result is malloc'd and NUL-terminated. A raw
 * NUL is impossible because a 0x00 byte inside the quotes is a control char
 * and rejected. */
static int jstring(jcur *c, char **out)
{
    if (jexpect(c, '"') != 0) {
        return -1;
    }
    const uint8_t *start = c->p;
    /* Unescaping never grows the string, so the raw span bounds the result. */
    while (c->p < c->end && *c->p != '"') {
        if (*c->p == '\\') {
            c->p++;
        }
        if (c->p >= c->end) {
            return -1;
        }
        if (*c->p < 0x20) {
            return -1;
        }
        c->p++;
    }
    if (c->p >= c->end) {
        return -1;
    }
    const uint8_t *stop = c->p; /* closing quote; c->p must not move during
                                 * unescaping or the decoder loses its end */
    size_t raw_len = (size_t)(stop - start);
    char *buf = malloc(raw_len + 1);
    if (!buf) {
        zen_set_error("metadata json: out of memory");
        return -1;
    }
    size_t n = 0;
    jcur q = { start, stop };
    while (q.p < q.end) {
        uint8_t ch = *q.p++;
        if (ch != '\\') {
            buf[n++] = (char)ch;
            continue;
        }
        if (q.p >= q.end) {
            free(buf);
            return -1;
        }
        ch = *q.p++;
        switch (ch) {
        case '"': buf[n++] = '"'; break;
        case '\\': buf[n++] = '\\'; break;
        case '/': buf[n++] = '/'; break;
        case 'b': buf[n++] = '\b'; break;
        case 'f': buf[n++] = '\f'; break;
        case 'n': buf[n++] = '\n'; break;
        case 'r': buf[n++] = '\r'; break;
        case 't': buf[n++] = '\t'; break;
        case 'u': {
            uint32_t cp = 0;
            if (jhex4(&q, &cp) != 0) {
                free(buf);
                return -1;
            }
            if (cp >= 0xd800 && cp <= 0xdbff) {
                if (q.p + 1 >= q.end || q.p[0] != '\\' || q.p[1] != 'u') {
                    free(buf);
                    return -1;
                }
                q.p += 2;
                uint32_t lo = 0;
                if (jhex4(&q, &lo) != 0 || lo < 0xdc00 || lo > 0xdfff) {
                    free(buf);
                    return -1;
                }
                cp = 0x10000 + ((cp - 0xd800) << 10) + (lo - 0xdc00);
            } else if (cp >= 0xdc00 && cp <= 0xdfff) {
                free(buf);
                return -1;
            }
            n += utf8_put(cp, (uint8_t *)buf + n);
            break;
        }
        default:
            free(buf);
            return -1;
        }
    }
    c->p = stop + 1; /* closing quote */
    buf[n] = '\0';
    *out = buf;
    return 0;
}

static int jskip(jcur *c, int depth)
{
    if (depth > JSON_MAX_DEPTH) {
        return -1;
    }
    jws(c);
    if (c->p >= c->end) {
        return -1;
    }
    uint8_t ch = *c->p;
    if (ch == '"') {
        char *skip = NULL;
        int rc = jstring(c, &skip);
        free(skip);
        return rc;
    }
    if (ch == '{' || ch == '[') {
        uint8_t close = ch == '{' ? '}' : ']';
        c->p++;
        jws(c);
        if (c->p < c->end && *c->p == close) {
            c->p++;
            return 0;
        }
        for (;;) {
            jws(c);
            if (ch == '{') {
                char *key = NULL;
                if (jstring(c, &key) != 0) {
                    free(key);
                    return -1;
                }
                free(key);
                jws(c);
                if (jexpect(c, ':') != 0) {
                    return -1;
                }
            }
            if (jskip(c, depth + 1) != 0) {
                return -1;
            }
            jws(c);
            if (c->p < c->end && *c->p == ',') {
                c->p++;
                continue;
            }
            return jexpect(c, close);
        }
    }
    if (ch == 't') {
        if ((size_t)(c->end - c->p) < 4 || memcmp(c->p, "true", 4) != 0) {
            return -1;
        }
        c->p += 4;
        return 0;
    }
    if (ch == 'f') {
        if ((size_t)(c->end - c->p) < 5 || memcmp(c->p, "false", 5) != 0) {
            return -1;
        }
        c->p += 5;
        return 0;
    }
    if (ch == 'n') {
        if ((size_t)(c->end - c->p) < 4 || memcmp(c->p, "null", 4) != 0) {
            return -1;
        }
        c->p += 4;
        return 0;
    }
    /* number */
    const uint8_t *start = c->p;
    if (c->p < c->end && (*c->p == '-' || *c->p == '+')) {
        c->p++;
    }
    while (c->p < c->end &&
           ((*c->p >= '0' && *c->p <= '9') || *c->p == '.' || *c->p == 'e' ||
            *c->p == 'E' || *c->p == '+' || *c->p == '-')) {
        c->p++;
    }
    return c->p == start ? -1 : 0;
}

/* Strict integer: an optional '-' and decimal digits, nothing else. Go's
 * json.Unmarshal into int64 rejects a float, so "180000.0" must fail. */
static int jint(jcur *c, int64_t *out)
{
    int neg = 0;
    if (c->p < c->end && *c->p == '-') {
        neg = 1;
        c->p++;
    }
    if (c->p >= c->end || *c->p < '0' || *c->p > '9') {
        return -1;
    }
    int64_t v = 0;
    while (c->p < c->end && *c->p >= '0' && *c->p <= '9') {
        int digit = *c->p - '0';
        if (v > (INT64_MAX - digit) / 10) {
            return -1;
        }
        v = v * 10 + digit;
        c->p++;
    }
    if (c->p < c->end && (*c->p == '.' || *c->p == 'e' || *c->p == 'E')) {
        return -1;
    }
    *out = neg ? -v : v;
    return 0;
}

static int jheaders(jcur *c, zmeta *m)
{
    if (jexpect(c, '{') != 0) {
        return -1;
    }
    jws(c);
    if (c->p < c->end && *c->p == '}') {
        c->p++;
        return 0;
    }
    for (;;) {
        char *name = NULL;
        char *value = NULL;
        if (jstring(c, &name) != 0) {
            free(name);
            return -1;
        }
        jws(c);
        if (jexpect(c, ':') != 0) {
            free(name);
            return -1;
        }
        jws(c);
        if (jstring(c, &value) != 0) {
            free(name);
            return -1;
        }
        int slot = -1;
        for (int i = 0; i < m->nheaders; i++) {
            if (strcmp(m->headers[i].name, name) == 0) {
                slot = i; /* duplicate key: last wins, like a JSON map */
                break;
            }
        }
        if (slot >= 0) {
            free(m->headers[slot].value);
            m->headers[slot].value = value;
            free(name);
        } else if (m->nheaders < ZEN_MAX_HEADERS) {
            m->headers[m->nheaders].name = name;
            m->headers[m->nheaders].value = value;
            m->nheaders++;
        } else {
            free(name);
            free(value);
            zen_set_error("metadata headers: too many headers (max %d)",
                          ZEN_MAX_HEADERS);
            return -1;
        }
        jws(c);
        if (c->p < c->end && *c->p == ',') {
            c->p++;
            continue;
        }
        return jexpect(c, '}');
    }
}

int zen_parse_metadata(const uint8_t *line, size_t len, zmeta *m)
{
    memset(m, 0, sizeof(*m));
    m->timeout_ms = 0;
    jcur c = { line, line + len };
    jws(&c);
    if (jexpect(&c, '{') != 0) {
        zen_set_error("metadata json: expected an object");
        return -1;
    }
    jws(&c);
    if (c.p < c.end && *c.p == '}') {
        c.p++;
    } else {
        for (;;) {
            jws(&c);
            char *key = NULL;
            if (jstring(&c, &key) != 0) {
                free(key);
                zen_set_error("metadata json: malformed key");
                return -1;
            }
            jws(&c);
            if (jexpect(&c, ':') != 0) {
                free(key);
                zen_set_error("metadata json: expected ':' after %s", key ? key : "?");
                return -1;
            }
            jws(&c);
            int rc = 0;
            if (strcmp(key, "url") == 0) {
                char *url = NULL;
                rc = jstring(&c, &url);
                if (rc == 0) {
                    free(m->url);
                    m->url = url;
                } else {
                    free(url);
                }
            } else if (strcmp(key, "headers") == 0) {
                rc = jheaders(&c, m);
            } else if (strcmp(key, "timeout_ms") == 0) {
                int64_t v = 0;
                rc = jint(&c, &v);
                m->timeout_ms = v;
            } else {
                rc = jskip(&c, 1);
            }
            free(key);
            if (rc != 0) {
                zen_set_error("metadata json: malformed value");
                return -1;
            }
            jws(&c);
            if (c.p < c.end && *c.p == ',') {
                c.p++;
                continue;
            }
            if (jexpect(&c, '}') != 0) {
                zen_set_error("metadata json: expected ',' or '}'");
                return -1;
            }
            break;
        }
    }
    jws(&c);
    if (c.p != c.end) {
        zen_set_error("metadata json: trailing data after the object");
        return -1;
    }
    return 0;
}

void zen_meta_free(zmeta *m)
{
    free(m->url);
    m->url = NULL;
    for (int i = 0; i < m->nheaders; i++) {
        free(m->headers[i].name);
        free(m->headers[i].value);
    }
    m->nheaders = 0;
}

int zen_meta_timeout_ms(const zmeta *m)
{
    if (m->timeout_ms <= 0) {
        return ZEN_DEFAULT_TIMEOUT_MS;
    }
    if (m->timeout_ms > ZEN_MAX_TIMEOUT_MS) {
        return ZEN_MAX_TIMEOUT_MS;
    }
    return (int)m->timeout_ms;
}
