// pidio.c
// Build: clang -O2 -Wall -Wextra -o pidio pidio.c
// Usage: ./pidio <pid>
// Output: "<bytes_read> <bytes_written>\n"

#include <stdio.h>
#include <stdlib.h>
#include <libproc.h>
#include <sys/resource.h>
#include <string.h>
#include <errno.h>

int main(int argc, char *argv[]) {
    if (argc != 2) {
        fprintf(stderr, "Usage: %s <pid>\n", argv[0]);
        return 2;
    }
    pid_t pid = (pid_t)strtol(argv[1], NULL, 10);

    // Use the widest available struct the OS supports. v4 is available on modern macOS,
    // but v2 already has disk I/O fields. We will try v4, then v2 for compatibility.
    struct rusage_info_v4 ri4;
    int rc = proc_pid_rusage(pid, RUSAGE_INFO_V4, (rusage_info_t *)&ri4);
    if (rc == 0) {
        unsigned long long r = ri4.ri_diskio_bytesread;
        unsigned long long w = ri4.ri_diskio_byteswritten;
        printf("%llu %llu\n", r, w);
        return 0;
    }

    struct rusage_info_v2 ri2;
    rc = proc_pid_rusage(pid, RUSAGE_INFO_V2, (rusage_info_t *)&ri2);
    if (rc == 0) {
        unsigned long long r = ri2.ri_diskio_bytesread;
        unsigned long long w = ri2.ri_diskio_byteswritten;
        printf("%llu %llu\n", r, w);
        return 0;
    }

    // If both calls failed, print a friendly error
    fprintf(stderr, "proc_pid_rusage failed for pid %d: %s\n", pid, strerror(errno));
    return 1;
}

