/*
 * Test-only parity harness: build the exact Bun 1.4.4 ClientHello record and
 * write it to stdout. It links the same libzen.a as paig but contains no
 * HTTP/shim/CLI code and is never installed or released.
 */
#include "zen.h"

#include <stdio.h>

int main(void)
{
    struct ztls *t = ztls_new(0, "opencode.ai", 1, NULL, 0, 5000);
    if (t == NULL) {
        fprintf(stderr, "ztls_new: %s\n", zen_error());
        return 1;
    }
    size_t len = 0;
    const uint8_t *record = ztls_hello_record(t, &len);
    if (record == NULL || len == 0) {
        fprintf(stderr, "empty ClientHello record\n");
        ztls_free(t);
        return 1;
    }
    if (fwrite(record, 1, len, stdout) != len) {
        perror("fwrite");
        ztls_free(t);
        return 1;
    }
    ztls_free(t);
    return 0;
}
