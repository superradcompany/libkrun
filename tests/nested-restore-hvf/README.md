# Native macOS nested-state probe

This manual probe executes a tiny EL2 counter, captures the production HVF CPU
and GIC state, and attempts two fresh-VM restores of that counter. It does not
qualify a running L2 VM, nested interrupts, or full sandbox snapshots. The VMM
nested-snapshot admission guard remains closed.

Run on an Apple Silicon Mac that supports EL2, with the Hypervisor entitlement:

```sh
cargo build --manifest-path tests/nested-restore-hvf/Cargo.toml
codesign --force -s - --entitlements /path/to/msb-entitlements.plist \
  tests/nested-restore-hvf/target/debug/nested-hvf-probe
tests/nested-restore-hvf/target/debug/nested-hvf-probe
```

The guest run has a two-second watchdog. A capture failure exits nonzero after
destroying the test VM. No register is silently omitted to make capture succeed.

Observed on Apple M5 Pro, macOS 26.5.1 (25F80): production CPU and GIC
capture succeeds, and two fresh VMs independently resume the saved counter
from 1 to 2 without changing the baseline RAM. Recapturing the restored VM
and restoring it again advances the counter to 3. This required correcting the
old SDK's MDCR_EL2 encoding and reading the implemented virtual interrupt
list-register count from ICH_VTR_EL2 (eight on this Mac). Restore rejects a
destination with a different list-register inventory before writing CPU state.

`macos-capability.c` separately queries the framework's EL2 capability. A true
result only advertises execution support; it does not establish snapshot support.

For the running KVM child and timer-interrupt fixture, see
[`../nested-restore`](../nested-restore/README.md). Its later qualification found
clock ordering, fresh-CPU EL2 initialization and timer-notification rearming
defects that the smaller probe does not reproduce.
The smaller EL2 probe passing is not sufficient to open snapshot admission.
