/*
 * Byte buffer, per-thread error string, monotonic clock, and the
 * timeout/EINTR-safe socket helpers.
 *
 * The standalone helper is one request per process, but PAIG also compiles
 * this module into the gateway where several free requests run on different
 * worker threads. The error buffer is therefore thread-local: a failed
 * handshake on one request can never overwrite the message another thread is
 * about to report. zen_set_error() still formats through a stack temporary
 * before touching that buffer: http.c composes messages as
 * `zen_set_error("status line: %s", zen_error())`, and vsnprintf must never
 * read and write the same object.
 *
 * Every wait loop carries a remaining budget instead of restarting the full
 * timeout. The 4.9.337-LanceX kernel this project runs on hands back
 * spurious EINTR from long-blocking recv; without the budget a hostile peer
 * could extend a 180 s deadline forever with one signal per second (or spin
 * forever when timeout_ms <= 0).
 */
#include "zen.h"

#include <errno.h>
#include <limits.h>
#include <poll.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <time.h>
#include <unistd.h>

#ifndef MSG_NOSIGNAL
#define MSG_NOSIGNAL 0
#endif
#ifndef MSG_DONTWAIT
#define MSG_DONTWAIT 0
#endif

/* ---------------------------- byte buffer ---------------------------- */

/* Grows geometrically, refuses length wraparound, and leaves the buffer
 * untouched when it cannot grow. A zeroed zbuf is a valid empty buffer. */
static int zbuf_reserve(zbuf *b, size_t add)
{
    if (add == 0) {
        return 0;
    }
    if (b->len > SIZE_MAX - add) {
        zen_set_error("buffer length overflow (%zu + %zu)", b->len, add);
        return -1;
    }
    size_t need = b->len + add;
    if (need <= b->cap) {
        return 0;
    }
    size_t cap = b->cap ? b->cap : 256;
    while (cap < need) {
        if (cap > SIZE_MAX / 2) {
            cap = need; /* one last oversized allocation, then fail cleanly */
            break;
        }
        cap *= 2;
    }
    uint8_t *grown = realloc(b->p, cap);
    if (!grown) {
        zen_set_error("out of memory: cannot grow a buffer to %zu bytes", cap);
        return -1;
    }
    b->p = grown;
    b->cap = cap;
    return 0;
}

int zbuf_add(zbuf *b, const void *data, size_t len)
{
    if (len == 0) {
        return 0;
    }
    if (!data) {
        zen_set_error("buffer append with a NULL source");
        return -1;
    }
    if (zbuf_reserve(b, len) != 0) {
        return -1;
    }
    memcpy(b->p + b->len, data, len);
    b->len += len;
    return 0;
}

int zbuf_addstr(zbuf *b, const char *s)
{
    if (!s) {
        return 0;
    }
    return zbuf_add(b, s, strlen(s));
}

void zbuf_free(zbuf *b)
{
    free(b->p);
    b->p = NULL;
    b->len = 0;
    b->cap = 0;
}

/* ------------------------------ errors ------------------------------- */

static _Thread_local char zen_err[ZEN_MAX_ERR];

void zen_set_error(const char *fmt, ...)
{
    char tmp[ZEN_MAX_ERR];
    va_list ap;
    va_start(ap, fmt);
    int n = vsnprintf(tmp, sizeof(tmp), fmt, ap);
    va_end(ap);
    if (n < 0) {
        memcpy(zen_err, "unknown error", sizeof("unknown error"));
        return;
    }
    size_t len = (size_t)n < sizeof(zen_err) ? (size_t)n : sizeof(zen_err) - 1;
    memcpy(zen_err, tmp, len);
    zen_err[len] = '\0';
}

const char *zen_error(void)
{
    return zen_err;
}

void zen_clear_error(void)
{
    zen_err[0] = '\0';
}

/* Set per request by the in-process shim; -1 keeps the standalone helper's
 * single-fd behaviour. */
static _Thread_local int zen_cancel_fd = -1;

void zen_set_cancel_fd(int fd)
{
    zen_cancel_fd = fd;
}

/* ------------------------------ timing ------------------------------- */

int64_t zen_now_ms(void)
{
    struct timespec ts;
    if (clock_gettime(CLOCK_MONOTONIC, &ts) != 0) {
        return 0; /* only a clock bug can land here; budgets then run hot */
    }
    return (int64_t)ts.tv_sec * 1000 + (int64_t)(ts.tv_nsec / 1000000);
}

/* Milliseconds left before deadline, -1 for "wait forever", 0 for expired. */
static int wait_budget(int64_t deadline, int timeout_ms)
{
    if (timeout_ms <= 0) {
        return -1;
    }
    int64_t left = deadline - zen_now_ms();
    if (left <= 0) {
        return 0;
    }
    return left > INT_MAX ? INT_MAX : (int)left;
}

int zen_wait_fd(int fd, int for_write, int timeout_ms)
{
    int64_t deadline = 0;
    if (timeout_ms > 0) {
        deadline = zen_now_ms() + timeout_ms;
    }
    for (;;) {
        int wait = wait_budget(deadline, timeout_ms);
        if (wait == 0) {
            errno = ETIMEDOUT;
            return -1;
        }
        struct pollfd pfd[2];
        pfd[0].fd = fd;
        pfd[0].events = (short)(for_write ? POLLOUT : POLLIN);
        pfd[0].revents = 0;
        nfds_t nfds = 1;
        if (zen_cancel_fd >= 0) {
            pfd[1].fd = zen_cancel_fd;
            pfd[1].events = POLLIN;
            pfd[1].revents = 0;
            nfds = 2;
        }
        int rc = poll(pfd, nfds, wait);
        if (rc > 0) {
            if (nfds == 2
                && (pfd[1].revents & (POLLIN | POLLHUP | POLLERR | POLLNVAL))) {
                zen_set_error("request cancelled");
                errno = ECANCELED;
                return -1;
            }
            return 0; /* POLLERR/HUP count as ready; the syscall reports why */
        }
        if (rc == 0) {
            errno = ETIMEDOUT;
            return -1;
        }
        if (errno != EINTR) {
            return -1;
        }
        /* Spurious EINTR: restart with what is left of the budget. */
    }
}

ssize_t zen_recv_timeout(int fd, void *buf, size_t len, int timeout_ms)
{
    if (len == 0) {
        return 0;
    }
    int64_t deadline = 0;
    if (timeout_ms > 0) {
        deadline = zen_now_ms() + timeout_ms;
    }
    for (;;) {
        int wait = wait_budget(deadline, timeout_ms);
        if (wait == 0) {
            errno = ETIMEDOUT;
            return -1;
        }
        if (zen_wait_fd(fd, 0, wait) != 0) {
            return -1;
        }
        /* MSG_DONTWAIT keeps a ready-but-empty socket from blocking past the
         * deadline; the poll above is the only place we sleep. */
        ssize_t n = recv(fd, buf, len, MSG_DONTWAIT);
        if (n >= 0) {
            return n;
        }
        if (errno == EINTR || errno == EAGAIN || errno == EWOULDBLOCK) {
            continue;
        }
        return -1;
    }
}

int zen_send_all(int fd, const void *buf, size_t len, int timeout_ms)
{
    const uint8_t *p = buf;
    size_t off = 0;
    int64_t deadline = 0;
    if (timeout_ms > 0) {
        deadline = zen_now_ms() + timeout_ms;
    }
    while (off < len) {
        ssize_t n = send(fd, p + off, len - off, MSG_NOSIGNAL | MSG_DONTWAIT);
        if (n > 0) {
            off += (size_t)n;
            continue;
        }
        if (n == 0) {
            errno = EPIPE;
            return -1;
        }
        if (errno == EINTR) {
            continue;
        }
        if (errno != EAGAIN && errno != EWOULDBLOCK) {
            return -1;
        }
        int wait = wait_budget(deadline, timeout_ms);
        if (wait == 0) {
            errno = ETIMEDOUT;
            return -1;
        }
        if (zen_wait_fd(fd, 1, wait) != 0) {
            return -1;
        }
    }
    return 0;
}

int zen_write_all(int fd, const void *buf, size_t len)
{
    const uint8_t *p = buf;
    size_t off = 0;
    while (off < len) {
        ssize_t n = write(fd, p + off, len - off);
        if (n > 0) {
            off += (size_t)n;
            continue;
        }
        if (n < 0 && errno == EINTR) {
            continue;
        }
        if (n == 0) {
            errno = EIO;
        }
        return -1;
    }
    return 0;
}

/* ------------------------------- util -------------------------------- */

/* zen_parse_hostport() lives in url.c with the rest of the URL grammar. */

/* Declared for diagnostics, unused by the shim. zen.h gives no semantics for
 * the "cc" it resolves, so this build refuses instead of inventing one. */
int zen_resolve_cc(const char *name)
{
    (void)name;
    zen_set_error("zen_resolve_cc is a diagnostic placeholder and resolves nothing");
    return -1;
}
