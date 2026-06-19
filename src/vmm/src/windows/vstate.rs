// Copyright 2026 Super Rad Company.
// SPDX-License-Identifier: Apache-2.0

use std::fmt::{Display, Formatter};
use std::mem::{size_of, zeroed};
use std::result;
use std::thread;

use crate::vmm_config::machine_config::CpuFeaturesTemplate;

use arch::ArchMemoryInfo;
use crossbeam_channel::{unbounded, Receiver, Sender, TryRecvError};
use vm_memory::{Address, GuestAddress, GuestMemoryBackend, GuestMemoryMmap, GuestMemoryRegion};
#[cfg(target_arch = "aarch64")]
use windows_sys::Win32::System::Hypervisor::{
    ARM64_RegisterCpsr, ARM64_RegisterPc, ARM64_RegisterX0, WHvSetVirtualProcessorRegisters,
    WHV_REGISTER_NAME, WHV_REGISTER_VALUE,
};
use windows_sys::Win32::System::Hypervisor::{
    WHvCancelRunVirtualProcessor, WHvCapabilityCodeHypervisorPresent, WHvCreatePartition,
    WHvCreateVirtualProcessor, WHvDeletePartition, WHvDeleteVirtualProcessor, WHvGetCapability,
    WHvMapGpaRange, WHvMapGpaRangeFlagExecute, WHvMapGpaRangeFlagRead, WHvMapGpaRangeFlagWrite,
    WHvPartitionPropertyCodeProcessorCount, WHvRunVirtualProcessor, WHvRunVpExitReasonCanceled,
    WHvRunVpExitReasonMemoryAccess, WHvRunVpExitReasonNone,
    WHvRunVpExitReasonUnrecoverableException, WHvRunVpExitReasonUnsupportedFeature,
    WHvRunVpExitReasonX64Halt, WHvSetPartitionProperty, WHvSetupPartition, WHvUnmapGpaRange,
    WHV_CAPABILITY, WHV_PARTITION_HANDLE, WHV_PARTITION_PROPERTY, WHV_RUN_VP_EXIT_CONTEXT,
    WHV_RUN_VP_EXIT_REASON,
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
    HypervisorNotPresent,
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
            Error::HypervisorNotPresent => write!(
                f,
                "Windows Hypervisor Platform is not available. Enable Windows Hypervisor Platform and virtualization support, then reboot."
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

#[derive(Clone, Copy, Debug)]
struct MappedMemoryRegion {
    guest_addr: u64,
    size: u64,
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

    pub fn create_vcpu(&self, id: u8) -> Result<Vcpu> {
        Vcpu::new(id, self.partition.handle)
    }
}

impl Vcpu {
    pub fn register_kick_signal_handler() {}

    pub fn new(id: u8, partition_handle: WHV_PARTITION_HANDLE) -> Result<Self> {
        let hresult = unsafe { WHvCreateVirtualProcessor(partition_handle, id as u32, 0) };
        if hresult < 0 {
            return Err(Error::CreateVirtualProcessor { id, hresult });
        }

        let (event_sender, event_receiver) = unbounded();
        let (response_sender, response_receiver) = unbounded();

        Ok(Self {
            id,
            partition_handle,
            event_sender: Some(event_sender),
            event_receiver: Some(event_receiver),
            response_sender: Some(response_sender),
            response_receiver: Some(response_receiver),
        })
    }

    pub fn set_mmio_bus(&mut self, _mmio_bus: devices::Bus) {}

    #[cfg(target_arch = "aarch64")]
    pub fn configure_windows(
        &mut self,
        _guest_mem: &GuestMemoryMmap,
        mem_info: &ArchMemoryInfo,
        entry_addr: GuestAddress,
    ) -> Result<()> {
        self.configure_aarch64(entry_addr, mem_info.fdt_addr)
    }

    #[cfg(target_arch = "x86_64")]
    pub fn configure_windows(
        &mut self,
        _guest_mem: &GuestMemoryMmap,
        _mem_info: &ArchMemoryInfo,
        _entry_addr: GuestAddress,
    ) -> Result<()> {
        Err(Error::NotImplemented("x86_64 WHP vCPU register setup"))
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
        self.partition_handle = 0;

        let vcpu_thread = thread::Builder::new()
            .name(format!("whp-vcpu-{id}"))
            .spawn(move || run_vcpu(id, partition_handle, event_receiver, response_sender))
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
    fn configure_aarch64(&mut self, entry_addr: GuestAddress, fdt_addr: u64) -> Result<()> {
        let mut names = vec![ARM64_RegisterCpsr as WHV_REGISTER_NAME];
        let mut values = vec![register_value_u64(AARCH64_PSTATE_FAULT_BITS_64)];

        if self.id == 0 {
            names.push(ARM64_RegisterPc as WHV_REGISTER_NAME);
            values.push(register_value_u64(entry_addr.raw_value()));
            names.push(ARM64_RegisterX0 as WHV_REGISTER_NAME);
            values.push(register_value_u64(fdt_addr));
        }

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

impl Partition {
    fn new(vcpu_count: u8) -> Result<Self> {
        let mut handle = 0;
        let hresult = unsafe { WHvCreatePartition(&mut handle) };
        if hresult < 0 {
            return Err(Error::CreatePartition(hresult));
        }

        let partition = Self { handle };
        partition.set_processor_count(vcpu_count)?;
        partition.setup()?;

        Ok(partition)
    }

    fn set_processor_count(&self, vcpu_count: u8) -> Result<()> {
        let property = WHV_PARTITION_PROPERTY {
            ProcessorCount: vcpu_count as u32,
        };
        let property_code = WHvPartitionPropertyCodeProcessorCount;
        let hresult = unsafe {
            WHvSetPartitionProperty(
                self.handle,
                property_code,
                &property as *const WHV_PARTITION_PROPERTY as *const _,
                size_of::<WHV_PARTITION_PROPERTY>() as u32,
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

#[cfg(target_arch = "aarch64")]
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
fn register_value_u64(value: u64) -> WHV_REGISTER_VALUE {
    WHV_REGISTER_VALUE { Reg64: value }
}

fn run_vcpu(
    id: u8,
    partition_handle: WHV_PARTITION_HANDLE,
    event_receiver: Receiver<VcpuEvent>,
    response_sender: Sender<VcpuResponse>,
) {
    if wait_until_resumed(&event_receiver, &response_sender).is_err() {
        return;
    }

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

        let hresult = unsafe {
            WHvRunVirtualProcessor(
                partition_handle,
                id as u32,
                &mut exit_context as *mut WHV_RUN_VP_EXIT_CONTEXT as *mut _,
                size_of::<WHV_RUN_VP_EXIT_CONTEXT>() as u32,
            )
        };
        if hresult < 0 {
            error!("{}", Error::RunVirtualProcessor { id, hresult });
            let _ = response_sender.send(VcpuResponse::Exited(1));
            return;
        }

        let reason = exit_context.ExitReason;
        if reason == WHvRunVpExitReasonNone || reason == WHvRunVpExitReasonCanceled {
            continue;
        }

        if reason == WHvRunVpExitReasonX64Halt {
            let _ = response_sender.send(VcpuResponse::Exited(0));
            return;
        }

        if reason == WHvRunVpExitReasonMemoryAccess
            || reason == WHvRunVpExitReasonUnrecoverableException
            || reason == WHvRunVpExitReasonUnsupportedFeature
        {
            error!("{}", Error::UnhandledExit { id, reason });
            let _ = response_sender.send(VcpuResponse::Exited(1));
            return;
        }

        error!("{}", Error::UnhandledExit { id, reason });
        let _ = response_sender.send(VcpuResponse::Exited(1));
        return;
    }
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

impl Default for Partition {
    fn default() -> Self {
        Self { handle: 0 }
    }
}
