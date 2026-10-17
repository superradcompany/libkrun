# Live nested VM restore probe

This harness boots a Linux parent VM whose KVM child continuously updates
a counter and services PIT interrupts. Capture and restore run in separate host
processes. Restore requires the original workload identity and continued timer
and counter progress, so rebooting the workload cannot pass.

Linux x86 nested snapshots are enabled in this backend PR. Other platforms remain
behind the admission guard until their own qualification is complete.

Build the Rust guest with:

```sh
cargo build --locked --release --target x86_64-unknown-linux-musl --manifest-path tests/nested-restore/guest/Cargo.toml
```

Place `guest/target/x86_64-unknown-linux-musl/release/nested-counter-guest` at `/init` in an
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

To capture again after restoring, set `TEST_OUTPUT_SNAPSHOT` to a new directory.
Restore that directory in another process to check a second generation. To test
forks, restore the same baseline into different `TEST_ROOT` directories. Do not
share output directories between concurrent runs. Require the printed `PASS`
marker as well as exit code zero: guest shutdown can terminate the runtime before
the controller finishes.

The fixture has no persistent guest disk or host filesystem mount. CPU, guest
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

## Continuous integration

The `Nested restore` workflow runs the Rust x86 counter fixture on Linux for pull
requests and pushes to `krun`. It captures a running nested VM, restores it in a
fresh process, forks the original capture, recaptures a restored VM and restores
that second generation. Both in-kernel and split outer IRQ controllers are tested.
Missing KVM/nesting or stalled counter/timer progress fails the job; it never skips
the live test. Logs and kernel configuration are retained as workflow artifacts.

`ci.sh build` builds the static guest, harness and pinned firmware with an embedded
initramfs; `ci.sh run` executes the matrix. Set `NESTED_TEST_DIR` to a dedicated
output directory and `NESTED_FIRMWARE_DIR` to a clean libkrunfw checkout. The build
requires the packages and Rust target listed in the workflow. No production kernel
or runtime admission bypass is used. This job qualifies Linux x86 on the runner
CPU only; Windows, macOS, cross-frequency and full-OS disk tests remain separate
qualification runs.
