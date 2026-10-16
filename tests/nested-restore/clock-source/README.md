# Cross-frequency source fixture

These test adapters create a source VM at a non-native TSC rate using the real
hypervisor API. Capture it with the normal runtime, then restore with the normal
runtime **without the adapter**. They do not rewrite saved clock values or guest
memory. This tests rate preservation on one CPU model; it is not evidence of
migration between different CPU models or vendors.

The source host must support hardware TSC scaling. KVM's catch-up mode on hosts
without `KVM_CAP_TSC_CONTROL` is not a substitute. WHP must accept the requested
partition rate before setup.

## Linux

Build `cc -shared -fPIC -Wall -Wextra linux.c -ldl -o source-clock.so`.
For source creation only, set `LD_PRELOAD` to its absolute path and
`TEST_SOURCE_TSC_KHZ` to the desired frequency in kHz. The adapter programs each
new KVM vCPU before the guest runs and verifies the kernel's readback.

## Windows

Build the `windows` crate for `x86_64-pc-windows-msvc` (for example with
`cargo xwin build`). In an isolated source executable directory, copy the output
DLL to `WinHvPlatform.dll`, and copy the host's system `WinHvPlatform.dll` to
`WinHvPlatformNative.dll`. Never replace a system DLL. Put the source executable
in that directory and set `TEST_SOURCE_TSC_HZ` to the desired frequency in Hz.

The adapter forwards WHP operations to the real system DLL, sets the source
partition rate before setup, and reports that same rate to the boot calibration
path. The destination executable must be in a separate directory with no adapter.

## Required checks

Use the full-OS nested fixture, and enable `TEST_CLOCK_MILLISECONDS=true` in the
standalone harness. For Microsandbox's nested snapshot smoke test, use
`NESTED_SOURCE_MSB_PATH` for the isolated source executable,
`NESTED_SOURCE_ENV` for a JSON object containing the source-only environment
settings, and `NESTED_CHECK_CLOCK_RATE=1`.

Require the original workload identity, disk consistency, advancing nested
timers and worker counters, and guest elapsed time within 20% of host elapsed
time. Test a lower and higher source rate, fresh-process restore, forks, and
recapture/restore. The pre-fix runtime must reject the exact different-rate
artifact. On hosts without scaling, require a clear rejection and verify that
native-rate capture and restore remain usable.
