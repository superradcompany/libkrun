#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <limits.h>
#include <linux/kvm.h>
#include <stdarg.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/ioctl.h>

/* Source-only x86-64 test adapter. The restore process must not preload it. */
int ioctl(int fd, unsigned long request, ...) {
    typedef int (*ioctl_fn)(int, unsigned long, ...);
    static _Atomic(ioctl_fn) resolved;
    ioctl_fn real_ioctl = atomic_load(&resolved);
    if (!real_ioctl) {
        real_ioctl = dlsym(RTLD_NEXT, "ioctl");
        if (!real_ioctl) abort();
        atomic_store(&resolved, real_ioctl);
    }
    va_list args;
    va_start(args, request);
    unsigned long arg = va_arg(args, unsigned long);
    va_end(args);

    int ret = real_ioctl(fd, request, arg);
    const char *value = getenv("TEST_SOURCE_TSC_KHZ");
    if (request == KVM_CREATE_VCPU && ret >= 0 && value) {
        char *end;
        errno = 0;
        unsigned long khz = strtoul(value, &end, 10);
        if (errno || end == value || *end || !khz || khz > INT_MAX) abort();
        if (real_ioctl(fd, KVM_CHECK_EXTENSION, KVM_CAP_TSC_CONTROL) <= 0) {
            fprintf(stderr, "source host lacks hardware TSC scaling\n");
            abort();
        }
        if (real_ioctl(ret, KVM_SET_TSC_KHZ, khz) < 0) {
            perror("source TSC setup");
            abort();
        }
        int actual = real_ioctl(ret, KVM_GET_TSC_KHZ, 0);
        fprintf(stderr, "source vCPU clock: %d kHz\n", actual);
        if (actual != (int)khz) abort();
    }
    return ret;
}
