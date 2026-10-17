// PID 1 in the inner Linux VM. Its disk and RAM must survive outer-VM restore.
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <stdint.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/mman.h>
#include <sys/random.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

struct record {
    uint64_t identity;
    uint64_t generation;
    uint64_t check;
};

static void fail(const char *operation) {
    perror(operation);
    exit(1);
}

static void mount_fs(const char *source, const char *target, const char *type) {
    if (mkdir(target, 0755) && errno != EEXIST) fail("mkdir");
    if (mount(source, target, type, 0, NULL) && errno != EBUSY) fail(target);
}

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    mount_fs("devtmpfs", "/dev", "devtmpfs");
    mount_fs("proc", "/proc", "proc");
    mount_fs("/dev/vda", "/data", "ext4");

    uint64_t identity;
    if (getrandom(&identity, sizeof(identity), 0) != sizeof(identity)) fail("getrandom");
    // A reboot against the old disk must fail instead of masquerading as restore.
    int fd = open("/data/progress", O_RDWR | O_CREAT | O_EXCL, 0600);
    if (fd < 0) fail("create progress");
    uint64_t generation = 0;
    struct record saved = {identity, generation, identity ^ generation};
    if (pwrite(fd, &saved, sizeof(saved), 0) != sizeof(saved) || fsync(fd)) fail("initialize disk");
    int dir = open("/data", O_RDONLY | O_DIRECTORY);
    if (dir < 0 || fsync(dir)) fail("sync directory");
    close(dir);

    // Optional load profile keeps both inner CPUs busy while the outer VM is
    // captured. Disk/timer/identity checks below remain the pass criteria.
    _Atomic uint64_t *busy = NULL;
    if (getenv("NESTED_BUSY") && !strcmp(getenv("NESTED_BUSY"), "1")) {
        busy = mmap(NULL, 2 * sizeof(*busy), PROT_READ | PROT_WRITE,
                    MAP_SHARED | MAP_ANONYMOUS, -1, 0);
        if (busy == MAP_FAILED) fail("map busy counters");
        atomic_init(&busy[0], 0);
        atomic_init(&busy[1], 0);
        for (unsigned i = 0; i < 2; ++i) {
            pid_t child = fork();
            if (child < 0) fail("fork busy worker");
            if (child == 0) {
                for (;;) atomic_fetch_add_explicit(&busy[i], 1, memory_order_relaxed);
            }
        }
        puts("NESTED_BUSY workers=2");
    }

    for (;;) {
        // Verify restored disk contents before overwriting them with the next record.
        int error = posix_fadvise(fd, 0, 0, POSIX_FADV_DONTNEED);
        if (error) { errno = error; fail("evict restored file cache"); }
        struct record actual;
        if (pread(fd, &actual, sizeof(actual), 0) != sizeof(actual)) fail("read disk");
        if (memcmp(&actual, &saved, sizeof(saved))) {
            fprintf(stderr, "FAIL: disk record disagrees with running guest memory\n");
            return 1;
        }
        saved.generation = ++generation;
        saved.check = identity ^ generation;
        if (pwrite(fd, &saved, sizeof(saved), 0) != sizeof(saved) || fsync(fd)) fail("commit disk");
        // Evict clean file pages so verification also exercises the virtual block device.
        error = posix_fadvise(fd, 0, 0, POSIX_FADV_DONTNEED);
        if (error) { errno = error; fail("evict file cache"); }
        // Read committed data again; errors must stop progress before the host accepts it.
        if (pread(fd, &actual, sizeof(actual), 0) != sizeof(actual) || memcmp(&actual, &saved, sizeof(saved))) {
            fprintf(stderr, "FAIL: committed disk record could not be read back\n");
            return 1;
        }
        struct timespec now;
        if (clock_gettime(CLOCK_MONOTONIC, &now)) fail("clock_gettime");
        uint32_t elapsed_ms = (uint32_t)((uint64_t)now.tv_sec * 1000 + now.tv_nsec / 1000000);
        if (busy) {
            printf("NESTED_BUSY_PROGRESS %" PRIu64 " %" PRIu64 "\n",
                   atomic_load_explicit(&busy[0], memory_order_relaxed),
                   atomic_load_explicit(&busy[1], memory_order_relaxed));
        }
        printf("NESTED_PROGRESS %" PRIu32 " %" PRIu32 " %" PRIu64 " disk=%" PRIu64 "\n",
               (uint32_t)generation, elapsed_ms, identity, actual.generation);
        struct timespec delay = {.tv_nsec = 100000000};
        while (nanosleep(&delay, &delay)) if (errno != EINTR) fail("nanosleep");
    }
}
