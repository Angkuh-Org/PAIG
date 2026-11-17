/*
 * In-process entry point for the OpenCode free transport.
 *
 * The standalone helper and PAIG's gateway both run the same shim body. The
 * helper maps its stdio to (STDIN_FILENO, STDOUT_FILENO); the gateway passes
 * one end of a socketpair so the request and response never cross a process
 * boundary.
 */
#ifndef ZEN_SHIM_H
#define ZEN_SHIM_H

typedef struct {
    int insecure;
    int cancel_fd;
    const char *sni;
    const char *dial;
} zen_shim_opts;

#ifdef PAIG_ZEN_EXTERNAL_CA
/* Set by paig_zen_set_ca once the embedded PEM has been parsed. */
extern const void *zen_ca_chain;
#endif

/* in_fd carries one JSON metadata line followed by the raw request body;
 * out_fd receives one JSON status line followed by the decoded body. The
 * same fd is valid for both directions when it is a socketpair. Returns 0 on
 * success, non-zero after writing a status-0 error line. */
int zen_shim_run(int in_fd, int out_fd, const zen_shim_opts *opts);

#endif /* ZEN_SHIM_H */
