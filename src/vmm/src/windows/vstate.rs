// Copyright 2026 Super Rad Company.
// SPDX-License-Identifier: Apache-2.0

use std::fmt::{Display, Formatter};
use std::mem::{size_of, zeroed};
use std::result;
#[cfg(target_arch = "aarch64")]
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread;
#[cfg(target_arch = "aarch64")]
use std::time::Duration;

use crate::vmm_config::machine_config::CpuFeaturesTemplate;

use arch::ArchMemoryInfo;
use crossbeam_channel::{unbounded, Receiver, Sender, TryRecvError};
use utils::eventfd::EventFd;
#[cfg(target_arch = "x86_64")]
use vm_memory::Bytes;
use vm_memory::{Address, GuestAddress, GuestMemoryBackend, GuestMemoryMmap, GuestMemoryRegion};
#[cfg(target_arch = "x86_64")]
use windows_sys::Win32::Foundation::{E_FAIL, E_INVALIDARG, S_OK};
use windows_sys::Win32::System::Hypervisor::{
    WHvCancelRunVirtualProcessor, WHvCapabilityCodeHypervisorPresent, WHvCreatePartition,
    WHvCreateVirtualProcessor, WHvDeletePartition, WHvDeleteVirtualProcessor, WHvGetCapability,
    WHvGetVirtualProcessorRegisters, WHvMapGpaRange, WHvMapGpaRangeFlagExecute,
    WHvMapGpaRangeFlagRead, WHvMapGpaRangeFlagWrite, WHvPartitionPropertyCodeProcessorCount,
    WHvRunVirtualProcessor, WHvRunVpExitReasonNone, WHvSetPartitionProperty,
    WHvSetVirtualProcessorRegisters, WHvSetupPartition, WHvUnmapGpaRange, WHV_CAPABILITY,
    WHV_PARTITION_HANDLE, WHV_REGISTER_NAME, WHV_REGISTER_VALUE, WHV_RUN_VP_EXIT_REASON,
};
#[cfg(target_arch = "x86_64")]
use windows_sys::Win32::System::Hypervisor::{
    WHvRunVpExitReasonCanceled, WHvRunVpExitReasonMemoryAccess,
    WHvRunVpExitReasonUnrecoverableException, WHvRunVpExitReasonUnsupportedFeature,
    WHvRunVpExitReasonX64Halt, WHvRunVpExitReasonX64IoPortAccess, WHvTranslateGva,
    WHvTranslateGvaResultSuccess, X64_RegisterCr0, X64_RegisterCr3, X64_RegisterCr4,
    X64_RegisterCs, X64_RegisterDs, X64_RegisterEfer, X64_RegisterEs, X64_RegisterFs,
    X64_RegisterGdtr, X64_RegisterGs, X64_RegisterIdtr, X64_RegisterRFlags, X64_RegisterRbp,
    X64_RegisterRip, X64_RegisterRsi, X64_RegisterRsp, X64_RegisterSs, X64_RegisterTr,
    WHV_EMULATOR_CALLBACKS, WHV_EMULATOR_IO_ACCESS_INFO, WHV_EMULATOR_MEMORY_ACCESS_INFO,
    WHV_EMULATOR_STATUS, WHV_MEMORY_ACCESS_CONTEXT, WHV_RUN_VP_EXIT_CONTEXT,
    WHV_TRANSLATE_GVA_FLAGS, WHV_TRANSLATE_GVA_RESULT, WHV_TRANSLATE_GVA_RESULT_CODE,
    WHV_X64_IO_PORT_ACCESS_CONTEXT, WHV_X64_SEGMENT_REGISTER, WHV_X64_SEGMENT_REGISTER_0,
    WHV_X64_TABLE_REGISTER,
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
const WHV_ARM64_EXIT_REASON_UNRECOVERABLE_EXCEPTION: WHV_RUN_VP_EXIT_REASON =
    0x8000_0021_u32 as i32;
#[cfg(target_arch = "aarch64")]
const WHV_ARM64_EXIT_REASON_UNSUPPORTED_FEATURE: WHV_RUN_VP_EXIT_REASON = 0x8000_0022_u32 as i32;
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
    #[cfg(target_arch = "aarch64")]
    GetVirtualProcessorRegisters {
        id: u8,
        hresult: windows_sys::core::HRESULT,
    },
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
            #[cfg(target_arch = "aarch64")]
            Error::GetVirtualProcessorRegisters { id, hresult } => write!(
                f,
                "WHvGetVirtualProcessorRegisters({id}) failed with HRESULT 0x{:08x}",
                *hresult as u32
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
    pub ht_enabled: bool,
    pub cpu_template: Option<CpuFeaturesTemplate>,
}

pub struct Vcpu {
    id: u8,
    partition_handle: WHV_PARTITION_HANDLE,
    guest_mem: Option<GuestMemoryMmap>,
    mmio_bus: Option<devices::Bus>,
    #[cfg(target_arch = "x86_64")]
    pio_bus: Option<devices::Bus>,
    exit_evt: EventFd,
    event_sender: Option<Sender<VcpuEvent>>,
    event_receiver: Option<Receiver<VcpuEvent>>,
    response_sender: Option<Sender<VcpuResponse>>,
    response_receiver: Option<Receiver<VcpuResponse>>,
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
    event_sender: Sender<VcpuEvent>,
    response_receiver: Receiver<VcpuResponse>,
    _vcpu_thread: thread::JoinHandle<()>,
}

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

#[cfg(target_arch = "aarch64")]
#[repr(C)]
struct WhvArm64RunVpExitContext {
    exit_reason: WHV_RUN_VP_EXIT_REASON,
    reserved: u32,
    reserved1: u64,
    message: WhvArm64RunVpExitMessage,
}

#[cfg(target_arch = "aarch64")]
impl Default for WhvArm64RunVpExitContext {
    fn default() -> Self {
        Self {
            exit_reason: 0,
            reserved: 0,
            reserved1: 0,
            message: WhvArm64RunVpExitMessage::default(),
        }
    }
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

    pub fn partition_handle(&self) -> WHV_PARTITION_HANDLE {
        self.partition.handle
    }

    pub fn create_vcpu(&self, id: u8, exit_evt: EventFd) -> Result<Vcpu> {
        Vcpu::new(id, self.partition.handle, exit_evt)
    }
}

impl Vcpu {
    pub fn register_kick_signal_handler() {}

    pub fn new(id: u8, partition_handle: WHV_PARTITION_HANDLE, exit_evt: EventFd) -> Result<Self> {
        let hresult = unsafe { WHvCreateVirtualProcessor(partition_handle, id as u32, 0) };
        if hresult < 0 {
            return Err(Error::CreateVirtualProcessor { id, hresult });
        }

        let (event_sender, event_receiver) = unbounded();
        let (response_sender, response_receiver) = unbounded();

        Ok(Self {
            id,
            partition_handle,
            guest_mem: None,
            mmio_bus: None,
            #[cfg(target_arch = "x86_64")]
            pio_bus: None,
            exit_evt,
            event_sender: Some(event_sender),
            event_receiver: Some(event_receiver),
            response_sender: Some(response_sender),
            response_receiver: Some(response_receiver),
        })
    }

    pub fn set_mmio_bus(&mut self, mmio_bus: devices::Bus) {
        self.mmio_bus = Some(mmio_bus);
    }

    #[cfg(target_arch = "x86_64")]
    pub fn set_pio_bus(&mut self, pio_bus: devices::Bus) {
        self.pio_bus = Some(pio_bus);
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

    pub fn start_threaded(mut self) -> Result<VcpuHandle> {
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
        let partition_handle = self.partition_handle;
        let guest_mem = self
            .guest_mem
            .take()
            .expect("guest memory missing before vcpu start");
        let mmio_bus = self.mmio_bus.take();
        #[cfg(target_arch = "x86_64")]
        let pio_bus = self.pio_bus.take();
        let exit_evt = self.exit_evt.try_clone().map_err(Error::VcpuThreadSpawn)?;
        self.partition_handle = 0;

        let vcpu_thread = thread::Builder::new()
            .name(format!("whp-vcpu-{id}"))
            .spawn(move || {
                run_vcpu(
                    id,
                    partition_handle,
                    guest_mem,
                    mmio_bus,
                    #[cfg(target_arch = "x86_64")]
                    pio_bus,
                    exit_evt,
                    event_receiver,
                    response_sender,
                )
            })
            .map_err(Error::VcpuThreadSpawn)?;

        Ok(VcpuHandle::new(
            id,
            partition_handle,
            event_sender,
            response_receiver,
            vcpu_thread,
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
            X64_RegisterRip,
            X64_RegisterRFlags,
            X64_RegisterRsp,
            X64_RegisterRbp,
            X64_RegisterRsi,
            X64_RegisterCr0,
            X64_RegisterCr3,
            X64_RegisterCr4,
            X64_RegisterEfer,
            X64_RegisterCs,
            X64_RegisterDs,
            X64_RegisterEs,
            X64_RegisterFs,
            X64_RegisterGs,
            X64_RegisterSs,
            X64_RegisterTr,
            X64_RegisterGdtr,
            X64_RegisterIdtr,
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
            event_sender,
            response_receiver,
            _vcpu_thread: vcpu_thread,
        }
    }

    pub fn send_event(&self, event: VcpuEvent) -> Result<()> {
        let should_cancel = matches!(event, VcpuEvent::Pause);
        self.event_sender
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

fn set_vcpu_registers(
    partition_handle: WHV_PARTITION_HANDLE,
    id: u8,
    names: &[WHV_REGISTER_NAME],
    values: &[WHV_REGISTER_VALUE],
) -> Result<()> {
    debug_assert_eq!(names.len(), values.len());

    let hresult = unsafe {
        WHvSetVirtualProcessorRegisters(
            partition_handle,
            id as u32,
            names.as_ptr(),
            names.len() as u32,
            values.as_ptr(),
        )
    };
    if hresult < 0 {
        return Err(Error::SetVirtualProcessorRegisters { id, hresult });
    }

    Ok(())
}

#[cfg(target_arch = "aarch64")]
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
    WHV_REGISTER_VALUE { Reg64: value }
}

#[cfg(target_arch = "x86_64")]
fn register_value_segment(value: WHV_X64_SEGMENT_REGISTER) -> WHV_REGISTER_VALUE {
    WHV_REGISTER_VALUE { Segment: value }
}

#[cfg(target_arch = "x86_64")]
fn register_value_table(value: WHV_X64_TABLE_REGISTER) -> WHV_REGISTER_VALUE {
    WHV_REGISTER_VALUE { Table: value }
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

fn run_vcpu(
    id: u8,
    partition_handle: WHV_PARTITION_HANDLE,
    guest_mem: GuestMemoryMmap,
    mmio_bus: Option<devices::Bus>,
    #[cfg(target_arch = "x86_64")] pio_bus: Option<devices::Bus>,
    exit_evt: EventFd,
    event_receiver: Receiver<VcpuEvent>,
    response_sender: Sender<VcpuResponse>,
) {
    if wait_until_resumed(&event_receiver, &response_sender).is_err() {
        return;
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

        let hresult = unsafe {
            WHvRunVirtualProcessor(
                partition_handle,
                id as u32,
                exit_context_ptr,
                exit_context_size,
            )
        };
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
            if exit_code == 0 {
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

        if reason == whp_unrecoverable_exception_exit_reason()
            || reason == whp_unsupported_feature_exit_reason()
        {
            error!("{}", Error::UnhandledExit { id, reason });
            signal_vcpu_exit(&response_sender, &exit_evt, 1);
            return;
        }

        error!("{}", Error::UnhandledExit { id, reason });
        signal_vcpu_exit(&response_sender, &exit_evt, 1);
        return;
    }
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
fn whp_unrecoverable_exception_exit_reason() -> WHV_RUN_VP_EXIT_REASON {
    WHV_ARM64_EXIT_REASON_UNRECOVERABLE_EXCEPTION
}

#[cfg(target_arch = "x86_64")]
fn whp_unrecoverable_exception_exit_reason() -> WHV_RUN_VP_EXIT_REASON {
    WHvRunVpExitReasonUnrecoverableException
}

#[cfg(target_arch = "aarch64")]
fn whp_unsupported_feature_exit_reason() -> WHV_RUN_VP_EXIT_REASON {
    WHV_ARM64_EXIT_REASON_UNSUPPORTED_FEATURE
}

#[cfg(target_arch = "x86_64")]
fn whp_unsupported_feature_exit_reason() -> WHV_RUN_VP_EXIT_REASON {
    WHvRunVpExitReasonUnsupportedFeature
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
        return E_INVALIDARG;
    }

    let Some(pio_bus) = &context.pio_bus else {
        return E_FAIL;
    };

    let port = u64::from(io_access.Port);
    match io_access.Direction {
        WHV_EMULATOR_DIRECTION_READ => {
            let mut data = io_access.Data.to_le_bytes();
            if pio_bus.read(context.id.into(), port, &mut data[..len]) {
                io_access.Data = u32::from_le_bytes(data);
                S_OK
            } else {
                E_FAIL
            }
        }
        WHV_EMULATOR_DIRECTION_WRITE => {
            let data = io_access.Data.to_le_bytes();
            if pio_bus.write(context.id.into(), port, &data[..len]) {
                S_OK
            } else {
                E_FAIL
            }
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
                Err(_) => E_FAIL,
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
                Err(_) => E_FAIL,
            }
        }
        _ => E_INVALIDARG,
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

impl Default for Partition {
    fn default() -> Self {
        Self { handle: 0 }
    }
}
