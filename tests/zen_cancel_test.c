/*
 * The in-process gateway must be able to wake the C transport when the
 * downstream client disappears. This is the narrowest check: zen_wait_fd()
 * polls the cancel socketpair alongside the upstream fd and returns with
 * ECANCELED instead of waiting for the full timeout.
 */
#include "zen.h"

#include <errno.h>
#include <pthread.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

static int cancel_write_fd;

static void *cancel_later(void *arg)
{
    (void)arg;
    usleep(100 * 1000);
    char byte = 'x';
    ssize_t written = write(cancel_write_fd, &byte, 1);
    (void)written;
    return NULL;
}

int main(void)
{
    int idle[2];
    int cancel[2];
    if (pipe(idle) != 0 || pipe(cancel) != 0) {
        perror("pipe");
        return 1;
    }
    cancel_write_fd = cancel[1];
    zen_set_cancel_fd(cancel[0]);

    pthread_t thread;
    if (pthread_create(&thread, NULL, cancel_later, NULL) != 0) {
        fprintf(stderr, "pthread_create failed\n");
        return 1;
    }
    int rc = zen_wait_fd(idle[0], 0, 5000);
    int saved = errno;
    pthread_join(thread, NULL);

    if (rc != -1 || saved != ECANCELED
        || strstr(zen_error(), "cancelled") == NULL) {
        fprintf(stderr, "cancel not observed: rc=%d errno=%d error=%s\n",
                rc, saved, zen_error());
        return 1;
    }
    puts("zen_cancel_test: ok");
    return 0;
}
