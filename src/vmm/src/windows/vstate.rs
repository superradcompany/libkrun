// Copyright 2026 Super Rad Company.
// SPDX-License-Identifier: Apache-2.0

use std::fmt::{Display, Formatter};
use std::mem::{size_of, zeroed};
use std::result;
use std::thread;

use crate::vmm_config::machine_config::CpuFeaturesTemplate;

use crossbeam_channel::{unbounded, Receiver, Sender};
use vm_memory::GuestMemoryMmap;
use windows_sys::Win32::System::Hypervisor::{
    WHvCapabilityCodeHypervisorPresent, WHvDeletePartition, WHvGetCapability, WHV_CAPABILITY,
    WHV_PARTITION_HANDLE,
};

#[derive(Debug)]
pub enum Error {
    GetCapability {
        code: i32,
        hresult: windows_sys::core::HRESULT,
    },
    HypervisorNotPresent,
    NotImplemented(&'static str),
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        match self {
            Error::GetCapability { code, hresult } => write!(
                f,
                "WHvGetCapability({code}) failed with HRESULT 0x{:08x}",
                *hresult as u32
            ),
            Error::HypervisorNotPresent => write!(
                f,
                "Windows Hypervisor Platform is not available. Enable Windows Hypervisor Platform and virtualization support, then reboot."
            ),
            Error::NotImplemented(feature) => {
                write!(f, "WHP backend support is not implemented yet: {feature}")
            }
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
}

#[derive(Debug, Eq, PartialEq)]
pub struct VcpuConfig {
    pub vcpu_count: u8,
    pub ht_enabled: bool,
    pub cpu_template: Option<CpuFeaturesTemplate>,
}

pub struct Vcpu {
    id: u8,
    event_sender: Option<Sender<VcpuEvent>>,
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
    event_sender: Sender<VcpuEvent>,
    response_receiver: Receiver<VcpuResponse>,
}

struct Partition {
    handle: WHV_PARTITION_HANDLE,
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
    pub fn new() -> Result<Self> {
        WhpCapabilities::ensure_hypervisor_present()?;

        Err(Error::NotImplemented("partition creation"))
    }

    pub fn memory_init(&mut self, _guest_mem: &GuestMemoryMmap) -> Result<()> {
        Err(Error::NotImplemented("guest memory mapping"))
    }

    pub fn partition_handle(&self) -> WHV_PARTITION_HANDLE {
        self.partition.handle
    }
}

impl Vcpu {
    pub fn register_kick_signal_handler() {}

    pub fn new(id: u8) -> Result<Self> {
        let (event_sender, _event_receiver) = unbounded();
        let (_response_sender, response_receiver) = unbounded();

        Ok(Self {
            id,
            event_sender: Some(event_sender),
            response_receiver: Some(response_receiver),
        })
    }

    pub fn set_mmio_bus(&mut self, _mmio_bus: devices::Bus) {}

    pub fn start_threaded(mut self) -> Result<VcpuHandle> {
        let event_sender = self
            .event_sender
            .take()
            .expect("event sender missing before vcpu start");
        let response_receiver = self
            .response_receiver
            .take()
            .expect("response receiver missing before vcpu start");
        let _ = (event_sender, response_receiver);

        Err(Error::NotImplemented("vCPU run loop"))
    }

    pub fn cpu_index(&self) -> u8 {
        self.id
    }
}

impl VcpuHandle {
    pub fn new(
        event_sender: Sender<VcpuEvent>,
        response_receiver: Receiver<VcpuResponse>,
        _vcpu_thread: thread::JoinHandle<()>,
    ) -> Self {
        Self {
            event_sender,
            response_receiver,
        }
    }

    pub fn send_event(&self, event: VcpuEvent) -> Result<()> {
        self.event_sender
            .send(event)
            .expect("event sender channel closed on vcpu end.");
        Ok(())
    }

    pub fn response_receiver(&self) -> &Receiver<VcpuResponse> {
        &self.response_receiver
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

impl Default for Partition {
    fn default() -> Self {
        Self { handle: 0 }
    }
}
