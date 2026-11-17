/* Regression test for the "TLS read wants more data" stream aborts.
 *
 * Include the shim source so the static BIO callbacks are directly callable.
 * A blocking recv() interrupted by a signal must be retried inside bio_recv;
 * returning MBEDTLS_ERR_SSL_WANT_READ here used to reach the Rust wrapper,
 * which aborted the stream after three interruptions. */
#define _GNU_SOURCE
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

#include "../src/transport/tls_shim.c"

static volatile sig_atomic_t handled;
static volatile int storm_on;

static void on_signal(int sig)
{
    (void) sig;
    handled++;
}

static void *storm(void *arg)
{
    pthread_t target = *(pthread_t *) arg;
    while (storm_on) {
        pthread_kill(target, SIGUSR1);
        usleep(2000);
    }
    return NULL;
}

static void *writer(void *arg)
{
    int fd = *(int *) arg;
    usleep(300000);
    if (write(fd, "okay", 4) != 4) {
        perror("write");
    }
    return NULL;
}

int main(void)
{
    struct sigaction sa;
    pthread_t storm_tid, writer_tid, target;
    PaigTls *tls;
    unsigned char buf[4];
    int sv[2];
    int rc;

    memset(&sa, 0, sizeof sa);
    sa.sa_handler = on_signal;
    sigemptyset(&sa.sa_mask);
    if (sigaction(SIGUSR1, &sa, NULL) != 0) {
        perror("sigaction");
        return 1;
    }
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, sv) != 0) {
        perror("socketpair");
        return 1;
    }

    tls = calloc(1, sizeof *tls);
    if (tls == NULL) {
        perror("calloc");
        return 1;
    }
    tls->fd = sv[0];

    if (pthread_create(&writer_tid, NULL, writer, &sv[1]) != 0) {
        perror("pthread_create writer");
        return 1;
    }
    target = pthread_self();
    storm_on = 1;
    if (pthread_create(&storm_tid, NULL, storm, &target) != 0) {
        perror("pthread_create storm");
        return 1;
    }

    rc = bio_recv(tls, buf, sizeof buf);

    storm_on = 0;
    pthread_join(storm_tid, NULL);
    pthread_join(writer_tid, NULL);

    if (rc != 4) {
        fprintf(stderr,
                "bio_recv returned %d (WANT_READ=%d), expected 4\n",
                rc, MBEDTLS_ERR_SSL_WANT_READ);
        return 1;
    }
    if (memcmp(buf, "okay", 4) != 0) {
        fprintf(stderr, "bio_recv payload mismatch\n");
        return 1;
    }
    if (handled == 0) {
        fprintf(stderr, "no SIGUSR1 handled; the test did not interrupt recv\n");
        return 1;
    }
    printf("ok: bio_recv absorbed %d interruptions\n", (int) handled);
    free(tls);
    close(sv[0]);
    close(sv[1]);
    return 0;
}
