# Live nested VM restore probe

This manual harness boots a Linux parent VM whose KVM child continuously updates
a counter and services timer interrupts. Capture and restore run in separate host
processes. Restore requires the original workload identity and continued timer
and counter progress, so rebooting the workload cannot pass.

The production build admits nested capture and restore on qualified Linux x86-64,
Windows x86-64, and Apple Silicon macOS hosts. Linux ARM64 and Windows ARM64
nesting are unsupported. Run this harness without an admission bypass. The dated
results below describe earlier diagnostic builds; see `docs/nested-snapshots.md`
for the current platform policy and qualification scope.

Build `guest.c` as a static Linux x86 executable and place it at `/init` in an
initramfs containing `/dev/console` (character device 5:1) and `/dev`. The parent
kernel needs KVM, VMX/SVM, IRQ chip and PIT support. On Linux x86 the existing
firmware mmap boot path does not load an external initramfs, so embed it in the
test kernel with `CONFIG_INITRAMFS_SOURCE`. Windows loads `TEST_INITRAMFS` through
the kernel-copy path.

```sh
cargo build --manifest-path tests/nested-restore/Cargo.toml
export KRUNFW_PATH=/absolute/path/to/libkrunfw.so
export TEST_INITRAMFS=/absolute/path/to/initramfs.cpio
export TEST_ROOT=/absolute/path/to/empty-output-directory
export TEST_SNAPSHOT=/absolute/path/to/snapshot-directory
tests/nested-restore/target/debug/nested-restore-live capture
tests/nested-restore/target/debug/nested-restore-live restore
```

On Linux x86_64, set `TEST_SPLIT_IRQCHIP=true` to use the outer VM's split
interrupt controller, matching Microsandbox's runtime configuration. The default
is `false`, preserving the fully in-kernel controller as a separate test profile.
Use the same value for capture and restore. The harness prints the selected
profile and rejects this option on other host platforms. This controls libkrun's
outer VM, not the `kernel_irqchip` setting of QEMU inside the guest.

To capture again after restoring, set `TEST_OUTPUT_SNAPSHOT` to a new directory.
Restore that directory in another process to check a second generation. To test
forks, restore the same baseline into different `TEST_ROOT` directories. Do not
share output directories between concurrent runs. Require the printed `PASS`
marker after ten seconds of advancing counter and interrupt samples as well as
exit code zero. The observation window restarts on stalled progress; guest shutdown
before the controller finishes is a failure.

For a complete Linux guest with an ext4 workload, use the [full-OS fixture](os/README.md).

The counter fixture has no persistent guest disk or host filesystem mount. CPU, guest
memory and supported virtio device states are captured through production APIs.
It does not qualify sandbox filesystem recovery or CPU migration across hosts.

2026-10-13: Intel Linux/KVM passed capture, fresh-process restore, two independent
restores from one baseline, and capture/restore of a second generation using the
candidate KVM capability contract and the diagnostic guard bypass.

2026-10-14: AMD EPYC 9B45 with Linux 7.0.0-1014-gcp passed capture,
fresh-process restore, concurrent forks and a second snapshot generation after
two fixes: normalize the guest topology ID instead of retaining the current host
CPU number, and capture KVM's complete runtime MSR inventory for nested guests.
The ordinary guest whitelist omitted AMD hypervisor state such as VM_HSAVE_PA.
Intel Linux capture and restore also passed with the complete inventory.
Older diagnostic nested artifacts with incomplete MSR inventories now reject
before restore. The ordinary snapshot format and MSR inventory are unchanged.
Intel Windows, Linux ARM64 and broader workloads still require qualification.

On Windows the harness uses an explicit `hvc0` output device. On the AMD
Windows Server 2025 host, the initial diagnostic restore resumed the counter but
lost PIT interrupts. Reapplying saved interrupt state after WHP NestedState fixed
that reproduction. The candidate then passed two concurrent restores and repeated
recapture/restore generations. Progress lines include IF flags and PIC state to
help diagnose interrupt delivery. Counter progress alone is insufficient, and
Intel Windows, broader workloads and filesystem recovery remain unqualified.

## ARM64 on macOS

Build the KVM parent and its bare-metal ARM64 child together:

```sh
zig cc -target aarch64-linux-musl -static -pthread -Wall -Wextra -Werror \
  tests/nested-restore/guest-arm64.c tests/nested-restore/inner-arm64.S -o guest
```

Package `guest` as `/init` in the initramfs described above. The parent kernel
must include ARM KVM and VGICv3. The child runs at EL1, continuously updates RAM,
and handles virtual timer PPI 27 through its virtual GIC. Progress includes a
workload identity, interrupt count, fatal-exception flag, and parent monotonic time.

Use the same host harness and environment variables, with `KRUNFW_PATH` pointing
to an ARM64 libkrunfw dylib. macOS loads `TEST_INITRAMFS` through the kernel-copy
path. Codesign the exact host executable before running it:

```sh
codesign --force -s - --entitlements /path/to/msb-entitlements.plist \
  tests/nested-restore/target/debug/nested-restore-live
```

The ordinary build still rejects nested capture. Use a diagnostic build only
for backend qualification. In addition to the initial restore, recapture a
restored VM and restore that second snapshot: the original HVF ordering could
pass the first restore but stall on the second generation. The clock offset must
be installed before the saved timer registers so deadlines use the restored clock.

On 2026-10-13, Apple M5 Pro with macOS 26.5.1 passed diagnostic capture,
two concurrent restores from one baseline and a separate chain of four successive
restores. The same saved second-generation snapshot that stalled with the old
clock ordering resumed after the ordering fix. Two additional saved artifacts
exposed an internal error when the parent was captured at EL1, and bursty progress with an expired, notification-masked timer.
Initializing the fresh CPU at EL2 before restoring its saved mode and rearming
nested timer notifications fixed both exact reproductions. Fresh capture,
concurrent restores and repeated recapture/restore pass the strict ten-second
progress check. Preserve captures from the parent at EL1 and from EL2 timer
handling: a chain that only stops the child in its counter loop misses these cases.
The guest prints KVM internal-error data and its kernel log on failure.

These passes do not qualify a full inner operating system, filesystem recovery
or cross-host migration. The public nested-snapshot guard remains closed.
