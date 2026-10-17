// Linux KVM parent for the ARM64 nested counter and virtual timer workload.
#include <errno.h>
#include <fcntl.h>
#include <linux/kvm.h>
#include <pthread.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/klog.h>
#include <sys/mman.h>
#include <sys/mount.h>
#include <sys/random.h>
#include <time.h>
#include <unistd.h>

extern const unsigned char inner_start[], inner_end[];
static int cpu;
static struct kvm_run *run;

static void fail(const char *s)
{
    perror(s);
    exit(1);
}

static void set_reg(uint64_t id, uint64_t value)
{
    struct kvm_one_reg reg = {.id = id, .addr = (uintptr_t)&value};
    if (ioctl(cpu, KVM_SET_ONE_REG, &reg) < 0)
        fail("SET_ONE_REG");
}

#define CORE_REG(member)                                                       \
    (KVM_REG_ARM64 | KVM_REG_SIZE_U64 | KVM_REG_ARM_CORE |                     \
     (offsetof(struct kvm_regs, member) / sizeof(uint32_t)))
static void attr(int fd, uint32_t group, uint64_t attr, uint64_t *value)
{
    struct kvm_device_attr a = {
        .group = group, .attr = attr, .addr = (uintptr_t)value};
    if (ioctl(fd, KVM_SET_DEVICE_ATTR, &a) < 0)
        fail("SET_DEVICE_ATTR");
}

static void *execute(void *unused)
{
    (void)unused;
    for (;;) {
        if (ioctl(cpu, KVM_RUN, 0) < 0) {
            if (errno == EINTR)
                continue;
            fail("KVM_RUN");
        }

        if (run->exit_reason != KVM_EXIT_INTR) {
            fprintf(stderr, "unexpected inner exit %u\n", run->exit_reason);
            if (run->exit_reason == KVM_EXIT_INTERNAL_ERROR) {
                fprintf(stderr, "internal suberror=%u ndata=%u",
                        run->internal.suberror, run->internal.ndata);
                for (unsigned i = 0; i < run->internal.ndata && i < 16; ++i)
                    fprintf(stderr, " %llx",
                            (unsigned long long)run->internal.data[i]);
                fputc('\n', stderr);
            }
            char messages[16384];
            int length = klogctl(3, messages, sizeof(messages));
            if (length > 0)
                fwrite(messages, 1, length, stderr);
            exit(1);
        }
    }
}

int main(void)
{
    uint64_t identity;
    if (getrandom(&identity, sizeof(identity), 0) != sizeof(identity))
        fail("getrandom");
    if (mount("devtmpfs", "/dev", "devtmpfs", 0, NULL) < 0 && errno != EBUSY)
        fail("mount");

    int kvm = open("/dev/kvm", O_RDWR | O_CLOEXEC);
    if (kvm < 0)
        fail("open /dev/kvm");
    int vm = ioctl(kvm, KVM_CREATE_VM, 0);
    if (vm < 0)
        fail("CREATE_VM");

    unsigned char *memory = mmap(NULL, 0x10000, PROT_READ | PROT_WRITE,
                                 MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (memory == MAP_FAILED)
        fail("mmap");
    memcpy(memory, inner_start, inner_end - inner_start);
    struct kvm_userspace_memory_region region = {.slot = 0,
                                                 .guest_phys_addr = 0x40000000,
                                                 .memory_size = 0x10000,
                                                 .userspace_addr =
                                                     (uintptr_t)memory};
    if (ioctl(vm, KVM_SET_USER_MEMORY_REGION, &region) < 0)
        fail("SET_MEMORY");

    cpu = ioctl(vm, KVM_CREATE_VCPU, 0);
    if (cpu < 0)
        fail("CREATE_VCPU");
    struct kvm_vcpu_init init = {0};
    if (ioctl(vm, KVM_ARM_PREFERRED_TARGET, &init) < 0)
        fail("PREFERRED_TARGET");
    if (ioctl(cpu, KVM_ARM_VCPU_INIT, &init) < 0)
        fail("VCPU_INIT");

    struct kvm_create_device device = {.type = KVM_DEV_TYPE_ARM_VGIC_V3};
    if (ioctl(vm, KVM_CREATE_DEVICE, &device) < 0)
        fail("CREATE_VGIC");
    uint64_t dist = 0x08000000, redist = 0x080a0000;
    attr(device.fd, KVM_DEV_ARM_VGIC_GRP_ADDR, KVM_VGIC_V3_ADDR_TYPE_DIST,
         &dist);
    attr(device.fd, KVM_DEV_ARM_VGIC_GRP_ADDR, KVM_VGIC_V3_ADDR_TYPE_REDIST,
         &redist);
    attr(device.fd, KVM_DEV_ARM_VGIC_GRP_CTRL, KVM_DEV_ARM_VGIC_CTRL_INIT,
         NULL);

    set_reg(CORE_REG(regs.pc), 0x40000000);
    set_reg(CORE_REG(regs.pstate), 0x3c5);
    set_reg(CORE_REG(sp_el1), 0x4000f000);

    int size = ioctl(kvm, KVM_GET_VCPU_MMAP_SIZE, 0);
    if (size < 0)
        fail("GET_VCPU_MMAP_SIZE");
    run = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_SHARED, cpu, 0);
    if (run == MAP_FAILED)
        fail("mmap run");

    pthread_t thread;
    if (pthread_create(&thread, NULL, execute, NULL))
        fail("pthread_create");

    volatile uint32_t *counter = (volatile uint32_t *)(memory + 0x4000);
    for (;;) {
        struct timespec now;
        if (clock_gettime(CLOCK_MONOTONIC, &now) < 0)
            fail("clock_gettime");
        printf("NESTED_PROGRESS %u %u %llu fatal=%u monotonic_ns=%llu\n",
               counter[0], counter[1], (unsigned long long)identity, counter[2],
               (unsigned long long)now.tv_sec * 1000000000ULL + now.tv_nsec);
        fflush(stdout);
        usleep(100000);
    }
}
