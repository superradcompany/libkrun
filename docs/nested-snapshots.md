# Nested virtualization implementation stack

Nested virtualization is disabled by default on every host. Enable it explicitly when needed. An explicit enable still fails on unsupported hosts; an explicit disable is
always honored. Execution-state capture and
restore are enabled on qualified Linux x86-64 (Intel and AMD), Windows x86-64 (Intel and AMD),
and Apple Silicon macOS hosts, subject to the backend's hardware and restore checks.
Linux ARM64 and Windows ARM64 nesting are not currently supported and default to off; explicitly enabling nesting fails
with an unsupported error. Ordinary ARM64 VMs and their snapshots are unaffected.

The 2026-10-14 qualification includes a running inner Linux VM through fresh restore,
two forks, and second-generation capture/restore. Linux and Windows x86-64 also
passed Microsandbox archive export/import. macOS results cover the libkrun full-OS
fixture; the Microsandbox archive workflow still needs an end-to-end macOS run.
The inner workload retains its memory identity and resumes counter, timer, and disk progress.
These results do not promise cross-host migration, external mount recovery, or restored
network connections. The historical implementation notes below describe intermediate gates;
the platform policy in this section supersedes those gates.

## Intel Windows requirements

Intel Windows nested snapshots require WHP's enlightened-VMCS support and an outer
guest kernel that uses it. libkrun enables the documented Hyper-V interface when
the host provides the required features. The Windows firmware enables
`CONFIG_HYPERV`; Linux KVM selects enlightened VMCS automatically. A current
ordinary Intel VMCS is rejected during capture and restore because its cached
state can disagree with its opaque backing memory.

Nested WHP artifacts use backend ABI 4, recording the synthetic feature contract,
partition reference time and emulated PIT state. Non-nested WHP artifacts use ABI 2
and also save PIT state. Older ordinary ABI 1 and enlightened nested ABI 3 snapshots
require explicit periodic-PIT recovery or a restart from saved disk. Non-enlightened
nested snapshots remain incompatible.
AMD retains its existing guest feature configuration. These checks do not promise
migration between different CPUs or Windows builds.

## Restoring at a different host TSC frequency

On x86 Linux and Windows, restore preserves the snapshot's guest-visible TSC
frequency. The destination host's native frequency may differ only if its
hypervisor can provide the saved rate. Linux requires KVM hardware TSC scaling
for differing rates, programs each vCPU before importing nested state, and checks
readback. Windows configures the saved partition clock before partition setup,
then uses that rate for capture, compatibility checks and guest CPUID reporting.
Same-frequency Windows restores do not require the optional clock-setting API.

This is a clock compatibility feature, not unrestricted CPU migration. Existing
CPU, nested-state, MSR and backend compatibility checks still apply. Restore
rejects a rate the destination cannot provide. Linux catch-up emulation without
hardware scaling is not sufficient for a running nested VM. No guest memory or
saved kernel calibration values are rewritten, and the snapshot format is unchanged.

The [source-clock fixture](../tests/nested-restore/clock-source/README.md) exercises
non-native source rates with real hypervisor scaling and an unmodified destination.

2026-10-16 qualification: AMD Linux (EPYC 9B45, KVM on Linux 7.0) passed the busy
Microsandbox restore/fork/recapture/archive matrix at source rates of 1,347,974 and
3,369,935 kHz, against a native destination rate of 2,695,948 kHz. AMD Windows
(Ryzen 5 3600) passed the same matrix at 1,800,012,216 and 7,200,048,864 Hz, against
3,600,024,432 Hz native. The pre-fix runtimes rejected the saved lower-rate artifacts.
The inner workload retained its identity, disk contents, busy workers and timer
progress; guest/host elapsed-time ratios stayed within the test's 20% tolerance.

Intel Linux and Intel Windows passed the same-frequency busy matrix. The tested
Google N2/C4 and Azure Intel hosts did not expose hardware TSC scaling, so successful
Intel cross-frequency restore remains unqualified. The controlled-rate AMD tests
use a source adapter on the same host; migration between distinct physical hosts
and differing CPU contracts still needs separate qualification.

## Historical implementation and qualification notes

The following records describe the staged implementation before admission was opened.

### Dependencies

1. libkrun #156: Windows x86 TSC discovery.
2. libkrun #155, based on #156: host admission and opt-in nested execution.
3. This PR, based on #155: nested execution-state format and restore admission.
4. Separate backend PRs based on this contract: Linux x86, Linux ARM64, macOS ARM64,
   and Windows x86. Each must include a running-inner-VM restore proof before enabling capture.
5. Microsandbox #823 exposes configuration. A subsequent integration PR pins released
   dependencies and replaces the blanket snapshot/fork restriction with backend admission.

libkrunfw #32 is a cross-repository dependency of Windows x86 execution and its restore tests.
Do not merge the integration guard removal ahead of its qualified runtime and firmware.

## Contract

Ordinary artifacts continue to use schema 1 with unchanged framing. Schema 2 has the same
framing and identifies a nested-enabled VM; older readers reject it rather than partially
restoring an inner hypervisor. Restore checks nesting mode before any backend state mutation.
The backend ABI remains separate and must change when its opaque payload changes. Old
schema-1 artifacts are treated as ordinary: they contain no reliable nesting-mode declaration.

The initial contract rejected nested capture and restore on every backend. A backend PR must
supply complete state, validate destination capabilities, and explicitly remove its own gate.
This also prevents direct libkrun callers from bypassing the Microsandbox restriction and
silently saving incomplete nested state.

## Backend work and acceptance

- Linux x86: bounded KVM_GET_NESTED_STATE / KVM_SET_NESTED_STATE payloads; VMX/SVM
  and destination capability checks; documented ordering with MSRs and vCPU events;
  preserve ordinary snapshot compatibility. Test both Intel VMX and AMD SVM.
- Linux ARM64: audit KVM_CAP_ARM_EL2, GET_REG_LIST coverage, nested EL2 registers,
  nested exception state, GIC state and all exposed timers. Fail explicitly where the
  host kernel cannot export enough state; enabling EL2 alone does not prove migration.
- macOS ARM64: audit the existing EL2 and virtual GIC register lists against Hypervisor.framework,
  including timers and pending interrupts. Existing register serialization is not proof
  that a running inner VM can resume. Use a separate qualified Mac, not the primary developer Mac.
- Windows x86: capture and restore WHvVirtualProcessorStateTypeNestedState in addition
  to existing registers, XSAVE, APIC and SynIC state; version the payload; validate
  processor compatibility and clock frequency. Qualify both AMD and Intel.
- All: pause while an inner VM is actively incrementing a guest-memory counter and delivering
  timer interrupts; save; destroy the outer VM; restore into a fresh VM; verify progress
  continues from the saved counter with intact memory and interrupts. Repeat restores and
  fork two independent copies. A cold boot, idle KVM device, or same-VM resume is insufficient.

## Windows ARM64 feasibility (2026-10-13)

Microsoft documents WHP ARM64 starting with Windows 11 24H2 build 26100.3915. The
NestedVirtualization partition property is in the architecture-common declaration, but
that declaration alone does not establish implementation or hardware support. The documented
NestedState category is x64-only; the ARM register list does not expose an equivalent EL2
state interface. Keep Windows ARM64 unsupported pending native evidence, not because an
ARM CPU lacks virtualization in general.

Native investigation must first query WHP ARM64 support and attempt nesting-enabled partition
setup with valid GIC parameters. Then prove guest EL2 execution and a real inner VM, followed
by a complete state-export/import proof. Record Windows build, CPU model, API errors, and
state categories. An x64 host or an emulated ARM machine cannot establish native support.

Hetzner Cloud excludes Windows Server on CAX and does not expose nested virtualization.
Its dedicated RX line uses Ampere Altra; no supported Windows ARM deployment was established
for this investigation. No unsuitable server was ordered. A native Surface Laptop 7 (Snapdragon X Elite, Windows 11 25H2 build 26200.9711)
accepted ordinary ARM64 partition setup and vCPU creation. With valid GIC settings,
enabling nesting failed at WHvSetupPartition with STATUS_HV_INVALID_PARAMETER
(0xC0350005). The probe ran as a standard user; elevated confirmation remains pending.
No ARM64 nested-state capture/restore has been qualified.

Sources:
- https://learn.microsoft.com/en-us/virtualization/api/hypervisor-platform/funcs/whvpartitionpropertydatatypes
- https://learn.microsoft.com/en-us/virtualization/api/hypervisor-platform/funcs/whvgetvirtualprocessorstate
- https://learn.microsoft.com/en-us/virtualization/api/hypervisor-platform/funcs/whvvirtualprocessordatatypes
- https://docs.hetzner.com/cloud/servers/windows-on-cloud/
- https://docs.hetzner.com/cloud/servers/faq/
- https://docs.hetzner.com/robot/dedicated-server/server-lines/apple-rx-server/

## Linux x86 implementation status

The draft backend transports bounded KVM nested-state blobs and restores them after
ordinary CPU/MSR state, before saved vCPU events. Nested payloads use backend ABI 3
and include source KVM feature MSRs; ordinary payloads remain ABI 1. No dependency
version bump is needed. A read-only preflight checks every paused destination vCPU's
CPUID, TSC frequency, MSR inventory and KVM feature MSRs before restoring VM state.

The VMM admission gate deliberately remains closed. On 2026-10-13 an Intel GCloud
host passed diagnostic capture and fresh-process restore of a running KVM inner VM,
two independent restores from one baseline, and a second capture/restore generation.
The fixture checks workload identity, counter progress and PIT interrupt progress.
The ordinary build still refuses nested capture. Native idle-state transfer,
incompatible-feature rejection and bounded codec checks also passed.

These diagnostic results do not qualify AMD Linux, broader interrupt workloads,
cross-host migration or sandbox filesystem recovery. Complete those checks and
review each vendor's active nested restore ordering before opening admission.

## Windows x86 implementation status

The draft backend requests WHP's bounded opaque NestedState blob alongside its existing
CPU state. Ordinary snapshots now use backend ABI 2; enlightened nested snapshots
use ABI 4. Both include the complete supported emulated PIT state: channel registers,
reload/mode, remaining time, partial writes, and channel-2 read position. The PIT
worker stops before interrupt-state capture and resumes only after restoration. Restore
installs nested state before APIC/SynIC state. Missing nested state is an error.

The VMM gate remains closed. Intel nested restore ordering and destination VMX/SVM
compatibility still require native qualification.
This layer does not add Windows ARM support or claim that the ARM API has the same state type.


On 2026-10-13 the AMD Windows Server 2025 host captured a running KVM child,
but the initial restore lost timer interrupts despite continuing counter progress.
The outer CPU 0 APIC timer expired without a pending or in-service interrupt;
CPU 1 and the guest clock continued. Loading WHP NestedState overwrote the
previously restored interrupt state. Reapplying the captured interrupt state
after NestedState restored timer delivery from the same failing snapshot.

The corrected diagnostic build passed fresh-process restore, two concurrent
restores from a new baseline, and repeated recapture/restore generations while
preserving workload identity and both counter and PIT interrupt progress.
The ordinary build still rejects nested capture. These results do not qualify
Intel Windows, other nested interrupt workloads, cross-host migration or sandbox
filesystem recovery; the public guard remains closed.

Older ordinary ABI 1 and enlightened nested ABI 3 snapshots omitted the PIT. Normal
restore rejects them with recovery guidance rather than risking a frozen guest clock.
`ExecutionState::recover_windows_periodic_pit(divisor)` explicitly supplies a known
periodic channel-0 reload (1..=65536), starting a fresh period. It preserves the
original CPU payload and does not bypass host compatibility checks. The missing
phase, partial programming, and other channels cannot be reconstructed; validate
the recovered guest and capture a new snapshot. Older nested ABI 2 snapshots cannot
use this recovery because their non-enlightened VMCS state is independently unsafe.

## macOS native qualification

On Apple M5 Pro with macOS 26.5.1 (25F80), production CPU/GIC capture and two
independent fresh-VM restores pass in the native EL2 counter probe. Recapturing
a restored VM and restoring that state again also passes. The earlier MDCR_EL2
failure came from an obsolete SDK encoding; it was not an unavailable API.
Capture also now uses ICH_VTR_EL2's implemented list-register count (eight on
this host), and restore checks the destination inventory before writing CPU state.

The small probe in `tests/nested-restore-hvf` rejects watchdog cancellation as
successful execution. The ARM64 fixture in `tests/nested-restore` additionally
runs a real KVM child at EL1 with a virtual GIC and 100 Hz timer interrupts.
Diagnostic full-memory capture and fresh-process restore preserved its identity,
counter and interrupts for at least ten seconds. Two concurrent restores from
one baseline passed, as did a separate chain of four successive restores.

That workload exposed a restore-order defect: installing the HVF clock offset
after timer registers could stall a recaptured VM. Installing the offset first
resumed the same saved failing snapshot, without discarding clock rebasing.
Two further saved failures required initializing the fresh vCPU at EL2 before
restoring its saved mode, and rearming the framework's timer notifications. A
parent captured at EL1 otherwise returned NOT_SUPPORTED from its next nested
hypercall; retaining the old notification mask caused bursty timer progress.
Both exact artifacts now pass the sustained-progress check. Architectural timer
and GIC state are restored; only the nested framework notification mask is reset.
Fresh capture, concurrent restores and repeated recapture/restore also pass.
Nested snapshot admission stays closed. This fixture does not qualify a full
inner operating system, sandbox filesystem recovery or cross-host migration.

## Apple Silicon cross-frequency restore investigation

The current macOS restore path requires the saved Mach timebase to match the
host. It rebases the virtual-counter offset once for all vCPUs before restoring
timer deadlines. It does not scale the counter rate.

An audit of the public Hypervisor.framework headers and documentation found no
supported API that forces every required counter/timer access to exit to the
userspace VMM while retaining nested virtualization. This is an API-contract
finding, not proof about undocumented hardware behavior. No new native trap
experiments were performed for this investigation.

| Access | What an emulator needs | Public HVF interface and gap |
| --- | --- | --- |
| `CNTVCT_EL0`, `CNTPCT_EL0`, `CNTFRQ_EL0` | Return a coherent saved-rate counter and frequency to applications, kernels and the guest hypervisor | HVF exposes a virtual-counter offset, but no counter multiplier or documented host-wide counter-read trap switch. |
| `CNTV_*`, `CNTP_*`, `CNTHP_*` timer registers | Translate control, relative/absolute deadlines and reads into the same clock domain | Register get/set APIs permit state transfer; they do not promise an exit for each guest access. Timer-expiry notifications are not access traps. |
| `CNTKCTL_EL1`, `CNTHCTL_EL2`, `CNTVOFF_EL2` | Preserve the guest hypervisor's own timer and trap policy while applying host scaling | These are guest architectural state. Changing lower-level trap routing does not establish a host-owned interception path for the guest hypervisor's own accesses. The guest can also change its controls. |

The existing system-register-exit handler only handles accesses HVF actually
returns. Adding counter cases to it would not ensure that direct counter reads
or hardware-managed nested timers reach that handler. Likewise, changing the
reported frequency or periodically adjusting an offset would not preserve a
running guest's rate and deadlines.

The preferred prerequisite is an Apple-supported counter scaling API that also
covers nested timers. A software alternative first needs a documented or safely
qualified host interception mechanism covering the full table, including the
guest hypervisor at virtual EL2 and a nested guest at EL0/EL1. Only then could we
implement a shared scaled counter, deadline conversion, nested offset composition
and interrupt delivery. Validation would need to prove interception even when
the guest changes its controls, followed by multi-vCPU clock monotonicity,
physical/virtual timer expiry, busy nested execution, restore, fork and recapture
at lower and higher rates. Partial interception is insufficient. Keep rejecting
frequency mismatches until that prerequisite is established.

Sources:

- [Apple Silicon HVF API](https://developer.apple.com/documentation/hypervisor/apple-silicon)
- [Virtual timer offset](https://developer.apple.com/documentation/hypervisor/hv_vcpu_set_vtimer_offset(_:_:)):
  the installed SDK specifies `CNTVCT_EL0 = mach_absolute_time() - vtimer_offset`.
- [Guest CNTHCTL_EL2 register](https://developer.apple.com/documentation/hypervisor/hv_sys_reg_cnthctl_el2)
- [Explicit debug-register trap API](https://developer.apple.com/documentation/hypervisor/hv_vcpu_get_trap_debug_reg_accesses(_:_:)):
  the public vCPU API has debug trap controls, but no corresponding all-counter
  or all-timer trap control.
- [QEMU HVF backend](https://github.com/qemu/qemu/blob/master/target/arm/hvf/hvf.c):
  initializes `gt_cntfrq_hz` from host `CNTFRQ_EL0` and checks that they agree;
  this is supporting implementation evidence, not an Apple capability guarantee.

## TSC naming and snapshot units

Linux and Windows x86 state, VM resources and restore paths use
`tsc_frequency_hz` in memory. Windows applies it before partition setup; Linux
applies it when restoring each vCPU.
KVM's API and the existing Linux snapshot wire format still use `u32` kHz. The
Linux codec converts exactly at the boundary and rejects values that would need
rounding or overflow. Existing snapshot bytes and compatibility checks are
unchanged. ARM counter/timebase fields remain architecture-specific; they are
not x86 TSCs.
