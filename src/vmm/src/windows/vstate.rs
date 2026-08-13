// Copyright 2026 Super Rad Company.
// SPDX-License-Identifier: Apache-2.0

use std::fmt::{Display, Formatter};
use std::mem::{size_of, zeroed};
use std::result;
#[cfg(target_arch = "aarch64")]
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
#[cfg(target_arch = "x86_64")]
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

use crate::vmm_config::machine_config::{CpuFeaturesTemplate, HostCpuId};

use crate::resources::VcpuPlacementResult;
use arch::ArchMemoryInfo;
#[cfg(target_arch = "x86_64")]
use crossbeam_channel::RecvTimeoutError;
use crossbeam_channel::{unbounded, Receiver, Sender, TryRecvError};
use utils::{eventfd::EventFd, metrics::MetricsWriter};
#[cfg(target_arch = "x86_64")]
use vm_memory::Bytes;
use vm_memory::{Address, GuestAddress, GuestMemoryBackend, GuestMemoryMmap, GuestMemoryRegion};
use windows_sys::Win32::Foundation::FILETIME;
#[cfg(target_arch = "x86_64")]
use windows_sys::Win32::Foundation::{E_FAIL, E_INVALIDARG, S_OK};
use windows_sys::Win32::System::Hypervisor::{
    WHvCancelRunVirtualProcessor, WHvCapabilityCodeHypervisorPresent, WHvCreatePartition,
    WHvCreateVirtualProcessor, WHvDeletePartition, WHvDeleteVirtualProcessor, WHvGetCapability,
    WHvGetVirtualProcessorRegisters, WHvMapGpaRange, WHvMapGpaRangeFlagExecute,
    WHvMapGpaRangeFlagRead, WHvMapGpaRangeFlagWrite, WHvPartitionPropertyCodeProcessorCount,
    WHvRunVirtualProcessor, WHvRunVpExitReasonNone, WHvSetPartitionProperty,
    WHvSetVirtualProcessorRegisters, WHvSetupPartition, WHvUnmapGpaRange, WHV_CAPABILITY,
    WHV_MAP_GPA_RANGE_FLAGS, WHV_PARTITION_HANDLE, WHV_REGISTER_NAME, WHV_REGISTER_VALUE,
    WHV_RUN_VP_EXIT_REASON,
};
#[cfg(target_arch = "x86_64")]
use windows_sys::Win32::System::Hypervisor::{
    WHvCapabilityCodeExtendedVmExits, WHvCapabilityCodeProcessorClockFrequency,
    WHvPartitionPropertyCodeExtendedVmExits, WHvPartitionPropertyCodeLocalApicEmulationMode,
    WHvRegisterInternalActivityState, WHvRunVpExitReasonCanceled, WHvRunVpExitReasonMemoryAccess,
    WHvRunVpExitReasonX64ApicInitSipiTrap, WHvRunVpExitReasonX64Cpuid, WHvRunVpExitReasonX64Halt,
    WHvRunVpExitReasonX64IoPortAccess, WHvTranslateGva, WHvTranslateGvaResultSuccess,
    WHvX64LocalApicEmulationModeXApic, WHvX64RegisterApicId, WHvX64RegisterCr0, WHvX64RegisterCr2,
    WHvX64RegisterCr3, WHvX64RegisterCr4, WHvX64RegisterCs, WHvX64RegisterDs, WHvX64RegisterEfer,
    WHvX64RegisterEs, WHvX64RegisterFs, WHvX64RegisterGdtr, WHvX64RegisterGs, WHvX64RegisterIdtr,
    WHvX64RegisterRax, WHvX64RegisterRbp, WHvX64RegisterRbx, WHvX64RegisterRcx, WHvX64RegisterRdx,
    WHvX64RegisterRflags, WHvX64RegisterRip, WHvX64RegisterRsi, WHvX64RegisterRsp,
    WHvX64RegisterSs, WHvX64RegisterTr, WHV_EMULATOR_CALLBACKS, WHV_EMULATOR_IO_ACCESS_INFO,
    WHV_EMULATOR_MEMORY_ACCESS_INFO, WHV_EMULATOR_STATUS, WHV_MEMORY_ACCESS_CONTEXT,
    WHV_RUN_VP_EXIT_CONTEXT, WHV_TRANSLATE_GVA_FLAGS, WHV_TRANSLATE_GVA_RESULT,
    WHV_TRANSLATE_GVA_RESULT_CODE, WHV_X64_CPUID_ACCESS_CONTEXT, WHV_X64_IO_PORT_ACCESS_CONTEXT,
    WHV_X64_SEGMENT_REGISTER, WHV_X64_SEGMENT_REGISTER_0, WHV_X64_TABLE_REGISTER,
};
use windows_sys::Win32::System::SystemInformation::GROUP_AFFINITY;
#[cfg(test)]
use windows_sys::Win32::System::Threading::GetThreadGroupAffinity;
use windows_sys::Win32::System::Threading::{
    GetCurrentThread, GetThreadTimes, SetThreadGroupAffinity,
};

#[cfg(target_arch = "aarch64")]
const AARCH64_PSR_MODE_EL1H: u64 = 0x0000_0005;
#[cfg(target_arch = "aarch64")]
const AARCH64_PSR_F_BIT: u64 = 0x0000_0040;
#[cfg(target_arch = "aarch64")]
const AARCH64_PSR_I_BIT: u64 = 0x0000_0080;
#[cfg(target_arch = "aarch64")]
const AARCH64_PSR_A_BIT: u64 = 0x0000_0100;
#[cfg(target_arch = "aarch64")]
const AARCH64_PSR_D_BIT: u64 = 0x0000_0200;
#[cfg(target_arch = "aarch64")]
const AARCH64_PSTATE_FAULT_BITS_64: u64 = AARCH64_PSR_MODE_EL1H
    | AARCH64_PSR_A_BIT
    | AARCH64_PSR_F_BIT
    | AARCH64_PSR_I_BIT
    | AARCH64_PSR_D_BIT;
#[cfg(target_arch = "aarch64")]
const AARCH64_MPIDR_U_BIT: u64 = 1 << 30;
#[cfg(target_arch = "aarch64")]
const AARCH64_MPIDR_RES1_BIT: u64 = 1 << 31;
#[cfg(target_arch = "aarch64")]
const WHV_PARTITION_PROPERTY_CODE_ARM64_IC_PARAMETERS: i32 = 0x0000_1012;
#[cfg(target_arch = "aarch64")]
const WHV_ARM64_IC_EMULATION_MODE_GIC_V3: u32 = 1;
#[cfg(target_arch = "aarch64")]
pub(crate) const WHP_GICD_BASE: u64 = 0x0800_0000;
#[cfg(target_arch = "aarch64")]
pub(crate) const WHP_GICD_SIZE: u64 = 0x0001_0000;
#[cfg(target_arch = "aarch64")]
const WHP_GITS_TRANSLATER_BASE: u64 = 0x0808_0000;
#[cfg(target_arch = "aarch64")]
pub(crate) const WHP_GICR_BASE: u64 = 0x080a_0000;
#[cfg(target_arch = "aarch64")]
pub(crate) const WHP_GICR_SIZE: u64 = 0x0002_0000;
#[cfg(target_arch = "aarch64")]
const WHP_GIC_LPI_INT_ID_BITS: u32 = 1;
#[cfg(target_arch = "aarch64")]
const WHP_GIC_PPI_PERFORMANCE_MONITORS_INTERRUPT: u32 = 23;
#[cfg(target_arch = "aarch64")]
const WHV_ARM64_REGISTER_X0: WHV_REGISTER_NAME = 0x0002_0000;
#[cfg(target_arch = "aarch64")]
const WHV_ARM64_REGISTER_PC: WHV_REGISTER_NAME = 0x0002_0022;
#[cfg(target_arch = "aarch64")]
const WHV_ARM64_REGISTER_PSTATE: WHV_REGISTER_NAME = 0x0002_0023;
#[cfg(target_arch = "aarch64")]
const WHV_ARM64_REGISTER_MPIDR_EL1: WHV_REGISTER_NAME = 0x0004_0001;
#[cfg(target_arch = "aarch64")]
const WHV_ARM64_REGISTER_GICR_BASE_GPA: WHV_REGISTER_NAME = 0x0006_3000;
#[cfg(target_arch = "aarch64")]
const WHV_ARM64_EXIT_REASON_UNMAPPED_GPA: WHV_RUN_VP_EXIT_REASON = 0x8000_0000_u32 as i32;
#[cfg(target_arch = "aarch64")]
const WHV_ARM64_EXIT_REASON_GPA_INTERCEPT: WHV_RUN_VP_EXIT_REASON = 0x8000_0001_u32 as i32;
#[cfg(target_arch = "aarch64")]
const WHV_ARM64_EXIT_REASON_CANCELED: WHV_RUN_VP_EXIT_REASON = 0xffff_ffff_u32 as i32;
#[cfg(target_arch = "aarch64")]
const WHV_ARM64_EXIT_REASON_RESET: WHV_RUN_VP_EXIT_REASON = 0x8001_000c_u32 as i32;
#[cfg(target_arch = "aarch64")]
const WHV_ARM64_RESET_TYPE_POWER_OFF: u32 = 0;
#[cfg(target_arch = "aarch64")]
const WHV_ARM64_RESET_TYPE_REBOOT: u32 = 1;
#[cfg(target_arch = "aarch64")]
const AARCH64_EXCEPTION_CLASS_DATA_ABORT_LOWER: u64 = 0b100100;
#[cfg(target_arch = "aarch64")]
const AARCH64_ESR_ISV: u64 = 1 << 24;
#[cfg(target_arch = "aarch64")]
const AARCH64_ZERO_REGISTER_INDEX: u8 = 31;

#[cfg(target_arch = "x86_64")]
const WHV_EMULATOR_DIRECTION_READ: u8 = 0;
#[cfg(target_arch = "x86_64")]
const WHV_EMULATOR_DIRECTION_WRITE: u8 = 1;
#[cfg(target_arch = "x86_64")]
const WHV_EMULATOR_STATUS_SUCCESS: u32 = 1;

#[cfg(target_arch = "x86_64")]
type WhvEmulatorCreateEmulator = unsafe extern "system" fn(
    *const WHV_EMULATOR_CALLBACKS,
    *mut *mut core::ffi::c_void,
) -> windows_sys::core::HRESULT;
#[cfg(target_arch = "x86_64")]
type WhvEmulatorDestroyEmulator =
    unsafe extern "system" fn(*const core::ffi::c_void) -> windows_sys::core::HRESULT;
#[cfg(target_arch = "x86_64")]
type WhvEmulatorTryMmioEmulation = unsafe extern "system" fn(
    *const core::ffi::c_void,
    *const core::ffi::c_void,
    *const windows_sys::Win32::System::Hypervisor::WHV_VP_EXIT_CONTEXT,
    *const WHV_MEMORY_ACCESS_CONTEXT,
    *mut WHV_EMULATOR_STATUS,
) -> windows_sys::core::HRESULT;
#[cfg(target_arch = "x86_64")]
type WhvEmulatorTryIoEmulation = unsafe extern "system" fn(
    *const core::ffi::c_void,
    *const core::ffi::c_void,
    *const windows_sys::Win32::System::Hypervisor::WHV_VP_EXIT_CONTEXT,
    *const WHV_X64_IO_PORT_ACCESS_CONTEXT,
    *mut WHV_EMULATOR_STATUS,
) -> windows_sys::core::HRESULT;

#[cfg(target_arch = "x86_64")]
const X86_BOOT_GDT_OFFSET: u64 = 0x500;
#[cfg(target_arch = "x86_64")]
const X86_BOOT_IDT_OFFSET: u64 = 0x520;
#[cfg(target_arch = "x86_64")]
const X86_BOOT_GDT_MAX: usize = 4;
#[cfg(target_arch = "x86_64")]
const X86_PML4_START: u64 = 0x9000;
#[cfg(target_arch = "x86_64")]
const X86_PDPTE_START: u64 = 0xa000;
#[cfg(target_arch = "x86_64")]
const X86_PDE_START: u64 = 0xb000;
#[cfg(target_arch = "x86_64")]
const X86_EFER_LMA: u64 = 0x400;
#[cfg(target_arch = "x86_64")]
const X86_EFER_LME: u64 = 0x100;
#[cfg(target_arch = "x86_64")]
const X86_CR0_PE: u64 = 0x1;
#[cfg(target_arch = "x86_64")]
const X86_CR0_PG: u64 = 0x8000_0000;
#[cfg(target_arch = "x86_64")]
const X86_CR4_PAE: u64 = 0x20;

#[derive(Debug)]
pub enum Error {
    CreatePartition(windows_sys::core::HRESULT),
    CreateVirtualProcessor {
        id: u8,
        hresult: windows_sys::core::HRESULT,
    },
    CancelRunVirtualProcessor {
        id: u8,
        hresult: windows_sys::core::HRESULT,
    },
    DeleteVirtualProcessor {
        id: u8,
        hresult: windows_sys::core::HRESULT,
    },
    GetCapability {
        code: i32,
        hresult: windows_sys::core::HRESULT,
    },
    GuestMemoryHostAddress(vm_memory::GuestMemoryError),
    GuestMemoryWrite(&'static str),
    HypervisorNotPresent,
    LoadEmulator(libloading::Error),
    LoadEmulatorSymbol {
        symbol: &'static str,
        error: libloading::Error,
    },
    CreateEmulator(windows_sys::core::HRESULT),
    DestroyEmulator(windows_sys::core::HRESULT),
    EmulatorMmio {
        hresult: windows_sys::core::HRESULT,
        status: u32,
    },
    #[cfg(target_arch = "x86_64")]
    EmulatorIo {
        hresult: windows_sys::core::HRESULT,
        status: u32,
    },
    #[cfg(target_arch = "aarch64")]
    Arm64Mmio {
        id: u8,
        guest_addr: u64,
        size: usize,
        is_write: bool,
    },
    #[cfg(target_arch = "aarch64")]
    Arm64MmioSyndrome {
        id: u8,
        syndrome: u64,
    },
    MapGpaRange {
        guest_addr: u64,
        size: u64,
        hresult: windows_sys::core::HRESULT,
    },
    NotImplemented(&'static str),
    RunVirtualProcessor {
        id: u8,
        hresult: windows_sys::core::HRESULT,
    },
    GetVirtualProcessorRegisters {
        id: u8,
        hresult: windows_sys::core::HRESULT,
    },
    #[cfg(target_arch = "x86_64")]
    ApicInitSipiTrapUnsupported,
    SetVirtualProcessorRegisters {
        id: u8,
        hresult: windows_sys::core::HRESULT,
    },
    SetPartitionProperty {
        property: i32,
        hresult: windows_sys::core::HRESULT,
    },
    SetupPartition(windows_sys::core::HRESULT),
    UnmapGpaRange {
        guest_addr: u64,
        size: u64,
        hresult: windows_sys::core::HRESULT,
    },
    UnhandledExit {
        id: u8,
        reason: WHV_RUN_VP_EXIT_REASON,
    },
    VcpuAffinity(std::io::Error),
    VcpuThreadSpawn(std::io::Error),
    VcpuCountZero,
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        match self {
            Error::CreatePartition(hresult) => write!(
                f,
                "WHvCreatePartition failed with HRESULT 0x{:08x}",
                *hresult as u32
            ),
            Error::CreateVirtualProcessor { id, hresult } => write!(
                f,
                "WHvCreateVirtualProcessor({id}) failed with HRESULT 0x{:08x}",
                *hresult as u32
            ),
            Error::CancelRunVirtualProcessor { id, hresult } => write!(
                f,
                "WHvCancelRunVirtualProcessor({id}) failed with HRESULT 0x{:08x}",
                *hresult as u32
            ),
            Error::DeleteVirtualProcessor { id, hresult } => write!(
                f,
                "WHvDeleteVirtualProcessor({id}) failed with HRESULT 0x{:08x}",
                *hresult as u32
            ),
            Error::GetCapability { code, hresult } => write!(
                f,
                "WHvGetCapability({code}) failed with HRESULT 0x{:08x}",
                *hresult as u32
            ),
            Error::GuestMemoryHostAddress(e) => {
                write!(f, "cannot resolve guest memory host address: {e:?}")
            }
            Error::GuestMemoryWrite(region) => write!(f, "cannot write x86_64 boot {region}"),
            Error::HypervisorNotPresent => write!(
                f,
                "Windows Hypervisor Platform is not available. Enable Windows Hypervisor Platform and virtualization support, then reboot."
            ),
            Error::LoadEmulator(error) => {
                write!(f, "cannot load WinHvEmulation.dll: {error}")
            }
            Error::LoadEmulatorSymbol { symbol, error } => {
                write!(f, "cannot load WinHvEmulation.dll symbol {symbol}: {error}")
            }
            Error::CreateEmulator(hresult) => write!(
                f,
                "WHvEmulatorCreateEmulator failed with HRESULT 0x{:08x}",
                *hresult as u32
            ),
            Error::DestroyEmulator(hresult) => write!(
                f,
                "WHvEmulatorDestroyEmulator failed with HRESULT 0x{:08x}",
                *hresult as u32
            ),
            Error::EmulatorMmio { hresult, status } => write!(
                f,
                "WHvEmulatorTryMmioEmulation failed with HRESULT 0x{:08x}, status 0x{status:08x}",
                *hresult as u32
            ),
            #[cfg(target_arch = "x86_64")]
            Error::EmulatorIo { hresult, status } => write!(
                f,
                "WHvEmulatorTryIoEmulation failed with HRESULT 0x{:08x}, status 0x{status:08x}",
                *hresult as u32
            ),
            #[cfg(target_arch = "aarch64")]
            Error::Arm64Mmio {
                id,
                guest_addr,
                size,
                is_write,
            } => write!(
                f,
                "unhandled ARM64 WHP MMIO access on vCPU {id}: gpa=0x{guest_addr:x}, size={size}, write={is_write}"
            ),
            #[cfg(target_arch = "aarch64")]
            Error::Arm64MmioSyndrome { id, syndrome } => write!(
                f,
                "unsupported ARM64 WHP MMIO syndrome on vCPU {id}: syndrome=0x{syndrome:x}"
            ),
            Error::MapGpaRange {
                guest_addr,
                size,
                hresult,
            } => write!(
                f,
                "WHvMapGpaRange(guest_addr=0x{guest_addr:x}, size=0x{size:x}) failed with HRESULT 0x{:08x}",
                *hresult as u32
            ),
            Error::NotImplemented(feature) => {
                write!(f, "WHP backend support is not implemented yet: {feature}")
            }
            Error::RunVirtualProcessor { id, hresult } => write!(
                f,
                "WHvRunVirtualProcessor({id}) failed with HRESULT 0x{:08x}",
                *hresult as u32
            ),
            Error::GetVirtualProcessorRegisters { id, hresult } => write!(
                f,
                "WHvGetVirtualProcessorRegisters({id}) failed with HRESULT 0x{:08x}",
                *hresult as u32
            ),
            #[cfg(target_arch = "x86_64")]
            Error::ApicInitSipiTrapUnsupported => write!(
                f,
                "this host's Windows Hypervisor Platform cannot trap INIT/SIPI, \
                 which multi-CPU guests require; retry with a single CPU"
            ),
            Error::SetVirtualProcessorRegisters { id, hresult } => write!(
                f,
                "WHvSetVirtualProcessorRegisters({id}) failed with HRESULT 0x{:08x}",
                *hresult as u32
            ),
            Error::SetPartitionProperty { property, hresult } => write!(
                f,
                "WHvSetPartitionProperty({property}) failed with HRESULT 0x{:08x}",
                *hresult as u32
            ),
            Error::SetupPartition(hresult) => write!(
                f,
                "WHvSetupPartition failed with HRESULT 0x{:08x}",
                *hresult as u32
            ),
            Error::UnmapGpaRange {
                guest_addr,
                size,
                hresult,
            } => write!(
                f,
                "WHvUnmapGpaRange(guest_addr=0x{guest_addr:x}, size=0x{size:x}) failed with HRESULT 0x{:08x}",
                *hresult as u32
            ),
            Error::UnhandledExit { id, reason } => {
                write!(f, "WHP vCPU {id} exited with unhandled reason {reason}")
            }
            Error::VcpuAffinity(error) => {
                write!(f, "cannot apply WHP vCPU host affinity: {error}")
            }
            Error::VcpuThreadSpawn(e) => write!(f, "cannot spawn WHP vCPU thread: {e}"),
            Error::VcpuCountZero => write!(f, "WHP partition requires at least one vCPU"),
        }
    }
}

pub type Result<T> = result::Result<T, Error>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WhpCapabilities {
    pub hypervisor_present: bool,
}

pub struct Vm {
    partition: Partition,
    memory_regions: Vec<MappedMemoryRegion>,
}

#[derive(Debug, Eq, PartialEq)]
pub struct VcpuConfig {
    pub vcpu_count: u8,
    /// Maximum possible guest VCPUs. Always equal to `vcpu_count` on WHP, which
    /// has no parked-vcpu bring-up; wider capacity is rejected at config time.
    pub max_vcpu_count: u8,
    pub ht_enabled: bool,
    pub cpu_template: Option<CpuFeaturesTemplate>,
}

pub struct Vcpu {
    id: u8,
    host_cpu: Option<HostCpuId>,
    host_cpu_required: bool,
    partition_handle: WHV_PARTITION_HANDLE,
    guest_mem: Option<GuestMemoryMmap>,
    mmio_bus: Option<devices::Bus>,
    #[cfg(target_arch = "x86_64")]
    pio_bus: Option<devices::Bus>,
    #[cfg(target_arch = "x86_64")]
    sipi_router: Option<Arc<ApStartupRouter>>,
    exit_evt: EventFd,
    metrics: MetricsWriter,
    event_sender: Option<Sender<VcpuEvent>>,
    event_receiver: Option<Receiver<VcpuEvent>>,
    response_sender: Option<Sender<VcpuResponse>>,
    response_receiver: Option<Receiver<VcpuResponse>>,
}

/// Boot-time state of an x86 application processor under WHP.
///
/// WHP's in-hypervisor local APIC emulation does not start APs itself: an
/// ICR write on the sending vCPU exits with `X64ApicInitSipiTrap` and the
/// VMM must apply the INIT/SIPI to the targets. APs therefore stay parked
/// (architectural reset state, never entered) until a startup IPI arrives.
#[cfg(target_arch = "x86_64")]
#[derive(Clone, Copy, PartialEq, Eq)]
enum ApParkState {
    /// Waiting for a startup IPI; the vCPU thread has not entered the guest.
    Parked,
    /// Startup IPI received with this vector; the AP thread will apply the
    /// real-mode startup state and begin running.
    SipiPending(u8),
    /// The vCPU is executing guest code.
    Running,
}

/// Routes INIT/SIPI IPIs trapped on the sending vCPU to parked APs.
#[cfg(target_arch = "x86_64")]
pub struct ApStartupRouter {
    slots: Vec<Mutex<ApParkState>>,
}

#[cfg(target_arch = "x86_64")]
impl ApStartupRouter {
    const ICR_DELIVERY_INIT: u64 = 0b101;
    const ICR_DELIVERY_STARTUP: u64 = 0b110;

    pub fn new(vcpu_count: u8) -> Self {
        let slots = (0..vcpu_count)
            .map(|id| {
                // The BSP boots directly at the kernel entry point.
                Mutex::new(if id == 0 {
                    ApParkState::Running
                } else {
                    ApParkState::Parked
                })
            })
            .collect();
        Self { slots }
    }

    fn vcpu_count(&self) -> u8 {
        self.slots.len() as u8
    }

    /// Applies a trapped ICR write. INIT is a no-op for parked APs (they
    /// already hold reset state); a startup IPI records the vector and
    /// wakes the target. INIT/SIPI to a running vCPU is ignored, so CPU
    /// re-onlining after offline is not supported yet on WHP.
    fn deliver_icr(&self, sender: u8, icr: u64) {
        let delivery_mode = (icr >> 8) & 0x7;
        if delivery_mode == Self::ICR_DELIVERY_INIT {
            return;
        }
        if delivery_mode != Self::ICR_DELIVERY_STARTUP {
            warn!("WHP vCPU {sender}: unsupported trapped IPI ignored (icr={icr:#x})");
            return;
        }

        let vector = (icr & 0xff) as u8;
        let shorthand = (icr >> 18) & 0x3;
        let logical_destination = (icr >> 11) & 0x1 == 1;
        let targets: Vec<u8> = match shorthand {
            // All-including-self and all-excluding-self: the sender is
            // already running either way, so both reduce to "all others".
            0b10 | 0b11 => (0..self.slots.len() as u8)
                .filter(|&t| t != sender)
                .collect(),
            // Physical destination: xAPIC IDs match vCPU indices here.
            0b00 if !logical_destination => vec![((icr >> 56) & 0xff) as u8],
            _ => {
                warn!("WHP vCPU {sender}: unsupported SIPI destination ignored (icr={icr:#x})");
                return;
            }
        };

        for target in targets {
            let Some(slot) = self.slots.get(target as usize) else {
                warn!("WHP vCPU {sender}: SIPI to unknown vCPU {target} ignored");
                continue;
            };
            let mut state = slot.lock().unwrap();
            if *state != ApParkState::Running {
                debug!("WHP vCPU {sender}: startup IPI vector {vector:#x} -> vCPU {target}");
                *state = ApParkState::SipiPending(vector);
            }
        }
    }

    /// Parks an AP thread until its startup IPI arrives, keeping the
    /// pause/resume protocol responsive so VM-wide pauses cannot deadlock
    /// on a CPU the guest has not started. Returns the SIPI vector, or
    /// `Err` when the control channel closed (VM shutdown).
    fn wait_for_sipi(
        &self,
        id: u8,
        event_receiver: &Receiver<VcpuEvent>,
        response_sender: &Sender<VcpuResponse>,
    ) -> result::Result<u8, ()> {
        let slot = &self.slots[id as usize];
        loop {
            {
                let mut state = slot.lock().unwrap();
                if let ApParkState::SipiPending(vector) = *state {
                    *state = ApParkState::Running;
                    debug!("WHP vCPU {id}: unparked by startup IPI (vector {vector:#x})");
                    return Ok(vector);
                }
            }
            match event_receiver.recv_timeout(Duration::from_millis(10)) {
                Ok(VcpuEvent::Pause) => {
                    let _ = response_sender.send(VcpuResponse::Paused);
                }
                Ok(VcpuEvent::Resume) => {
                    let _ = response_sender.send(VcpuResponse::Resumed);
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Err(()),
            }
        }
    }
}

#[allow(unused)]
#[derive(Debug)]
pub enum VcpuEvent {
    Pause,
    Resume,
}

#[derive(Debug, Eq, PartialEq)]
pub enum VcpuResponse {
    Paused,
    Resumed,
    Exited(u8),
}

pub struct VcpuHandle {
    id: u8,
    partition_handle: WHV_PARTITION_HANDLE,
    event_sender: Option<Sender<VcpuEvent>>,
    response_receiver: Receiver<VcpuResponse>,
    vcpu_thread: Option<thread::JoinHandle<()>>,
}

#[derive(Default)]
struct Partition {
    handle: WHV_PARTITION_HANDLE,
}

#[cfg(target_arch = "x86_64")]
struct Emulator {
    _library: libloading::Library,
    handle: *mut core::ffi::c_void,
    destroy_emulator: WhvEmulatorDestroyEmulator,
    try_mmio_emulation: WhvEmulatorTryMmioEmulation,
    try_io_emulation: WhvEmulatorTryIoEmulation,
}

struct EmulatorContext {
    id: u8,
    partition_handle: WHV_PARTITION_HANDLE,
    #[cfg(target_arch = "x86_64")]
    guest_mem: GuestMemoryMmap,
    mmio_bus: Option<devices::Bus>,
    #[cfg(target_arch = "x86_64")]
    pio_bus: Option<devices::Bus>,
}

// WHP register values are 128-bit ABI slots; `windows-sys` models the union with only 8-byte
// alignment, but WinHvPlatform reads the register-value array as 16-byte-aligned storage.
#[repr(C, align(16))]
#[derive(Clone, Copy)]
struct AlignedRegisterValue(WHV_REGISTER_VALUE);

#[cfg(target_arch = "aarch64")]
#[derive(Default)]
#[repr(C)]
struct WhvArm64RunVpExitContext {
    exit_reason: WHV_RUN_VP_EXIT_REASON,
    reserved: u32,
    reserved1: u64,
    message: WhvArm64RunVpExitMessage,
}

#[cfg(target_arch = "aarch64")]
#[repr(C, align(16))]
struct WhvArm64RunVpExitMessage {
    bytes: [u8; 240],
    extra: [u8; 16],
}

#[cfg(target_arch = "aarch64")]
impl Default for WhvArm64RunVpExitMessage {
    fn default() -> Self {
        Self {
            bytes: [0; 240],
            extra: [0; 16],
        }
    }
}

#[cfg(target_arch = "aarch64")]
#[repr(C)]
#[derive(Clone, Copy)]
struct HvArm64InterceptMessageHeader {
    vp_index: u32,
    instruction_length: u8,
    intercept_access_type: u8,
    execution_state: u16,
    pc: u64,
    cpsr: u64,
}

#[cfg(target_arch = "aarch64")]
#[repr(C)]
#[derive(Clone, Copy)]
struct HvArm64MemoryInterceptMessage {
    header: HvArm64InterceptMessageHeader,
    cache_type: u32,
    instruction_byte_count: u8,
    memory_access_info: u8,
    reserved1: u16,
    instruction_bytes: [u8; 4],
    reserved2: u32,
    guest_virtual_address: u64,
    guest_physical_address: u64,
    syndrome: u64,
}

#[cfg(target_arch = "aarch64")]
#[repr(C)]
#[derive(Clone, Copy)]
struct WhvArm64ResetContext {
    header: HvArm64InterceptMessageHeader,
    reset_type: u32,
    reserved: u32,
}

#[derive(Clone, Copy, Debug)]
struct MappedMemoryRegion {
    guest_addr: u64,
    size: u64,
}

#[cfg(target_arch = "aarch64")]
#[repr(C)]
struct WhvArm64IcGicV3Parameters {
    gicd_base_address: u64,
    gits_translater_base_address: u64,
    reserved: u32,
    gic_lpi_int_id_bits: u32,
    gic_ppi_overflow_interrupt_from_cntv: u32,
    gic_ppi_performance_monitors_interrupt: u32,
    reserved1: [u32; 6],
}

#[cfg(target_arch = "aarch64")]
#[repr(C)]
struct WhvArm64IcParameters {
    emulation_mode: u32,
    reserved: u32,
    gic_v3_parameters: WhvArm64IcGicV3Parameters,
}

impl WhpCapabilities {
    pub fn probe() -> Result<Self> {
        Ok(Self {
            hypervisor_present: query_hypervisor_present()?,
        })
    }

    pub fn ensure_hypervisor_present() -> Result<Self> {
        let capabilities = Self::probe()?;
        if !capabilities.hypervisor_present {
            return Err(Error::HypervisorNotPresent);
        }

        Ok(capabilities)
    }
}

impl Vm {
    pub fn new(vcpu_count: u8) -> Result<Self> {
        WhpCapabilities::ensure_hypervisor_present()?;
        if vcpu_count == 0 {
            return Err(Error::VcpuCountZero);
        }

        let partition = Partition::new(vcpu_count)?;

        Ok(Self {
            partition,
            memory_regions: Vec::new(),
        })
    }

    pub fn memory_init(&mut self, guest_mem: &GuestMemoryMmap) -> Result<()> {
        for region in guest_mem.iter() {
            let guest_addr = region.start_addr().raw_value();
            let size = region.len();
            let host_addr = guest_mem
                .get_host_address(region.start_addr())
                .map_err(Error::GuestMemoryHostAddress)?;

            map_gpa_range(
                self.partition.handle,
                host_addr.cast_const(),
                guest_addr,
                size,
            )?;
            self.memory_regions
                .push(MappedMemoryRegion { guest_addr, size });
        }

        Ok(())
    }

    pub fn add_mapping(
        &self,
        reply_sender: Sender<bool>,
        host_addr: u64,
        guest_addr: u64,
        len: u64,
    ) {
        self.add_mapping_with_writable(reply_sender, host_addr, guest_addr, len, true);
    }

    pub fn add_mapping_with_writable(
        &self,
        reply_sender: Sender<bool>,
        host_addr: u64,
        guest_addr: u64,
        len: u64,
        writable: bool,
    ) {
        debug!("add_mapping: host_addr={host_addr:x}, guest_addr={guest_addr:x}, len={len}");

        if let Err(err) = unmap_gpa_range(self.partition.handle, guest_addr, len) {
            error!("{err}");
            reply_sender.send(false).unwrap();
            return;
        }

        let mut flags = WHvMapGpaRangeFlagRead;
        if writable {
            flags |= WHvMapGpaRangeFlagWrite;
        }
        if let Err(err) = map_gpa_range_with_flags(
            self.partition.handle,
            host_addr as *const u8,
            guest_addr,
            len,
            flags,
        ) {
            error!("{err}");
            reply_sender.send(false).unwrap();
        } else {
            reply_sender.send(true).unwrap();
        }
    }

    pub fn remove_mapping(&self, reply_sender: Sender<bool>, guest_addr: u64, len: u64) {
        debug!("remove_mapping: guest_addr={guest_addr:x}, len={len}");

        if let Err(err) = unmap_gpa_range(self.partition.handle, guest_addr, len) {
            error!("{err}");
            reply_sender.send(false).unwrap();
        } else {
            reply_sender.send(true).unwrap();
        }
    }

    pub fn partition_handle(&self) -> WHV_PARTITION_HANDLE {
        self.partition.handle
    }

    pub fn create_vcpu(&self, id: u8, exit_evt: EventFd, metrics: MetricsWriter) -> Result<Vcpu> {
        Vcpu::new(id, self.partition.handle, exit_evt, metrics)
    }
}

impl Vcpu {
    pub fn register_kick_signal_handler() {}

    pub fn new(
        id: u8,
        partition_handle: WHV_PARTITION_HANDLE,
        exit_evt: EventFd,
        metrics: MetricsWriter,
    ) -> Result<Self> {
        let hresult = unsafe { WHvCreateVirtualProcessor(partition_handle, id as u32, 0) };
        if hresult < 0 {
            return Err(Error::CreateVirtualProcessor { id, hresult });
        }

        let (event_sender, event_receiver) = unbounded();
        let (response_sender, response_receiver) = unbounded();

        Ok(Self {
            id,
            host_cpu: None,
            host_cpu_required: true,
            partition_handle,
            guest_mem: None,
            mmio_bus: None,
            #[cfg(target_arch = "x86_64")]
            pio_bus: None,
            #[cfg(target_arch = "x86_64")]
            sipi_router: None,
            exit_evt,
            metrics,
            event_sender: Some(event_sender),
            event_receiver: Some(event_receiver),
            response_sender: Some(response_sender),
            response_receiver: Some(response_receiver),
        })
    }

    pub fn set_mmio_bus(&mut self, mmio_bus: devices::Bus) {
        self.mmio_bus = Some(mmio_bus);
    }

    /// Selects the host processor group and logical processor for this vCPU thread.
    pub(crate) fn set_host_cpu(&mut self, host_cpu: HostCpuId, required: bool) {
        self.host_cpu = Some(host_cpu);
        self.host_cpu_required = required;
    }

    #[cfg(target_arch = "x86_64")]
    pub fn set_pio_bus(&mut self, pio_bus: devices::Bus) {
        self.pio_bus = Some(pio_bus);
    }

    #[cfg(target_arch = "x86_64")]
    pub fn set_sipi_router(&mut self, sipi_router: Arc<ApStartupRouter>) {
        self.sipi_router = Some(sipi_router);
    }

    #[cfg(target_arch = "aarch64")]
    pub fn configure_windows(
        &mut self,
        guest_mem: &GuestMemoryMmap,
        mem_info: &ArchMemoryInfo,
        entry_addr: GuestAddress,
    ) -> Result<()> {
        self.guest_mem = Some(guest_mem.clone());
        self.configure_aarch64(entry_addr, mem_info.fdt_addr)
    }

    #[cfg(target_arch = "x86_64")]
    pub fn configure_windows(
        &mut self,
        guest_mem: &GuestMemoryMmap,
        _mem_info: &ArchMemoryInfo,
        entry_addr: GuestAddress,
    ) -> Result<()> {
        self.guest_mem = Some(guest_mem.clone());
        self.configure_x86_64(guest_mem, entry_addr)
    }

    pub fn start_threaded(mut self) -> Result<(VcpuHandle, VcpuPlacementResult)> {
        let event_sender = self
            .event_sender
            .take()
            .expect("event sender missing before vcpu start");
        let event_receiver = self
            .event_receiver
            .take()
            .expect("event receiver missing before vcpu start");
        let response_sender = self
            .response_sender
            .take()
            .expect("response sender missing before vcpu start");
        let response_receiver = self
            .response_receiver
            .take()
            .expect("response receiver missing before vcpu start");

        let id = self.id;
        let host_cpu = self.host_cpu;
        let host_cpu_required = self.host_cpu_required;
        let partition_handle = self.partition_handle;
        let guest_mem = self
            .guest_mem
            .take()
            .expect("guest memory missing before vcpu start");
        let mmio_bus = self.mmio_bus.take();
        #[cfg(target_arch = "x86_64")]
        let pio_bus = self.pio_bus.take();
        #[cfg(target_arch = "x86_64")]
        let sipi_router = self.sipi_router.take();
        let exit_evt = self.exit_evt.try_clone().map_err(Error::VcpuThreadSpawn)?;
        let metrics = self.metrics.clone();
        self.partition_handle = 0;
        let (init_sender, init_receiver) = unbounded();

        let vcpu_thread = thread::Builder::new()
            .name(format!("whp-vcpu-{id}"))
            .spawn(move || {
                let init_result = apply_host_cpu_affinity(host_cpu, host_cpu_required, id);
                let initialized = init_result.is_ok();
                init_sender
                    .send(init_result)
                    .expect("cannot notify WHP vCPU initialization");

                if initialized {
                    run_vcpu(
                        id,
                        partition_handle,
                        guest_mem,
                        mmio_bus,
                        #[cfg(target_arch = "x86_64")]
                        pio_bus,
                        #[cfg(target_arch = "x86_64")]
                        sipi_router,
                        exit_evt,
                        metrics,
                        event_receiver,
                        response_sender,
                    );
                } else {
                    // Ownership moved out of `Vcpu` before the thread was spawned. If affinity
                    // initialization fails, release the WHP processor here instead of leaking it.
                    let hresult = unsafe {
                        WHvDeleteVirtualProcessor(partition_handle, id as u32)
                    };
                    if hresult < 0 {
                        error!(
                            "WHvDeleteVirtualProcessor({id}) after affinity failure returned HRESULT 0x{:08x}",
                            hresult as u32
                        );
                    }
                }
            })
            .map_err(Error::VcpuThreadSpawn)?;

        // Affinity is part of VM correctness: fail construction before any vCPU can execute if
        // Windows rejects a processor-group coordinate or the inherited host constraints.
        let placement = init_receiver
            .recv()
            .expect("error waiting for WHP vCPU initialization")?;

        Ok((
            VcpuHandle::new(
                id,
                partition_handle,
                event_sender,
                response_receiver,
                vcpu_thread,
            ),
            placement,
        ))
    }

    pub fn cpu_index(&self) -> u8 {
        self.id
    }

    #[cfg(target_arch = "aarch64")]
    pub fn get_mpidr(&self) -> u64 {
        AARCH64_MPIDR_RES1_BIT | AARCH64_MPIDR_U_BIT | u64::from(self.id)
    }

    #[cfg(target_arch = "aarch64")]
    fn configure_aarch64(&mut self, entry_addr: GuestAddress, fdt_addr: u64) -> Result<()> {
        let mut names = vec![
            WHV_ARM64_REGISTER_PSTATE,
            WHV_ARM64_REGISTER_MPIDR_EL1,
            WHV_ARM64_REGISTER_GICR_BASE_GPA,
        ];
        let mut values = vec![
            register_value_u64(AARCH64_PSTATE_FAULT_BITS_64),
            register_value_u64(self.get_mpidr()),
            register_value_u64(WHP_GICR_BASE + (u64::from(self.id) * WHP_GICR_SIZE)),
        ];

        if self.id == 0 {
            names.push(WHV_ARM64_REGISTER_PC);
            values.push(register_value_u64(entry_addr.raw_value()));
            names.push(WHV_ARM64_REGISTER_X0);
            values.push(register_value_u64(fdt_addr));
        }

        set_vcpu_registers(self.partition_handle, self.id, &names, &values)
    }

    #[cfg(target_arch = "x86_64")]
    fn configure_x86_64(
        &mut self,
        guest_mem: &GuestMemoryMmap,
        entry_addr: GuestAddress,
    ) -> Result<()> {
        // Only the BSP boots at the kernel entry point. APs keep the
        // architectural reset state and stay parked until the guest sends
        // them INIT/SIPI (see `ApStartupRouter`).
        if self.id != 0 {
            return Ok(());
        }

        write_x86_gdt_table(guest_mem)?;
        write_x86_idt_value(guest_mem)?;
        setup_x86_page_tables(guest_mem)?;

        let code_seg = x86_segment(0, 0xfffff, 0x8, 0xa09b);
        let data_seg = x86_segment(0, 0xfffff, 0x10, 0xc093);
        let tss_seg = x86_segment(0, 0xfffff, 0x18, 0x808b);
        let gdt = x86_table(
            X86_BOOT_GDT_OFFSET,
            (X86_BOOT_GDT_MAX * size_of::<u64>() - 1) as u16,
        );
        let idt = x86_table(X86_BOOT_IDT_OFFSET, size_of::<u64>() as u16 - 1);

        let names = [
            WHvX64RegisterRip,
            WHvX64RegisterRflags,
            WHvX64RegisterRsp,
            WHvX64RegisterRbp,
            WHvX64RegisterRsi,
            WHvX64RegisterCr0,
            WHvX64RegisterCr3,
            WHvX64RegisterCr4,
            WHvX64RegisterEfer,
            WHvX64RegisterCs,
            WHvX64RegisterDs,
            WHvX64RegisterEs,
            WHvX64RegisterFs,
            WHvX64RegisterGs,
            WHvX64RegisterSs,
            WHvX64RegisterTr,
            WHvX64RegisterGdtr,
            WHvX64RegisterIdtr,
        ];
        let values = [
            register_value_u64(entry_addr.raw_value()),
            register_value_u64(0x2),
            register_value_u64(arch::x86_64::layout::BOOT_STACK_POINTER),
            register_value_u64(arch::x86_64::layout::BOOT_STACK_POINTER),
            register_value_u64(arch::x86_64::layout::ZERO_PAGE_START),
            register_value_u64(X86_CR0_PE | X86_CR0_PG),
            register_value_u64(X86_PML4_START),
            register_value_u64(X86_CR4_PAE),
            register_value_u64(X86_EFER_LME | X86_EFER_LMA),
            register_value_segment(code_seg),
            register_value_segment(data_seg),
            register_value_segment(data_seg),
            register_value_segment(data_seg),
            register_value_segment(data_seg),
            register_value_segment(data_seg),
            register_value_segment(tss_seg),
            register_value_table(gdt),
            register_value_table(idt),
        ];

        set_vcpu_registers(self.partition_handle, self.id, &names, &values)
    }
}

//--------------------------------------------------------------------------------------------------
// Functions: Host CPU Affinity
//--------------------------------------------------------------------------------------------------

fn apply_host_cpu_affinity(
    host_cpu: Option<HostCpuId>,
    required: bool,
    vcpu_id: u8,
) -> Result<VcpuPlacementResult> {
    let Some(host_cpu) = host_cpu else {
        return Ok(VcpuPlacementResult::Inherited {
            vcpu_index: vcpu_id,
            requested_host_cpu: None,
            reason: None,
        });
    };
    if let Err(error) = apply_required_host_cpu_affinity(host_cpu) {
        if required {
            return Err(error);
        }
        let reason = error.to_string();
        warn!(
            "WHP vCPU {vcpu_id} host affinity unavailable; continuing with scheduler placement: {error}"
        );
        return Ok(VcpuPlacementResult::Inherited {
            vcpu_index: vcpu_id,
            requested_host_cpu: Some(host_cpu),
            reason: Some(reason),
        });
    }
    Ok(VcpuPlacementResult::Pinned {
        vcpu_index: vcpu_id,
        host_cpu,
    })
}

fn apply_required_host_cpu_affinity(host_cpu: HostCpuId) -> Result<()> {
    let affinity = group_affinity(host_cpu).map_err(Error::VcpuAffinity)?;

    // SAFETY: the pseudo-handle refers to this vCPU thread, `affinity` is initialized and remains
    // live for the call, and a null previous-affinity pointer is explicitly accepted by Windows.
    let result =
        unsafe { SetThreadGroupAffinity(GetCurrentThread(), &affinity, std::ptr::null_mut()) };
    if result == 0 {
        return Err(Error::VcpuAffinity(std::io::Error::last_os_error()));
    }
    Ok(())
}

fn group_affinity(host_cpu: HostCpuId) -> std::io::Result<GROUP_AFFINITY> {
    let index = u32::from(host_cpu.index);
    if index >= usize::BITS {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "host CPU {}:{} exceeds the {}-processor group width",
                host_cpu.group,
                host_cpu.index,
                usize::BITS
            ),
        ));
    }

    Ok(GROUP_AFFINITY {
        Mask: 1usize << index,
        Group: host_cpu.group,
        Reserved: [0; 3],
    })
}

impl VcpuHandle {
    pub fn new(
        id: u8,
        partition_handle: WHV_PARTITION_HANDLE,
        event_sender: Sender<VcpuEvent>,
        response_receiver: Receiver<VcpuResponse>,
        vcpu_thread: thread::JoinHandle<()>,
    ) -> Self {
        Self {
            id,
            partition_handle,
            event_sender: Some(event_sender),
            response_receiver,
            vcpu_thread: Some(vcpu_thread),
        }
    }

    pub fn send_event(&self, event: VcpuEvent) -> Result<()> {
        let should_cancel = matches!(event, VcpuEvent::Pause);
        self.event_sender
            .as_ref()
            .expect("vCPU event sender missing before handle drop")
            .send(event)
            .expect("event sender channel closed on vcpu end.");
        if should_cancel {
            cancel_run_virtual_processor(self.partition_handle, self.id)?;
        }
        Ok(())
    }

    pub fn response_receiver(&self) -> &Receiver<VcpuResponse> {
        &self.response_receiver
    }
}

impl Drop for VcpuHandle {
    fn drop(&mut self) {
        if self.partition_handle != 0 {
            if let Err(err) = cancel_run_virtual_processor(self.partition_handle, self.id) {
                error!("{err}");
            }
            // Disconnect the control channel and wait for the vCPU loop to observe cancellation
            // before deleting its WHP processor. This is required when a later vCPU fails the
            // startup placement barrier and already-created processors unwind.
            self.event_sender.take();
            if let Some(thread) = self.vcpu_thread.take() {
                let _ = thread.join();
            }
            let hresult =
                unsafe { WHvDeleteVirtualProcessor(self.partition_handle, self.id as u32) };
            if hresult < 0 {
                error!(
                    "WHvDeleteVirtualProcessor({}) failed with HRESULT 0x{:08x}",
                    self.id, hresult as u32
                );
            }
        }
    }
}

impl Drop for Partition {
    fn drop(&mut self) {
        if self.handle != 0 {
            unsafe {
                WHvDeletePartition(self.handle);
            }
        }
    }
}

impl Drop for Vm {
    fn drop(&mut self) {
        while let Some(region) = self.memory_regions.pop() {
            let hresult =
                unsafe { WHvUnmapGpaRange(self.partition.handle, region.guest_addr, region.size) };
            if hresult < 0 {
                error!(
                    "WHvUnmapGpaRange(guest_addr=0x{:x}, size=0x{:x}) failed with HRESULT 0x{:08x}",
                    region.guest_addr, region.size, hresult as u32
                );
            }
        }
    }
}

impl Drop for Vcpu {
    fn drop(&mut self) {
        if self.partition_handle != 0 {
            let hresult =
                unsafe { WHvDeleteVirtualProcessor(self.partition_handle, self.id as u32) };
            if hresult < 0 {
                error!(
                    "WHvDeleteVirtualProcessor({}) failed with HRESULT 0x{:08x}",
                    self.id, hresult as u32
                );
            }
        }
    }
}

#[cfg(target_arch = "x86_64")]
impl Emulator {
    fn new() -> Result<Self> {
        let library = unsafe { libloading::Library::new("WinHvEmulation.dll") }
            .map_err(Error::LoadEmulator)?;
        let create_emulator: WhvEmulatorCreateEmulator = unsafe {
            *library.get(b"WHvEmulatorCreateEmulator").map_err(|error| {
                Error::LoadEmulatorSymbol {
                    symbol: "WHvEmulatorCreateEmulator",
                    error,
                }
            })?
        };
        let destroy_emulator: WhvEmulatorDestroyEmulator = unsafe {
            *library
                .get(b"WHvEmulatorDestroyEmulator")
                .map_err(|error| Error::LoadEmulatorSymbol {
                    symbol: "WHvEmulatorDestroyEmulator",
                    error,
                })?
        };
        let try_mmio_emulation: WhvEmulatorTryMmioEmulation = unsafe {
            *library
                .get(b"WHvEmulatorTryMmioEmulation")
                .map_err(|error| Error::LoadEmulatorSymbol {
                    symbol: "WHvEmulatorTryMmioEmulation",
                    error,
                })?
        };
        let try_io_emulation: WhvEmulatorTryIoEmulation = unsafe {
            *library.get(b"WHvEmulatorTryIoEmulation").map_err(|error| {
                Error::LoadEmulatorSymbol {
                    symbol: "WHvEmulatorTryIoEmulation",
                    error,
                }
            })?
        };

        let callbacks = WHV_EMULATOR_CALLBACKS {
            Size: size_of::<WHV_EMULATOR_CALLBACKS>() as u32,
            Reserved: 0,
            WHvEmulatorIoPortCallback: Some(emulator_io_port_callback),
            WHvEmulatorMemoryCallback: Some(emulator_memory_callback),
            WHvEmulatorGetVirtualProcessorRegisters: Some(emulator_get_registers_callback),
            WHvEmulatorSetVirtualProcessorRegisters: Some(emulator_set_registers_callback),
            WHvEmulatorTranslateGvaPage: Some(emulator_translate_gva_callback),
        };
        let mut handle = std::ptr::null_mut();
        let hresult = unsafe { create_emulator(&callbacks, &mut handle) };
        if hresult < 0 {
            return Err(Error::CreateEmulator(hresult));
        }

        Ok(Self {
            _library: library,
            handle,
            destroy_emulator,
            try_mmio_emulation,
            try_io_emulation,
        })
    }

    fn emulate_mmio(
        &self,
        context: &mut EmulatorContext,
        exit_context: &WHV_RUN_VP_EXIT_CONTEXT,
        memory_access: &WHV_MEMORY_ACCESS_CONTEXT,
    ) -> Result<()> {
        let mut status = WHV_EMULATOR_STATUS { AsUINT32: 0 };
        let hresult = unsafe {
            (self.try_mmio_emulation)(
                self.handle,
                context as *mut EmulatorContext as *const _,
                &exit_context.VpContext,
                memory_access,
                &mut status,
            )
        };
        let status = unsafe { status.AsUINT32 };
        if hresult < 0 || status & WHV_EMULATOR_STATUS_SUCCESS == 0 {
            error!(
                "WHP MMIO emulation failed: vcpu={} gpa=0x{:x} gva=0x{:x} access_info=0x{:x} hresult=0x{:08x} status=0x{status:08x}",
                context.id,
                memory_access.Gpa,
                memory_access.Gva,
                unsafe { memory_access.AccessInfo.AsUINT32 },
                hresult as u32,
            );
            return Err(Error::EmulatorMmio { hresult, status });
        }

        Ok(())
    }

    fn emulate_io(
        &self,
        context: &mut EmulatorContext,
        exit_context: &WHV_RUN_VP_EXIT_CONTEXT,
        io_access: &WHV_X64_IO_PORT_ACCESS_CONTEXT,
    ) -> Result<()> {
        let mut status = WHV_EMULATOR_STATUS { AsUINT32: 0 };
        let hresult = unsafe {
            (self.try_io_emulation)(
                self.handle,
                context as *mut EmulatorContext as *const _,
                &exit_context.VpContext,
                io_access,
                &mut status,
            )
        };
        let status = unsafe { status.AsUINT32 };
        if hresult < 0 || status & WHV_EMULATOR_STATUS_SUCCESS == 0 {
            error!(
                "WHP I/O emulation failed: vcpu={} port=0x{:x} access_info=0x{:x} rax=0x{:x} hresult=0x{:08x} status=0x{status:08x}",
                context.id,
                io_access.PortNumber,
                unsafe { io_access.AccessInfo.AsUINT32 },
                io_access.Rax,
                hresult as u32,
            );
            return Err(Error::EmulatorIo { hresult, status });
        }

        Ok(())
    }
}

#[cfg(target_arch = "x86_64")]
impl Drop for Emulator {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            let hresult = unsafe { (self.destroy_emulator)(self.handle) };
            if hresult < 0 {
                error!("{}", Error::DestroyEmulator(hresult));
            }
        }
    }
}

impl Partition {
    fn new(vcpu_count: u8) -> Result<Self> {
        let mut handle = 0;
        let hresult = unsafe { WHvCreatePartition(&mut handle) };
        if hresult < 0 {
            return Err(Error::CreatePartition(hresult));
        }

        let partition = Self { handle };
        partition.set_processor_count(vcpu_count)?;
        #[cfg(target_arch = "x86_64")]
        partition.set_x64_local_apic_emulation()?;
        #[cfg(target_arch = "x86_64")]
        partition.set_x64_extended_vm_exits(vcpu_count)?;
        #[cfg(target_arch = "aarch64")]
        partition.set_arm64_ic_parameters()?;
        partition.setup()?;

        Ok(partition)
    }

    fn set_processor_count(&self, vcpu_count: u8) -> Result<()> {
        let property = vcpu_count as u32;
        let property_code = WHvPartitionPropertyCodeProcessorCount;
        let hresult = unsafe {
            WHvSetPartitionProperty(
                self.handle,
                property_code,
                &property as *const u32 as *const _,
                size_of::<u32>() as u32,
            )
        };
        if hresult < 0 {
            return Err(Error::SetPartitionProperty {
                property: property_code,
                hresult,
            });
        }

        Ok(())
    }

    /// INIT/SIPI writes to the emulated local APIC are only surfaced to the
    /// VMM (as `X64ApicInitSipiTrap` exits) when the corresponding extended
    /// VM exit is enabled; without it, application processors can never be
    /// started. CPUID exits let the VMM answer with a normalized result so
    /// guest boot does not depend on the host CPU or the host's WHP build.
    /// Bit 0 = X64CpuidExit, bit 6 = X64ApicInitSipiExitTrap
    /// (WinHvPlatformDefs.h); windows-sys models WHV_EXTENDED_VM_EXITS as an
    /// opaque u64 bitfield.
    #[cfg(target_arch = "x86_64")]
    fn set_x64_extended_vm_exits(&self, vcpu_count: u8) -> Result<()> {
        const X64_CPUID_EXIT: u64 = 1 << 0;
        const X64_APIC_INIT_SIPI_EXIT_TRAP: u64 = 1 << 6;

        let supported = query_extended_vm_exits()?;
        let mut property: u64 = 0;

        if supported & X64_CPUID_EXIT != 0 {
            property |= X64_CPUID_EXIT;
        } else {
            warn!("WHP: CPUID exits unsupported; the guest sees host CPUID unnormalized");
        }

        if supported & X64_APIC_INIT_SIPI_EXIT_TRAP != 0 {
            property |= X64_APIC_INIT_SIPI_EXIT_TRAP;
        } else if vcpu_count > 1 {
            // Single-CPU guests never send startup IPIs, so this host can
            // still run them; refuse multi-CPU rather than hang at boot.
            return Err(Error::ApicInitSipiTrapUnsupported);
        }

        if property == 0 {
            return Ok(());
        }

        let property_code = WHvPartitionPropertyCodeExtendedVmExits;
        let hresult = unsafe {
            WHvSetPartitionProperty(
                self.handle,
                property_code,
                &property as *const u64 as *const _,
                size_of::<u64>() as u32,
            )
        };
        if hresult < 0 {
            return Err(Error::SetPartitionProperty {
                property: property_code,
                hresult,
            });
        }

        Ok(())
    }

    #[cfg(target_arch = "x86_64")]
    fn set_x64_local_apic_emulation(&self) -> Result<()> {
        let property = WHvX64LocalApicEmulationModeXApic;
        let property_code = WHvPartitionPropertyCodeLocalApicEmulationMode;
        let hresult = unsafe {
            WHvSetPartitionProperty(
                self.handle,
                property_code,
                &property as *const _ as *const _,
                size_of::<i32>() as u32,
            )
        };
        if hresult < 0 {
            return Err(Error::SetPartitionProperty {
                property: property_code,
                hresult,
            });
        }

        Ok(())
    }

    #[cfg(target_arch = "aarch64")]
    fn set_arm64_ic_parameters(&self) -> Result<()> {
        let property = WhvArm64IcParameters {
            emulation_mode: WHV_ARM64_IC_EMULATION_MODE_GIC_V3,
            reserved: 0,
            gic_v3_parameters: WhvArm64IcGicV3Parameters {
                gicd_base_address: WHP_GICD_BASE,
                gits_translater_base_address: WHP_GITS_TRANSLATER_BASE,
                reserved: 0,
                gic_lpi_int_id_bits: WHP_GIC_LPI_INT_ID_BITS,
                gic_ppi_overflow_interrupt_from_cntv: arch::aarch64::layout::VTIMER_IRQ,
                gic_ppi_performance_monitors_interrupt: WHP_GIC_PPI_PERFORMANCE_MONITORS_INTERRUPT,
                reserved1: [0; 6],
            },
        };
        let property_code = WHV_PARTITION_PROPERTY_CODE_ARM64_IC_PARAMETERS;
        let hresult = unsafe {
            WHvSetPartitionProperty(
                self.handle,
                property_code,
                &property as *const WhvArm64IcParameters as *const _,
                size_of::<WhvArm64IcParameters>() as u32,
            )
        };
        if hresult < 0 {
            return Err(Error::SetPartitionProperty {
                property: property_code,
                hresult,
            });
        }

        Ok(())
    }

    fn setup(&self) -> Result<()> {
        let hresult = unsafe { WHvSetupPartition(self.handle) };
        if hresult < 0 {
            return Err(Error::SetupPartition(hresult));
        }

        Ok(())
    }
}

#[cfg(target_arch = "x86_64")]
fn query_extended_vm_exits() -> Result<u64> {
    let mut capability: WHV_CAPABILITY = unsafe { zeroed() };
    let mut written_size = 0;
    let code = WHvCapabilityCodeExtendedVmExits;
    let hresult = unsafe {
        WHvGetCapability(
            code,
            &mut capability as *mut WHV_CAPABILITY as *mut _,
            size_of::<WHV_CAPABILITY>() as u32,
            &mut written_size,
        )
    };

    if hresult < 0 {
        return Err(Error::GetCapability { code, hresult });
    }

    Ok(unsafe { capability.ExtendedVmExits.Anonymous._bitfield })
}

fn query_hypervisor_present() -> Result<bool> {
    let mut capability: WHV_CAPABILITY = unsafe { zeroed() };
    let mut written_size = 0;
    let code = WHvCapabilityCodeHypervisorPresent;
    let hresult = unsafe {
        WHvGetCapability(
            code,
            &mut capability as *mut WHV_CAPABILITY as *mut _,
            size_of::<WHV_CAPABILITY>() as u32,
            &mut written_size,
        )
    };

    if hresult < 0 {
        return Err(Error::GetCapability { code, hresult });
    }

    Ok(unsafe { capability.HypervisorPresent != 0 })
}

fn map_gpa_range(
    partition: WHV_PARTITION_HANDLE,
    host_addr: *const u8,
    guest_addr: u64,
    size: u64,
) -> Result<()> {
    let flags = WHvMapGpaRangeFlagRead | WHvMapGpaRangeFlagWrite | WHvMapGpaRangeFlagExecute;
    map_gpa_range_with_flags(partition, host_addr, guest_addr, size, flags)
}

fn map_gpa_range_with_flags(
    partition: WHV_PARTITION_HANDLE,
    host_addr: *const u8,
    guest_addr: u64,
    size: u64,
    flags: WHV_MAP_GPA_RANGE_FLAGS,
) -> Result<()> {
    let hresult = unsafe { WHvMapGpaRange(partition, host_addr.cast(), guest_addr, size, flags) };

    if hresult < 0 {
        return Err(Error::MapGpaRange {
            guest_addr,
            size,
            hresult,
        });
    }

    Ok(())
}

fn unmap_gpa_range(partition: WHV_PARTITION_HANDLE, guest_addr: u64, size: u64) -> Result<()> {
    let hresult = unsafe { WHvUnmapGpaRange(partition, guest_addr, size) };
    if hresult < 0 {
        return Err(Error::UnmapGpaRange {
            guest_addr,
            size,
            hresult,
        });
    }

    Ok(())
}

fn set_vcpu_registers(
    partition_handle: WHV_PARTITION_HANDLE,
    id: u8,
    names: &[WHV_REGISTER_NAME],
    values: &[WHV_REGISTER_VALUE],
) -> Result<()> {
    debug_assert_eq!(names.len(), values.len());
    let aligned_values: Vec<AlignedRegisterValue> =
        values.iter().copied().map(AlignedRegisterValue).collect();
    debug_assert_eq!(
        size_of::<AlignedRegisterValue>(),
        size_of::<WHV_REGISTER_VALUE>()
    );

    let hresult = unsafe {
        WHvSetVirtualProcessorRegisters(
            partition_handle,
            id as u32,
            names.as_ptr(),
            names.len() as u32,
            aligned_values.as_ptr().cast(),
        )
    };
    if hresult < 0 {
        return Err(Error::SetVirtualProcessorRegisters { id, hresult });
    }

    Ok(())
}

fn get_vcpu_register_u64(
    partition_handle: WHV_PARTITION_HANDLE,
    id: u8,
    name: WHV_REGISTER_NAME,
) -> Result<u64> {
    let mut value = WHV_REGISTER_VALUE { Reg64: 0 };
    let hresult = unsafe {
        WHvGetVirtualProcessorRegisters(partition_handle, id.into(), &name, 1, &mut value)
    };
    if hresult < 0 {
        return Err(Error::GetVirtualProcessorRegisters { id, hresult });
    }

    Ok(unsafe { value.Reg64 })
}

fn register_value_u64(value: u64) -> WHV_REGISTER_VALUE {
    let mut register = WHV_REGISTER_VALUE::default();
    register.Reg64 = value;
    register
}

#[cfg(target_arch = "x86_64")]
fn register_value_segment(value: WHV_X64_SEGMENT_REGISTER) -> WHV_REGISTER_VALUE {
    let mut register = WHV_REGISTER_VALUE::default();
    register.Segment = value;
    register
}

#[cfg(target_arch = "x86_64")]
fn register_value_table(value: WHV_X64_TABLE_REGISTER) -> WHV_REGISTER_VALUE {
    let mut register = WHV_REGISTER_VALUE::default();
    register.Table = value;
    register
}

#[cfg(target_arch = "x86_64")]
fn write_x86_gdt_table(guest_mem: &GuestMemoryMmap) -> Result<()> {
    let table: [u64; X86_BOOT_GDT_MAX] = [
        0,
        0x00af_9b00_0000_ffff,
        0x00cf_9300_0000_ffff,
        0x008f_8b00_0000_ffff,
    ];

    for (index, entry) in table.iter().enumerate() {
        let addr = GuestAddress(X86_BOOT_GDT_OFFSET + (index * size_of::<u64>()) as u64);
        guest_mem
            .write_obj(*entry, addr)
            .map_err(|_| Error::GuestMemoryWrite("GDT"))?;
    }

    Ok(())
}

#[cfg(target_arch = "x86_64")]
fn write_x86_idt_value(guest_mem: &GuestMemoryMmap) -> Result<()> {
    guest_mem
        .write_obj(0u64, GuestAddress(X86_BOOT_IDT_OFFSET))
        .map_err(|_| Error::GuestMemoryWrite("IDT"))
}

#[cfg(target_arch = "x86_64")]
fn setup_x86_page_tables(guest_mem: &GuestMemoryMmap) -> Result<()> {
    guest_mem
        .write_obj(X86_PDPTE_START | 0x03, GuestAddress(X86_PML4_START))
        .map_err(|_| Error::GuestMemoryWrite("PML4"))?;
    guest_mem
        .write_obj(X86_PDE_START | 0x03, GuestAddress(X86_PDPTE_START))
        .map_err(|_| Error::GuestMemoryWrite("PDPTE"))?;

    for index in 0..512 {
        let entry = (index << 21) + 0x83u64;
        let addr = GuestAddress(X86_PDE_START + index * size_of::<u64>() as u64);
        guest_mem
            .write_obj(entry, addr)
            .map_err(|_| Error::GuestMemoryWrite("PDE"))?;
    }

    Ok(())
}

#[cfg(target_arch = "x86_64")]
fn x86_segment(base: u64, limit: u32, selector: u16, attributes: u16) -> WHV_X64_SEGMENT_REGISTER {
    WHV_X64_SEGMENT_REGISTER {
        Base: base,
        Limit: limit,
        Selector: selector,
        Anonymous: WHV_X64_SEGMENT_REGISTER_0 {
            Attributes: attributes,
        },
    }
}

#[cfg(target_arch = "x86_64")]
fn x86_table(base: u64, limit: u16) -> WHV_X64_TABLE_REGISTER {
    WHV_X64_TABLE_REGISTER {
        Pad: [0; 3],
        Limit: limit,
        Base: base,
    }
}

#[allow(clippy::too_many_arguments)]
fn run_vcpu(
    id: u8,
    partition_handle: WHV_PARTITION_HANDLE,
    guest_mem: GuestMemoryMmap,
    mmio_bus: Option<devices::Bus>,
    #[cfg(target_arch = "x86_64")] pio_bus: Option<devices::Bus>,
    #[cfg(target_arch = "x86_64")] sipi_router: Option<Arc<ApStartupRouter>>,
    exit_evt: EventFd,
    metrics: MetricsWriter,
    event_receiver: Receiver<VcpuEvent>,
    response_sender: Sender<VcpuResponse>,
) {
    if wait_until_resumed(&event_receiver, &response_sender).is_err() {
        return;
    }

    // x86 APs must not execute until the guest starts them: park here until
    // the BSP's startup IPI supplies the real-mode entry vector.
    #[cfg(target_arch = "x86_64")]
    if id != 0 {
        if let Some(router) = &sipi_router {
            match router.wait_for_sipi(id, &event_receiver, &response_sender) {
                Ok(vector) => {
                    if let Err(err) = apply_sipi_startup_state(partition_handle, id, vector) {
                        error!("{err}");
                        signal_vcpu_exit(&response_sender, &exit_evt, 1);
                        return;
                    }
                    spawn_ap_probe(partition_handle, id);
                }
                Err(()) => return,
            }
        }
    }

    #[cfg(target_arch = "aarch64")]
    let _ = &guest_mem;
    #[cfg(target_arch = "aarch64")]
    let _trace_guard = Arm64TraceCancelGuard::new(partition_handle, id);

    let mut emulator_context = EmulatorContext {
        id,
        partition_handle,
        #[cfg(target_arch = "x86_64")]
        guest_mem,
        mmio_bus,
        #[cfg(target_arch = "x86_64")]
        pio_bus,
    };
    #[cfg(target_arch = "x86_64")]
    let emulator = match Emulator::new() {
        Ok(emulator) => emulator,
        Err(err) => {
            error!("{err}");
            signal_vcpu_exit(&response_sender, &exit_evt, 1);
            return;
        }
    };

    #[cfg(target_arch = "aarch64")]
    let mut exit_context = WhvArm64RunVpExitContext::default();
    #[cfg(target_arch = "x86_64")]
    let mut exit_context = WHV_RUN_VP_EXIT_CONTEXT::default();
    loop {
        match event_receiver.try_recv() {
            Ok(VcpuEvent::Pause) => {
                let _ = response_sender.send(VcpuResponse::Paused);
                if wait_until_resumed(&event_receiver, &response_sender).is_err() {
                    return;
                }
            }
            Ok(VcpuEvent::Resume) => {
                let _ = response_sender.send(VcpuResponse::Resumed);
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => return,
        }

        #[cfg(target_arch = "aarch64")]
        let exit_context_ptr =
            &mut exit_context as *mut WhvArm64RunVpExitContext as *mut core::ffi::c_void;
        #[cfg(target_arch = "x86_64")]
        let exit_context_ptr =
            &mut exit_context as *mut WHV_RUN_VP_EXIT_CONTEXT as *mut core::ffi::c_void;
        #[cfg(target_arch = "aarch64")]
        let exit_context_size = size_of::<WhvArm64RunVpExitContext>() as u32;
        #[cfg(target_arch = "x86_64")]
        let exit_context_size = size_of::<WHV_RUN_VP_EXIT_CONTEXT>() as u32;

        let cpu_time_before = current_thread_cpu_time_ns();
        let hresult = unsafe {
            WHvRunVirtualProcessor(
                partition_handle,
                id as u32,
                exit_context_ptr,
                exit_context_size,
            )
        };
        if let (Some(before), Some(after)) = (cpu_time_before, current_thread_cpu_time_ns()) {
            metrics.add_vcpu_time_ns(after.saturating_sub(before));
        }
        if hresult < 0 {
            error!("{}", Error::RunVirtualProcessor { id, hresult });
            signal_vcpu_exit(&response_sender, &exit_evt, 1);
            return;
        }

        #[cfg(target_arch = "aarch64")]
        let reason = exit_context.exit_reason;
        #[cfg(target_arch = "x86_64")]
        let reason = exit_context.ExitReason;
        if reason == WHvRunVpExitReasonNone {
            continue;
        }
        if reason == whp_canceled_exit_reason() {
            #[cfg(target_arch = "aarch64")]
            trace_arm64_vcpu_pulse(partition_handle, id);
            continue;
        }

        #[cfg(target_arch = "x86_64")]
        if reason == WHvRunVpExitReasonX64Halt {
            signal_vcpu_exit(&response_sender, &exit_evt, 0);
            return;
        }

        #[cfg(target_arch = "aarch64")]
        if reason == WHV_ARM64_EXIT_REASON_UNMAPPED_GPA
            || reason == WHV_ARM64_EXIT_REASON_GPA_INTERCEPT
        {
            let memory_access = exit_context.arm64_memory_access();
            if let Err(err) = handle_arm64_mmio(&mut emulator_context, memory_access) {
                error!("{err}");
                signal_vcpu_exit(&response_sender, &exit_evt, 1);
                return;
            }
            continue;
        }

        #[cfg(target_arch = "aarch64")]
        if reason == WHV_ARM64_EXIT_REASON_RESET {
            let reset = exit_context.arm64_reset();
            let exit_code = match reset.reset_type {
                WHV_ARM64_RESET_TYPE_POWER_OFF => 0,
                WHV_ARM64_RESET_TYPE_REBOOT => 1,
                _ => 1,
            };
            if reset.reset_type == WHV_ARM64_RESET_TYPE_POWER_OFF
                || reset.reset_type == WHV_ARM64_RESET_TYPE_REBOOT
            {
                info!(
                    "WHP ARM64 reset exit on vCPU {id}: reset_type={}, exit_code={exit_code}",
                    reset.reset_type
                );
            } else {
                error!(
                    "WHP ARM64 reset exit on vCPU {id}: reset_type={}, exit_code={exit_code}",
                    reset.reset_type
                );
            }
            signal_vcpu_exit(&response_sender, &exit_evt, exit_code);
            return;
        }

        #[cfg(target_arch = "x86_64")]
        if reason == WHvRunVpExitReasonMemoryAccess {
            let memory_access = unsafe { exit_context.Anonymous.MemoryAccess };
            if let Err(err) =
                emulator.emulate_mmio(&mut emulator_context, &exit_context, &memory_access)
            {
                error!("{err}");
                signal_vcpu_exit(&response_sender, &exit_evt, 1);
                return;
            }
            continue;
        }

        #[cfg(target_arch = "x86_64")]
        if reason == WHvRunVpExitReasonX64Cpuid {
            let cpuid = unsafe { exit_context.Anonymous.CpuidAccess };
            let vcpu_count = sipi_router.as_ref().map_or(1, |router| router.vcpu_count());
            let result = normalize_cpuid(id, vcpu_count, &cpuid);
            // The VpContext bitfield's low nibble is the instruction length.
            let next_rip =
                exit_context.VpContext.Rip + u64::from(exit_context.VpContext._bitfield & 0xf);
            let names = [
                WHvX64RegisterRax,
                WHvX64RegisterRbx,
                WHvX64RegisterRcx,
                WHvX64RegisterRdx,
                WHvX64RegisterRip,
            ];
            let values = [
                register_value_u64(result.rax),
                register_value_u64(result.rbx),
                register_value_u64(result.rcx),
                register_value_u64(result.rdx),
                register_value_u64(next_rip),
            ];
            if let Err(err) = set_vcpu_registers(partition_handle, id, &names, &values) {
                error!("{err}");
                signal_vcpu_exit(&response_sender, &exit_evt, 1);
                return;
            }
            continue;
        }

        #[cfg(target_arch = "x86_64")]
        if reason == WHvRunVpExitReasonX64ApicInitSipiTrap {
            let icr = unsafe { exit_context.Anonymous.ApicInitSipi.ApicIcr };
            debug!("WHP vCPU {id}: INIT/SIPI trap (icr={icr:#x})");
            if let Some(router) = &sipi_router {
                router.deliver_icr(id, icr);
            } else {
                warn!("WHP vCPU {id}: INIT/SIPI trap without an AP router (icr={icr:#x})");
            }
            continue;
        }

        #[cfg(target_arch = "x86_64")]
        if reason == WHvRunVpExitReasonX64IoPortAccess {
            let io_access = unsafe { exit_context.Anonymous.IoPortAccess };
            if let Err(err) = emulator.emulate_io(&mut emulator_context, &exit_context, &io_access)
            {
                error!("{err}");
                signal_vcpu_exit(&response_sender, &exit_evt, 1);
                return;
            }
            continue;
        }

        // Unrecoverable exceptions, unsupported features, and unknown exit
        // reasons all end the VM; dump the guest state first so triple
        // faults are diagnosable from a single log capture.
        error!("{}", Error::UnhandledExit { id, reason });
        #[cfg(target_arch = "x86_64")]
        log_x86_fatal_exit_state(partition_handle, id, &exit_context);
        signal_vcpu_exit(&response_sender, &exit_evt, 1);
        return;
    }
}

/// Debug aid, gated on `MSB_KRUN_WHP_TRACE`: samples an AP's state a few
/// seconds after its startup IPI so a silently stuck AP (e.g. parked in a
/// cli;hlt loop) can be located without guest cooperation.
#[cfg(target_arch = "x86_64")]
fn spawn_ap_probe(partition_handle: WHV_PARTITION_HANDLE, id: u8) {
    if std::env::var_os("MSB_KRUN_WHP_TRACE").is_none() {
        return;
    }
    let handle = partition_handle;
    thread::spawn(move || {
        for wait_secs in [1u64, 3, 8] {
            thread::sleep(Duration::from_secs(wait_secs));
            let reg = |name: WHV_REGISTER_NAME| match get_vcpu_register_u64(handle, id, name) {
                Ok(value) => format!("{value:#x}"),
                Err(_) => "<unavailable>".to_string(),
            };
            warn!(
                "WHP AP probe vCPU {id}: rip={} cs_base={} cr0={} cr3={} cr2={} apic_id={} activity={}",
                reg(WHvX64RegisterRip),
                reg(WHvX64RegisterCs),
                reg(WHvX64RegisterCr0),
                reg(WHvX64RegisterCr3),
                reg(WHvX64RegisterCr2),
                reg(WHvX64RegisterApicId),
                reg(WHvRegisterInternalActivityState),
            );
        }
    });
}

#[cfg(target_arch = "x86_64")]
struct CpuidResult {
    rax: u64,
    rbx: u64,
    rcx: u64,
    rdx: u64,
}

/// Answers a trapped CPUID with the hypervisor's default result, normalized
/// so the guest sees the virtual machine rather than the host: stable APIC
/// IDs and topology derived from the vCPU count, the hypervisor-present
/// bit, and no x2APIC (the local APIC emulation mode is xAPIC-only). Guest
/// boot paths must not vary with the host CPU vendor or WHP build defaults.
#[cfg(target_arch = "x86_64")]
fn normalize_cpuid(id: u8, vcpu_count: u8, access: &WHV_X64_CPUID_ACCESS_CONTEXT) -> CpuidResult {
    const CPUID_1_ECX_X2APIC: u32 = 1 << 21;
    const CPUID_1_ECX_HYPERVISOR: u32 = 1 << 31;
    const CPUID_1_EDX_HTT: u32 = 1 << 28;

    let leaf = access.Rax as u32;
    let subleaf = access.Rcx as u32;
    let mut result = CpuidResult {
        rax: access.DefaultResultRax,
        rbx: access.DefaultResultRbx,
        rcx: access.DefaultResultRcx,
        rdx: access.DefaultResultRdx,
    };

    match leaf {
        // The guest must be able to reach the synthesized TSC leaf (0x15)
        // even on hosts whose CPUID tops out below it (AMD stops at 0xD).
        0x0 => {
            let max_leaf = (result.rax as u32).max(0x15);
            result.rax = u64::from(max_leaf);
        }
        0x1 => {
            let mut ebx = result.rbx as u32;
            ebx = (ebx & 0x00ff_ffff) | (u32::from(id) << 24);
            ebx = (ebx & 0xff00_ffff) | (u32::from(vcpu_count) << 16);
            result.rbx = u64::from(ebx);

            let mut ecx = result.rcx as u32;
            ecx |= CPUID_1_ECX_HYPERVISOR;
            ecx &= !CPUID_1_ECX_X2APIC;
            result.rcx = u64::from(ecx);

            let mut edx = result.rdx as u32;
            if vcpu_count > 1 {
                edx |= CPUID_1_EDX_HTT;
            }
            result.rdx = u64::from(edx);
        }
        // Extended topology: one thread per core, `vcpu_count` cores, and
        // the vCPU index as the x2APIC ID, replacing whatever slice of the
        // host topology the default result leaks.
        0xB | 0x1F => {
            let core_shift = u32::from(vcpu_count).next_power_of_two().trailing_zeros();
            let (shift, count, level_type) = match subleaf {
                0 => (0, 1, 1u32),                           // SMT level
                1 => (core_shift, u32::from(vcpu_count), 2), // core level
                _ => (0, 0, 0),                              // no further levels
            };
            result.rax = u64::from(shift);
            result.rbx = u64::from(count);
            result.rcx = u64::from(subleaf | (level_type << 8));
            result.rdx = u64::from(id);
        }
        // TSC/crystal ratio: hand the guest its TSC frequency directly so
        // it never falls back to PIT/HPET calibration (neither of which we
        // emulate well enough) and the jiffies clocksource. The guest TSC
        // runs at the host rate under WHP, which the platform reports via
        // WHvCapabilityCodeProcessorClockFrequency.
        0x15 => {
            if let Some(tsc_hz) = host_tsc_frequency_hz() {
                result.rax = 1; // ratio denominator
                result.rbx = 1; // ratio numerator
                result.rcx = tsc_hz; // "crystal" clock in Hz
                result.rdx = 0;
            }
        }
        _ => {}
    }

    result
}

/// Host processor clock (TSC) frequency, queried once. `None` when the
/// platform cannot report it; the guest then calibrates as before.
#[cfg(target_arch = "x86_64")]
fn host_tsc_frequency_hz() -> Option<u64> {
    static TSC_HZ: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
    *TSC_HZ.get_or_init(|| {
        let mut capability: WHV_CAPABILITY = unsafe { zeroed() };
        let mut written_size = 0;
        let hresult = unsafe {
            WHvGetCapability(
                WHvCapabilityCodeProcessorClockFrequency,
                &mut capability as *mut WHV_CAPABILITY as *mut _,
                size_of::<WHV_CAPABILITY>() as u32,
                &mut written_size,
            )
        };
        if hresult < 0 {
            warn!("WHP: processor clock frequency unavailable (hresult {hresult:#x})");
            return None;
        }
        let hz = unsafe { capability.ProcessorClockFrequency };
        (hz != 0).then_some(hz)
    })
}

/// Applies the architectural SIPI effect: the AP starts in real mode at
/// `vector << 12` with everything else still in the reset state it has held
/// while parked.
#[cfg(target_arch = "x86_64")]
fn apply_sipi_startup_state(
    partition_handle: WHV_PARTITION_HANDLE,
    id: u8,
    vector: u8,
) -> Result<()> {
    let cs = x86_segment(
        u64::from(vector) << 12,
        0xffff,
        u16::from(vector) << 8,
        0x9b,
    );
    // Clearing InternalActivityState releases the VP from the
    // startup-suspend state WHP parks APs in; without this the processor
    // holds its reset state forever and never fetches an instruction.
    let names = [
        WHvX64RegisterCs,
        WHvX64RegisterRip,
        WHvX64RegisterRflags,
        WHvRegisterInternalActivityState,
    ];
    let values = [
        register_value_segment(cs),
        register_value_u64(0),
        register_value_u64(0x2),
        register_value_u64(0),
    ];
    set_vcpu_registers(partition_handle, id, &names, &values)
}

#[cfg(target_arch = "x86_64")]
fn log_x86_fatal_exit_state(
    partition_handle: WHV_PARTITION_HANDLE,
    id: u8,
    exit_context: &WHV_RUN_VP_EXIT_CONTEXT,
) {
    let vp = &exit_context.VpContext;
    // The exit context only carries RIP/CS/RFLAGS; fetch control registers separately so early-boot faults (paging off, CR3 unset) are distinguishable from post-boot ones.
    let reg = |name: WHV_REGISTER_NAME| match get_vcpu_register_u64(partition_handle, id, name) {
        Ok(value) => format!("{value:#x}"),
        Err(_) => "<unavailable>".to_string(),
    };
    error!(
        "WHP vCPU {id} fatal exit state: rip={:#x} cs={:#x} rflags={:#x} exec_state={:#x} cr0={} cr2={} cr3={} cr4={} efer={} rsp={}",
        vp.Rip,
        vp.Cs.Selector,
        vp.Rflags,
        unsafe { vp.ExecutionState.AsUINT16 },
        reg(WHvX64RegisterCr0),
        reg(WHvX64RegisterCr2),
        reg(WHvX64RegisterCr3),
        reg(WHvX64RegisterCr4),
        reg(WHvX64RegisterEfer),
        reg(WHvX64RegisterRsp),
    );
}

fn current_thread_cpu_time_ns() -> Option<u64> {
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    let ok = unsafe {
        GetThreadTimes(
            GetCurrentThread(),
            &mut creation,
            &mut exit,
            &mut kernel,
            &mut user,
        )
    };
    if ok == 0 {
        return None;
    }

    Some(filetime_100ns(&kernel).saturating_add(filetime_100ns(&user)) * 100)
}

fn filetime_100ns(time: &FILETIME) -> u64 {
    (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime)
}

fn signal_vcpu_exit(response_sender: &Sender<VcpuResponse>, exit_evt: &EventFd, exit_code: u8) {
    let _ = response_sender.send(VcpuResponse::Exited(exit_code));
    if let Err(err) = exit_evt.write(1) {
        error!("Failed signaling vcpu exit event: {err}");
    }
}

#[cfg(target_arch = "aarch64")]
struct Arm64TraceCancelGuard {
    partition_handle: WHV_PARTITION_HANDLE,
    id: u8,
    stop: Option<Arc<AtomicBool>>,
    thread: Option<thread::JoinHandle<()>>,
}

#[cfg(target_arch = "aarch64")]
impl Arm64TraceCancelGuard {
    fn new(partition_handle: WHV_PARTITION_HANDLE, id: u8) -> Self {
        if !arm64_whp_trace_enabled() {
            return Self {
                partition_handle,
                id,
                stop: None,
                thread: None,
            };
        }

        debug!("WHP ARM64 vCPU {id} trace pulse enabled");
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let thread = thread::spawn(move || {
            while !thread_stop.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_secs(1));
                if !thread_stop.load(Ordering::Relaxed) {
                    match cancel_run_virtual_processor(partition_handle, id) {
                        Ok(()) => debug!("WHP ARM64 vCPU {id} trace cancel requested"),
                        Err(err) => debug!("WHP ARM64 vCPU {id} trace cancel failed: {err}"),
                    }
                }
            }
        });

        Self {
            partition_handle,
            id,
            stop: Some(stop),
            thread: Some(thread),
        }
    }
}

#[cfg(target_arch = "aarch64")]
impl Drop for Arm64TraceCancelGuard {
    fn drop(&mut self) {
        if let Some(stop) = &self.stop {
            stop.store(true, Ordering::Relaxed);
            let _ = cancel_run_virtual_processor(self.partition_handle, self.id);
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(target_arch = "aarch64")]
fn arm64_whp_trace_enabled() -> bool {
    std::env::var_os("MSB_KRUN_WHP_TRACE").is_some()
        || std::env::var_os("MSB_KRUN_BOOT_TRACE").is_some()
}

#[cfg(target_arch = "aarch64")]
fn trace_arm64_vcpu_pulse(partition_handle: WHV_PARTITION_HANDLE, id: u8) {
    if !arm64_whp_trace_enabled() {
        return;
    }

    match (
        get_vcpu_register_u64(partition_handle, id, WHV_ARM64_REGISTER_PC),
        get_vcpu_register_u64(partition_handle, id, WHV_ARM64_REGISTER_PSTATE),
    ) {
        (Ok(pc), Ok(pstate)) => {
            debug!("WHP ARM64 vCPU {id} pulse: pc=0x{pc:x}, pstate=0x{pstate:x}");
        }
        (pc, pstate) => {
            debug!(
                "WHP ARM64 vCPU {id} pulse register read failed: pc={:?}, pstate={:?}",
                pc.err(),
                pstate.err()
            );
        }
    }
}

#[cfg(target_arch = "aarch64")]
impl WhvArm64RunVpExitContext {
    fn arm64_memory_access(&self) -> &HvArm64MemoryInterceptMessage {
        unsafe { &*(self.message.bytes.as_ptr() as *const HvArm64MemoryInterceptMessage) }
    }

    fn arm64_reset(&self) -> &WhvArm64ResetContext {
        unsafe { &*(self.message.bytes.as_ptr() as *const WhvArm64ResetContext) }
    }
}

#[cfg(target_arch = "aarch64")]
fn whp_canceled_exit_reason() -> WHV_RUN_VP_EXIT_REASON {
    WHV_ARM64_EXIT_REASON_CANCELED
}

#[cfg(target_arch = "x86_64")]
fn whp_canceled_exit_reason() -> WHV_RUN_VP_EXIT_REASON {
    WHvRunVpExitReasonCanceled
}

#[cfg(target_arch = "aarch64")]
fn handle_arm64_mmio(
    context: &mut EmulatorContext,
    message: &HvArm64MemoryInterceptMessage,
) -> Result<()> {
    let syndrome = message.syndrome;
    let exception_class = (syndrome >> 26) & 0x3f;
    if exception_class != AARCH64_EXCEPTION_CLASS_DATA_ABORT_LOWER
        || syndrome & AARCH64_ESR_ISV == 0
    {
        return Err(Error::Arm64MmioSyndrome {
            id: context.id,
            syndrome,
        });
    }

    let size = 1usize << ((syndrome >> 22) & 0x3);
    let is_write = (syndrome >> 6) & 0x1 != 0;
    let register_index = ((syndrome >> 16) & 0x1f) as u8;
    let guest_addr = message.guest_physical_address;

    let Some(mmio_bus) = &context.mmio_bus else {
        return Err(Error::Arm64Mmio {
            id: context.id,
            guest_addr,
            size,
            is_write,
        });
    };

    if is_write {
        let value = arm64_mmio_write_register_value(context, register_index)?;
        let data = value.to_ne_bytes();
        if !mmio_bus.write(context.id.into(), guest_addr, &data[..size]) {
            return Err(Error::Arm64Mmio {
                id: context.id,
                guest_addr,
                size,
                is_write,
            });
        }
    } else {
        let mut data = [0u8; 8];
        if !mmio_bus.read(context.id.into(), guest_addr, &mut data[..size]) {
            return Err(Error::Arm64Mmio {
                id: context.id,
                guest_addr,
                size,
                is_write,
            });
        }

        if register_index != AARCH64_ZERO_REGISTER_INDEX {
            let mut value = u64::from_ne_bytes(data);
            let sign_extend = (syndrome >> 21) & 0x1 != 0;
            if sign_extend {
                let shift = 64 - (size * 8);
                value = ((value as i64) << shift >> shift) as u64;
            }
            let register_is_64_bit = (syndrome >> 15) & 0x1 != 0;
            if !register_is_64_bit {
                value &= 0xffff_ffff;
            }
            set_vcpu_registers(
                context.partition_handle,
                context.id,
                &[arm64_general_register(register_index)],
                &[register_value_u64(value)],
            )?;
        }
    }

    let pc = message
        .header
        .pc
        .wrapping_add(if (syndrome >> 25) & 0x1 != 0 { 4 } else { 2 });
    set_vcpu_registers(
        context.partition_handle,
        context.id,
        &[WHV_ARM64_REGISTER_PC],
        &[register_value_u64(pc)],
    )
}

#[cfg(target_arch = "aarch64")]
fn arm64_mmio_write_register_value(context: &EmulatorContext, register_index: u8) -> Result<u64> {
    if register_index == AARCH64_ZERO_REGISTER_INDEX {
        return Ok(0);
    }

    get_vcpu_register_u64(
        context.partition_handle,
        context.id,
        arm64_general_register(register_index),
    )
}

#[cfg(target_arch = "aarch64")]
fn arm64_general_register(index: u8) -> WHV_REGISTER_NAME {
    debug_assert!(index < AARCH64_ZERO_REGISTER_INDEX);
    WHV_ARM64_REGISTER_X0 + i32::from(index)
}

fn wait_until_resumed(
    event_receiver: &Receiver<VcpuEvent>,
    response_sender: &Sender<VcpuResponse>,
) -> result::Result<(), ()> {
    loop {
        match event_receiver.recv() {
            Ok(VcpuEvent::Resume) => {
                let _ = response_sender.send(VcpuResponse::Resumed);
                return Ok(());
            }
            Ok(VcpuEvent::Pause) => {
                let _ = response_sender.send(VcpuResponse::Paused);
            }
            Err(_) => return Err(()),
        }
    }
}

fn cancel_run_virtual_processor(partition_handle: WHV_PARTITION_HANDLE, id: u8) -> Result<()> {
    let hresult = unsafe { WHvCancelRunVirtualProcessor(partition_handle, id as u32, 0) };
    if hresult < 0 {
        return Err(Error::CancelRunVirtualProcessor { id, hresult });
    }

    Ok(())
}

#[cfg(target_arch = "x86_64")]
unsafe extern "system" fn emulator_io_port_callback(
    context: *const core::ffi::c_void,
    io_access: *mut WHV_EMULATOR_IO_ACCESS_INFO,
) -> windows_sys::core::HRESULT {
    let Some(context) = (context as *mut EmulatorContext).as_mut() else {
        return E_INVALIDARG;
    };
    let Some(io_access) = io_access.as_mut() else {
        return E_INVALIDARG;
    };

    let len = io_access.AccessSize as usize;
    if !matches!(len, 1 | 2 | 4) {
        error!(
            "unsupported WHP I/O access size: vcpu={} port=0x{:x} access_size={} direction={}",
            context.id, io_access.Port, io_access.AccessSize, io_access.Direction
        );
        return E_INVALIDARG;
    }

    let Some(pio_bus) = &context.pio_bus else {
        error!(
            "WHP I/O access has no port bus: vcpu={} port=0x{:x} access_size={} direction={}",
            context.id, io_access.Port, io_access.AccessSize, io_access.Direction
        );
        return E_FAIL;
    };

    // Unclaimed ports behave like a floating ISA bus (reads return all
    // ones, writes are dropped) instead of tearing the VM down: guests
    // legitimately probe ports we do not emulate (ELCR, PCI config, ...).
    let port = u64::from(io_access.Port);
    match io_access.Direction {
        WHV_EMULATOR_DIRECTION_READ => {
            let mut data = [0; 4];
            if !pio_bus.read(context.id.into(), port, &mut data[..len]) {
                debug!(
                    "unclaimed WHP I/O port read: vcpu={} port=0x{port:x} access_size={len}",
                    context.id
                );
                data[..len].fill(0xff);
            }
            io_access.Data = u32::from_le_bytes(data);
            S_OK
        }
        WHV_EMULATOR_DIRECTION_WRITE => {
            let data = io_access.Data.to_le_bytes();
            if !pio_bus.write(context.id.into(), port, &data[..len]) {
                debug!(
                    "unclaimed WHP I/O port write: vcpu={} port=0x{port:x} access_size={len} data=0x{:x}",
                    context.id, io_access.Data
                );
            }
            S_OK
        }
        _ => E_INVALIDARG,
    }
}

#[cfg(target_arch = "x86_64")]
unsafe extern "system" fn emulator_memory_callback(
    context: *const core::ffi::c_void,
    memory_access: *mut WHV_EMULATOR_MEMORY_ACCESS_INFO,
) -> windows_sys::core::HRESULT {
    let Some(context) = (context as *mut EmulatorContext).as_mut() else {
        return E_INVALIDARG;
    };
    let Some(memory_access) = memory_access.as_mut() else {
        return E_INVALIDARG;
    };

    let len = memory_access.AccessSize as usize;
    if len > memory_access.Data.len() {
        error!(
            "unsupported WHP MMIO access size: vcpu={} gpa=0x{:x} access_size={} direction={}",
            context.id, memory_access.GpaAddress, memory_access.AccessSize, memory_access.Direction
        );
        return E_INVALIDARG;
    }

    match memory_access.Direction {
        WHV_EMULATOR_DIRECTION_READ => {
            let mut data = [0u8; 8];
            if let Some(mmio_bus) = &context.mmio_bus {
                if mmio_bus.read(
                    context.id.into(),
                    memory_access.GpaAddress,
                    &mut data[..len],
                ) {
                    memory_access.Data[..len].copy_from_slice(&data[..len]);
                    return S_OK;
                }
            }

            match context.guest_mem.read(
                &mut memory_access.Data[..len],
                GuestAddress(memory_access.GpaAddress),
            ) {
                Ok(_) => S_OK,
                Err(error) => {
                    error!(
                        "unhandled WHP MMIO read: vcpu={} gpa=0x{:x} access_size={len} guest_mem_error={error:?}",
                        context.id, memory_access.GpaAddress
                    );
                    E_FAIL
                }
            }
        }
        WHV_EMULATOR_DIRECTION_WRITE => {
            let data = &memory_access.Data[..len];
            if let Some(mmio_bus) = &context.mmio_bus {
                if mmio_bus.write(context.id.into(), memory_access.GpaAddress, data) {
                    return S_OK;
                }
            }

            match context
                .guest_mem
                .write(data, GuestAddress(memory_access.GpaAddress))
            {
                Ok(_) => S_OK,
                Err(error) => {
                    error!(
                        "unhandled WHP MMIO write: vcpu={} gpa=0x{:x} access_size={len} data={:x?} guest_mem_error={error:?}",
                        context.id, memory_access.GpaAddress, data
                    );
                    E_FAIL
                }
            }
        }
        _ => {
            error!(
                "unsupported WHP MMIO direction: vcpu={} gpa=0x{:x} access_size={len} direction={}",
                context.id, memory_access.GpaAddress, memory_access.Direction
            );
            E_INVALIDARG
        }
    }
}

#[cfg(target_arch = "x86_64")]
unsafe extern "system" fn emulator_get_registers_callback(
    context: *const core::ffi::c_void,
    register_names: *const WHV_REGISTER_NAME,
    register_count: u32,
    register_values: *mut WHV_REGISTER_VALUE,
) -> windows_sys::core::HRESULT {
    let Some(context) = (context as *const EmulatorContext).as_ref() else {
        return E_INVALIDARG;
    };

    WHvGetVirtualProcessorRegisters(
        context.partition_handle,
        context.id.into(),
        register_names,
        register_count,
        register_values,
    )
}

#[cfg(target_arch = "x86_64")]
unsafe extern "system" fn emulator_set_registers_callback(
    context: *const core::ffi::c_void,
    register_names: *const WHV_REGISTER_NAME,
    register_count: u32,
    register_values: *const WHV_REGISTER_VALUE,
) -> windows_sys::core::HRESULT {
    let Some(context) = (context as *const EmulatorContext).as_ref() else {
        return E_INVALIDARG;
    };

    WHvSetVirtualProcessorRegisters(
        context.partition_handle,
        context.id.into(),
        register_names,
        register_count,
        register_values,
    )
}

#[cfg(target_arch = "x86_64")]
unsafe extern "system" fn emulator_translate_gva_callback(
    context: *const core::ffi::c_void,
    gva: u64,
    translate_flags: WHV_TRANSLATE_GVA_FLAGS,
    translation_result: *mut WHV_TRANSLATE_GVA_RESULT_CODE,
    gpa: *mut u64,
) -> windows_sys::core::HRESULT {
    let Some(context) = (context as *const EmulatorContext).as_ref() else {
        return E_INVALIDARG;
    };
    if translation_result.is_null() || gpa.is_null() {
        return E_INVALIDARG;
    }

    let mut result = WHV_TRANSLATE_GVA_RESULT {
        ResultCode: WHvTranslateGvaResultSuccess,
        Reserved: 0,
    };
    let hresult = WHvTranslateGva(
        context.partition_handle,
        context.id.into(),
        gva,
        translate_flags,
        &mut result,
        gpa,
    );
    if hresult >= 0 {
        *translation_result = result.ResultCode;
    }

    hresult
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn group_affinity_encodes_processor_group_and_single_cpu_mask() {
        let affinity = group_affinity(HostCpuId::in_group(3, 7)).unwrap();

        assert_eq!(affinity.Group, 3);
        assert_eq!(affinity.Mask, 1usize << 7);
        assert_eq!(affinity.Reserved, [0; 3]);
    }

    #[test]
    fn group_affinity_rejects_index_outside_windows_group_width() {
        let error = match group_affinity(HostCpuId::new(usize::BITS as u16)) {
            Ok(_) => panic!("processor index outside the group width should fail"),
            Err(error) => error,
        };

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn best_effort_host_affinity_reports_os_rejection_as_inherited() {
        let unavailable = HostCpuId::new(usize::BITS as u16);

        assert!(apply_host_cpu_affinity(Some(unavailable), true, 0).is_err());
        assert!(matches!(
            apply_host_cpu_affinity(Some(unavailable), false, 0),
            Ok(VcpuPlacementResult::Inherited { .. })
        ));
    }

    #[test]
    fn host_affinity_binds_the_current_thread_to_one_allowed_processor() {
        let mut inherited = GROUP_AFFINITY::default();
        let result = unsafe { GetThreadGroupAffinity(GetCurrentThread(), &mut inherited) };
        assert_ne!(result, 0, "GetThreadGroupAffinity failed");
        assert_ne!(
            inherited.Mask, 0,
            "current thread has an empty affinity mask"
        );

        let index = inherited.Mask.trailing_zeros() as u16;
        apply_host_cpu_affinity(Some(HostCpuId::in_group(inherited.Group, index)), true, 0)
            .unwrap();

        let mut applied = GROUP_AFFINITY::default();
        let result = unsafe { GetThreadGroupAffinity(GetCurrentThread(), &mut applied) };
        assert_ne!(result, 0, "GetThreadGroupAffinity failed after binding");
        assert_eq!(applied.Group, inherited.Group);
        assert_eq!(applied.Mask, 1usize << index);
    }
}
