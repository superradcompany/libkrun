# Nested virtualization implementation stack

This is an implementation plan, not a claim of released support. Nesting remains opt-in.
Full snapshots and forks remain disabled in Microsandbox until the relevant backend passes
live restore qualification. Disk-only snapshots are unaffected.

## Dependencies

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

The initial contract rejects nested capture and restore on every backend. A backend PR must
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
for this investigation. No unsuitable server was ordered. Native Windows ARM testing remains
blocked on an appropriate machine; the public API review is inconclusive about execution
and insufficient to implement full nested restore safely.

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
