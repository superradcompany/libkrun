// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the THIRD-PARTY file.

use std::cell::Cell;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::fmt::{Display, Formatter};
use std::io;
use std::result;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use super::super::{
    VcpuControlRequestId, FC_EXIT_CODE_GENERIC_ERROR, FC_EXIT_CODE_OK,
    VCPU_CONTROL_MAILBOX_CAPACITY,
};
use crate::memory_state::GuestMemoryRange;
use crate::vmm_config::machine_config::CpuFeaturesTemplate;

use arch::ArchMemoryInfo;
use crossbeam_channel::{bounded, unbounded, Receiver, Sender};
use devices::legacy::VcpuList;
use hvf::{protect_memory, vcpu_request_exit, HvfVcpu, HvfVcpuState, HvfVm, VcpuExit, Vcpus};
use serde::{Deserialize, Serialize};
use utils::eventfd::EventFd;
use utils::metrics::MetricsWriter;
use vm_memory::{
    Address, GuestAddress, GuestMemoryBackend, GuestMemoryError, GuestMemoryMmap, GuestMemoryRegion,
};

/// Errors associated with the wrappers over KVM ioctls.
#[derive(Debug)]
pub enum Error {
    /// Invalid guest memory configuration.
    GuestMemoryMmap(GuestMemoryError),
    /// The number of configured slots is bigger than the maximum reported by KVM.
    NotEnoughMemorySlots,
    /// Error configuring the general purpose aarch64 registers.
    REGSConfiguration(arch::aarch64::regs::Error),
    /// Cannot set the memory regions.
    SetUserMemoryRegion(hvf::Error),
    /// Cannot change guest-memory permissions for dirty tracking.
    DirtyMemoryProtect(hvf::Error),
    /// Dirty tracking is not active for this VM.
    DirtyTrackingInactive,
    /// Cannot determine the host protection granule.
    DirtyPageSize(io::Error),
    /// Failed writing a PSCI result into the vCPU.
    VcpuHvf(hvf::Error),
    /// Backend execution state could not be encoded or decoded.
    StateCodec(String),
    /// Failed to signal Vcpu.
    SignalVcpu(utils::errno::Error),
    /// Error doing Vcpu Init on Arm.
    VcpuArmInit,
    /// Error getting the Vcpu preferred target on Arm.
    VcpuArmPreferredTarget,
    /// vCPU count is not initialized.
    VcpuCountNotInitialized,
    /// Cannot run the VCPUs.
    VcpuRun,
    /// Cannot spawn a new vCPU thread.
    VcpuSpawn(io::Error),
    /// Cannot cleanly initialize vcpu TLS.
    VcpuTlsInit,
    /// Vcpu not present in TLS.
    VcpuTlsNotPresent,
    /// Unexpected KVM_RUN exit reason
    VcpuUnhandledKvmExit,
    /// Cannot configure the microvm.
    VmSetup(hvf::Error),
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        use self::Error::*;

        match self {
            GuestMemoryMmap(e) => write!(f, "Guest memory error: {e:?}"),
            VcpuCountNotInitialized => write!(f, "vCPU count is not initialized"),
            VmSetup(e) => write!(f, "Cannot configure the microvm: {e:?}"),
            VcpuRun => write!(f, "Cannot run the VCPUs"),
            NotEnoughMemorySlots => write!(
                f,
                "The number of configured slots is bigger than the maximum reported by KVM"
            ),
            SetUserMemoryRegion(e) => write!(f, "Cannot set the memory regions: {e:?}"),
            DirtyMemoryProtect(e) => write!(f, "Cannot change tracked memory permissions: {e}"),
            DirtyTrackingInactive => write!(f, "HVF dirty tracking is not active"),
            DirtyPageSize(e) => write!(f, "Cannot determine the HVF protection granule: {e}"),
            VcpuHvf(e) => write!(f, "Failed writing a PSCI result into the vCPU: {e:?}"),
            StateCodec(e) => write!(f, "HVF execution-state codec failed: {e}"),
            SignalVcpu(e) => write!(f, "Failed to signal Vcpu: {e}"),
            REGSConfiguration(e) => write!(
                f,
                "Error configuring the general purpose aarch64 registers: {e:?}"
            ),
            VcpuSpawn(e) => write!(f, "Cannot spawn a new vCPU thread: {e}"),
            VcpuTlsInit => write!(f, "Cannot clean init vcpu TLS"),
            VcpuTlsNotPresent => write!(f, "Vcpu not present in TLS"),
            VcpuUnhandledKvmExit => write!(f, "Unexpected KVM_RUN exit reason"),
            VcpuArmPreferredTarget => write!(f, "Error getting the Vcpu preferred target on Arm"),
            VcpuArmInit => write!(f, "Error doing Vcpu Init on Arm"),
        }
    }
}

pub type Result<T> = result::Result<T, Error>;

/// A wrapper around creating and using a VM.
pub struct Vm {
    hvf_vm: HvfVm,
    dirty_tracker: Arc<HvfDirtyTracker>,
}

pub(crate) struct HvfDirtyTracker {
    regions: Mutex<Vec<GuestMemoryRange>>,
    dirty_pages: Mutex<BTreeSet<u64>>,
    page_size: u64,
    active: AtomicBool,
}

impl Vm {
    /// Constructs a new `Vm` using the given `Kvm` instance.
    pub fn new(nested_enabled: bool) -> Result<Self> {
        let hvf_vm = HvfVm::new(nested_enabled).map_err(Error::VmSetup)?;

        let page_size = host_page_size()?;
        Ok(Vm {
            hvf_vm,
            dirty_tracker: Arc::new(HvfDirtyTracker {
                regions: Mutex::new(Vec::new()),
                dirty_pages: Mutex::new(BTreeSet::new()),
                page_size,
                active: AtomicBool::new(false),
            }),
        })
    }

    pub fn hvf_vm(&self) -> &HvfVm {
        &self.hvf_vm
    }

    /// Initializes the guest memory.
    pub fn memory_init(&mut self, guest_mem: &GuestMemoryMmap) -> Result<()> {
        for region in guest_mem.iter() {
            // It's safe to unwrap because the guest address is valid.
            let host_addr = guest_mem.get_host_address(region.start_addr()).unwrap();
            debug!(
                "Guest memory host_addr={:x?} guest_addr={:x?} len={:x?}",
                host_addr,
                region.start_addr().raw_value(),
                region.len()
            );
            self.hvf_vm
                .map_memory(
                    host_addr as u64,
                    region.start_addr().raw_value(),
                    region.len(),
                )
                .map_err(Error::SetUserMemoryRegion)?;
            self.dirty_tracker
                .regions
                .lock()
                .expect("HVF dirty-region mutex poisoned")
                .push(
                    GuestMemoryRange::new(region.start_addr().raw_value(), region.len())
                        .expect("guest-memory regions are non-empty and bounded"),
                );
        }

        Ok(())
    }

    pub(crate) fn dirty_tracker(&self) -> Arc<HvfDirtyTracker> {
        Arc::clone(&self.dirty_tracker)
    }

    /// Starts first-write tracking for every ordinary guest-memory mapping.
    pub fn begin_dirty_tracking(&mut self) -> Result<()> {
        self.dirty_tracker.begin()
    }

    /// Seals the current first-write generation and re-protects its dirty pages.
    pub fn take_dirty_ranges(&mut self) -> Result<Vec<GuestMemoryRange>> {
        self.dirty_tracker.take_dirty_ranges()
    }

    /// Stops first-write tracking and restores writable mappings.
    pub fn stop_dirty_tracking(&mut self) -> Result<()> {
        self.dirty_tracker.stop()
    }

    /// Captures the process-wide in-kernel interrupt-controller state.
    pub fn capture_execution_state(&self) -> Result<Vec<u8>> {
        self.hvf_vm.capture_gic_state().map_err(Error::VmSetup)
    }

    /// Restores the process-wide in-kernel interrupt-controller state.
    pub fn restore_execution_state(&self, bytes: &[u8]) -> Result<()> {
        self.hvf_vm.restore_gic_state(bytes).map_err(Error::VmSetup)
    }

    /// Completes a vCPU artifact on the VMM controller thread.
    pub fn complete_vcpu_execution_capture(&self, _id: u32, bytes: Vec<u8>) -> Result<Vec<u8>> {
        Ok(bytes)
    }

    /// Prepares a vCPU artifact for restoration on its owning thread.
    pub fn prepare_vcpu_execution_restore(&self, _id: u32, bytes: &[u8]) -> Result<Vec<u8>> {
        Ok(bytes.to_vec())
    }

    pub fn add_mapping(
        &self,
        reply_sender: Sender<bool>,
        host_addr: u64,
        guest_addr: u64,
        len: u64,
    ) {
        debug!("add_mapping: host_addr={host_addr:x}, guest_addr={guest_addr:x}, len={len}");
        if let Err(e) = self.hvf_vm.unmap_memory(guest_addr, len) {
            error!("Error removing memory map: {e:?}");
        }

        if let Err(e) = self.hvf_vm.map_memory(host_addr, guest_addr, len) {
            error!("Error adding memory map: {e:?}");
            reply_sender.send(false).unwrap();
        } else {
            reply_sender.send(true).unwrap();
        }
    }

    pub fn remove_mapping(&self, reply_sender: Sender<bool>, guest_addr: u64, len: u64) {
        debug!("remove_mapping: guest_addr={guest_addr:x}, len={len}");
        if let Err(e) = self.hvf_vm.unmap_memory(guest_addr, len) {
            error!("Error removing memory map: {e:?}");
            reply_sender.send(false).unwrap();
        } else {
            reply_sender.send(true).unwrap();
        }
    }
}

impl HvfDirtyTracker {
    fn begin(&self) -> Result<()> {
        if self.active.load(Ordering::Acquire) {
            return Ok(());
        }

        let regions = self
            .regions
            .lock()
            .expect("HVF dirty-region mutex poisoned");
        for region in regions.iter() {
            protect_memory(region.start(), region.length(), false)
                .map_err(Error::DirtyMemoryProtect)?;
        }
        self.dirty_pages
            .lock()
            .expect("HVF dirty-page mutex poisoned")
            .clear();
        self.active.store(true, Ordering::Release);
        Ok(())
    }

    fn take_dirty_ranges(&self) -> Result<Vec<GuestMemoryRange>> {
        if !self.active.load(Ordering::Acquire) {
            return Err(Error::DirtyTrackingInactive);
        }

        let mut pages = self
            .dirty_pages
            .lock()
            .expect("HVF dirty-page mutex poisoned");
        for page in pages.iter().copied() {
            protect_memory(page, self.page_size, false).map_err(Error::DirtyMemoryProtect)?;
        }

        let ranges = pages_to_ranges(pages.iter().copied(), self.page_size);
        pages.clear();
        Ok(ranges)
    }

    fn stop(&self) -> Result<()> {
        if !self.active.load(Ordering::Acquire) {
            return Ok(());
        }

        let regions = self
            .regions
            .lock()
            .expect("HVF dirty-region mutex poisoned");
        for region in regions.iter() {
            protect_memory(region.start(), region.length(), true)
                .map_err(Error::DirtyMemoryProtect)?;
        }
        self.dirty_pages
            .lock()
            .expect("HVF dirty-page mutex poisoned")
            .clear();
        self.active.store(false, Ordering::Release);
        Ok(())
    }

    fn handle_write_fault(&self, guest_addr: u64) -> Result<bool> {
        if !self.active.load(Ordering::Acquire) {
            return Ok(false);
        }

        let tracked = self
            .regions
            .lock()
            .expect("HVF dirty-region mutex poisoned")
            .iter()
            .any(|region| guest_addr >= region.start() && guest_addr < region.end());
        if !tracked {
            return Ok(false);
        }

        let page = guest_addr / self.page_size * self.page_size;
        self.dirty_pages
            .lock()
            .expect("HVF dirty-page mutex poisoned")
            .insert(page);
        protect_memory(page, self.page_size, true).map_err(Error::DirtyMemoryProtect)?;
        Ok(true)
    }
}

fn host_page_size() -> Result<u64> {
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if page_size <= 0 {
        Err(Error::DirtyPageSize(io::Error::last_os_error()))
    } else {
        Ok(page_size as u64)
    }
}

fn pages_to_ranges<I>(pages: I, page_size: u64) -> Vec<GuestMemoryRange>
where
    I: IntoIterator<Item = u64>,
{
    let mut ranges: Vec<GuestMemoryRange> = Vec::new();
    for page in pages {
        if let Some(previous) = ranges.last_mut() {
            if previous.end() == page {
                *previous = GuestMemoryRange::new(previous.start(), previous.length() + page_size)
                    .expect("coalesced HVF dirty range remains bounded");
                continue;
            }
        }
        ranges.push(
            GuestMemoryRange::new(page, page_size)
                .expect("HVF dirty pages are non-empty and bounded"),
        );
    }
    ranges
}

/// Encapsulates configuration parameters for the guest vCPUS.
#[derive(Debug, Eq, PartialEq)]
pub struct VcpuConfig {
    /// Number of guest VCPUs.
    pub vcpu_count: u8,
    /// Maximum possible guest VCPUs; vcpus above `vcpu_count` boot parked awaiting hotplug.
    pub max_vcpu_count: u8,
    /// Enable hyperthreading in the CPUID configuration.
    pub ht_enabled: bool,
    /// CPUID template to use.
    pub cpu_template: Option<CpuFeaturesTemplate>,
}

// Using this for easier explicit type-casting to help IDEs interpret the code.
type VcpuCell = Cell<Option<*const Vcpu>>;

/// A wrapper around creating and using a kvm-based VCPU.
pub struct Vcpu {
    id: u8,
    boot_entry_addr: u64,
    boot_receiver: Option<Receiver<u64>>,
    // Shared by every vCPU: a runtime CPU online (PSCI CPU_ON) can be issued from
    // whichever vCPU the guest scheduled the hotplug path on, not just the boot CPU.
    boot_senders: Option<Arc<HashMap<u64, Sender<u64>>>>,
    // Shared parked/offline flags (keyed by MPIDR) so AFFINITY_INFO can be answered
    // from any vCPU thread.
    parked_cpus: Option<Arc<HashMap<u64, AtomicBool>>>,
    // Host-authoritative online ceiling; vCPUs at or above it park between
    // emulation steps regardless of guest cooperation.
    enforcement: Option<Arc<devices::virtio::CpuEnforcement>>,
    fdt_addr: u64,
    mmio_bus: Option<devices::Bus>,
    #[cfg_attr(all(test, target_arch = "aarch64"), allow(unused))]
    exit_evt: EventFd,

    #[cfg(target_arch = "aarch64")]
    mpidr: u64,

    #[allow(unused)]
    event_receiver: Receiver<VcpuEvent>,
    // The transmitting end of the events channel which will be given to the handler.
    event_sender: Option<Sender<VcpuEvent>>,
    // The receiving end of the responses channel which will be given to the handler.
    response_receiver: Option<Receiver<VcpuResponse>>,
    // The transmitting end of the responses channel owned by the vcpu side.
    response_sender: Sender<VcpuResponse>,

    vcpu_list: Arc<VcpuList>,
    nested_enabled: bool,
    metrics: MetricsWriter,
    dirty_tracker: Arc<HvfDirtyTracker>,
    restored_state: bool,
}

#[derive(Deserialize, Serialize)]
struct MacVcpuExecutionState {
    hvf: HvfVcpuState,
    parked: bool,
}

impl Vcpu {
    thread_local!(static TLS_VCPU_PTR: VcpuCell = const { Cell::new(None) });

    /// Associates `self` with the current thread.
    ///
    /// It is a prerequisite to successfully run `init_thread_local_data()` before using
    /// `run_on_thread_local()` on the current thread.
    /// This function will return an error if there already is a `Vcpu` present in the TLS.
    fn init_thread_local_data(&mut self) -> Result<()> {
        Self::TLS_VCPU_PTR.with(|cell: &VcpuCell| {
            if cell.get().is_some() {
                return Err(Error::VcpuTlsInit);
            }
            cell.set(Some(self as *const Vcpu));
            Ok(())
        })
    }

    /// Deassociates `self` from the current thread.
    ///
    /// Should be called if the current `self` had called `init_thread_local_data()` and
    /// now needs to move to a different thread.
    ///
    /// Fails if `self` was not previously associated with the current thread.
    fn reset_thread_local_data(&mut self) -> Result<()> {
        // Best-effort to clean up TLS. If the `Vcpu` was moved to another thread
        // _before_ running this, then there is nothing we can do.
        Self::TLS_VCPU_PTR.with(|cell: &VcpuCell| {
            if let Some(vcpu_ptr) = cell.get() {
                if std::ptr::eq(vcpu_ptr, self) {
                    Self::TLS_VCPU_PTR.with(|cell: &VcpuCell| cell.take());
                    return Ok(());
                }
            }
            Err(Error::VcpuTlsNotPresent)
        })
    }

    /// Registers a signal handler which makes use of TLS and kvm immediate exit to
    /// kick the vcpu running on the current thread, if there is one.
    pub fn register_kick_signal_handler() {
        /*
        extern "C" fn handle_signal(_: c_int, _: *mut siginfo_t, _: *mut c_void) {
            // This is safe because it's temporarily aliasing the `Vcpu` object, but we are
            // only reading `vcpu.fd` which does not change for the lifetime of the `Vcpu`.
            unsafe {
                let _ = Vcpu::run_on_thread_local(|_vcpu| {
                    vcpu.fd.set_kvm_immediate_exit(1);
                    fence(Ordering::Release);
                });
            }
        }
        */

        //register_signal_handler(sigrtmin() + VCPU_RTSIG_OFFSET, handle_signal)
        //    .expect("Failed to register vcpu signal handler");
    }

    /// Constructs a new VCPU for `vm`.
    ///
    /// # Arguments
    ///
    /// * `id` - Represents the CPU number between [0, max vcpus).
    /// * `vm_fd` - The kvm `VmFd` for the virtual machine this vcpu will get attached to.
    /// * `exit_evt` - An `EventFd` that will be written into when this vcpu exits.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new_aarch64(
        id: u8,
        boot_entry_addr: GuestAddress,
        boot_receiver: Option<Receiver<u64>>,
        exit_evt: EventFd,
        vcpu_list: Arc<VcpuList>,
        dirty_tracker: Arc<HvfDirtyTracker>,
        nested_enabled: bool,
        metrics: MetricsWriter,
    ) -> Result<Self> {
        let (event_sender, event_receiver) = bounded(VCPU_CONTROL_MAILBOX_CAPACITY);
        let (response_sender, response_receiver) = bounded(VCPU_CONTROL_MAILBOX_CAPACITY);

        Ok(Vcpu {
            id,
            boot_entry_addr: boot_entry_addr.raw_value(),
            boot_receiver,
            boot_senders: None,
            parked_cpus: None,
            enforcement: None,
            fdt_addr: 0,
            mmio_bus: None,
            exit_evt,
            mpidr: id as u64,
            event_receiver,
            event_sender: Some(event_sender),
            response_receiver: Some(response_receiver),
            response_sender,
            vcpu_list,
            nested_enabled,
            metrics,
            dirty_tracker,
            restored_state: false,
        })
    }

    /// Returns the cpu index as seen by the guest OS.
    pub fn cpu_index(&self) -> u8 {
        self.id
    }

    /// Gets the MPIDR register value.
    pub fn get_mpidr(&self) -> u64 {
        self.mpidr
    }

    /// Sets a MMIO bus for this vcpu.
    pub fn set_mmio_bus(&mut self, mmio_bus: devices::Bus) {
        self.mmio_bus = Some(mmio_bus);
    }

    pub fn set_boot_senders(&mut self, boot_senders: Arc<HashMap<u64, Sender<u64>>>) {
        self.boot_senders = Some(boot_senders);
    }

    pub fn set_parked_cpus(&mut self, parked_cpus: Arc<HashMap<u64, AtomicBool>>) {
        self.parked_cpus = Some(parked_cpus);
    }

    pub fn set_enforcement(&mut self, enforcement: Arc<devices::virtio::CpuEnforcement>) {
        self.enforcement = Some(enforcement);
    }

    /// Record this vCPU's parked/offline state for AFFINITY_INFO queries.
    fn set_parked(&self, parked: bool) {
        if let Some(flag) = self
            .parked_cpus
            .as_ref()
            .and_then(|parked_cpus| parked_cpus.get(&self.mpidr))
        {
            flag.store(parked, Ordering::Release);
        }
    }

    fn is_parked(&self) -> bool {
        self.parked_cpus
            .as_ref()
            .and_then(|parked_cpus| parked_cpus.get(&self.mpidr))
            .is_some_and(|flag| flag.load(Ordering::Acquire))
    }

    /// Configures an aarch64 specific vcpu.
    ///
    /// # Arguments
    ///
    /// * `guest_mem` - The guest memory used by this microvm.
    pub fn configure_aarch64(&mut self, mem_info: &ArchMemoryInfo) -> Result<()> {
        self.fdt_addr = mem_info.fdt_addr;

        Ok(())
    }

    /// Moves the vcpu to its own thread and constructs a VcpuHandle.
    /// The handle can be used to control the remote vcpu.
    pub fn start_threaded(mut self) -> Result<VcpuHandle> {
        let event_sender = self.event_sender.take().unwrap();
        let response_receiver = self.response_receiver.take().unwrap();
        let (init_tls_sender, init_tls_receiver) = unbounded();

        let vcpu_thread = thread::Builder::new()
            .name(format!("fc_vcpu {}", self.cpu_index()))
            .spawn(move || {
                self.init_thread_local_data()
                    .expect("Cannot cleanly initialize vcpu TLS.");

                self.run(init_tls_sender);
            })
            .map_err(Error::VcpuSpawn)?;

        let hvf_vcpuid = init_tls_receiver
            .recv()
            .expect("Error waiting for TLS initialization.");

        Ok(VcpuHandle::new(
            hvf_vcpuid,
            event_sender,
            response_receiver,
            vcpu_thread,
        ))
    }

    /// Returns error or enum specifying whether emulation was handled or interrupted.
    fn run_emulation(&mut self, hvf_vcpu: &mut HvfVcpu) -> Result<VcpuEmulation> {
        let vcpuid = hvf_vcpu.id();

        match hvf_vcpu.run(self.vcpu_list.clone()) {
            Ok(exit) => match exit {
                VcpuExit::Breakpoint => {
                    debug!("vCPU {vcpuid} breakpoint");
                    Ok(VcpuEmulation::Interrupted)
                }
                VcpuExit::Canceled => {
                    debug!("vCPU {vcpuid} canceled");
                    Ok(VcpuEmulation::Interrupted)
                }
                VcpuExit::AffinityInfo(mpidr) => {
                    debug!("AffinityInfo: mpidr=0x{mpidr:x}");
                    let off = self
                        .parked_cpus
                        .as_ref()
                        .and_then(|parked| parked.get(&mpidr))
                        .is_some_and(|flag| flag.load(Ordering::Acquire));
                    // PSCI AFFINITY_INFO: 0 = ON, 1 = OFF.
                    hvf_vcpu
                        .write_psci_result(if off { 1 } else { 0 })
                        .map_err(Error::VcpuHvf)?;
                    Ok(VcpuEmulation::Handled)
                }
                VcpuExit::CpuOff => {
                    debug!("CpuOff: vCPU {vcpuid}");
                    Ok(VcpuEmulation::CpuOff)
                }
                VcpuExit::CpuOn(mpidr, entry, context_id) => {
                    debug!("CpuOn: mpidr=0x{mpidr:x} entry=0x{entry:x} context_id={context_id}");
                    if let Some(sender) = self
                        .boot_senders
                        .as_ref()
                        .and_then(|senders| senders.get(&mpidr))
                    {
                        sender.send(entry).unwrap()
                    } else {
                        error!("CPU_ON for unknown target mpidr=0x{mpidr:x}");
                    }
                    Ok(VcpuEmulation::Handled)
                }
                VcpuExit::HypervisorCall => {
                    debug!("vCPU {vcpuid} HVC");
                    Ok(VcpuEmulation::Handled)
                }
                VcpuExit::MmioRead(addr, data) => {
                    if let Some(ref mmio_bus) = self.mmio_bus {
                        debug!("vCPU {vcpuid} MMIO read 0x{addr:x}");
                        mmio_bus.read(vcpuid, addr, data);
                    }
                    Ok(VcpuEmulation::Handled)
                }
                VcpuExit::MmioWrite(addr, data) => {
                    if self.dirty_tracker.handle_write_fault(addr)? {
                        // The protected RAM write has not executed. Make the mapping writable and
                        // retry the original instruction instead of treating it as device MMIO.
                        hvf_vcpu.retry_current_instruction();
                        return Ok(VcpuEmulation::Handled);
                    }
                    if let Some(ref mmio_bus) = self.mmio_bus {
                        mmio_bus.write(vcpuid, addr, data);
                    }
                    Ok(VcpuEmulation::Handled)
                }
                VcpuExit::PsciHandled => {
                    debug!("vCPU {vcpuid} PSCI");
                    Ok(VcpuEmulation::Handled)
                }
                VcpuExit::SecureMonitorCall => {
                    debug!("vCPU {vcpuid} SMC");
                    Ok(VcpuEmulation::Handled)
                }
                VcpuExit::Shutdown => {
                    info!("vCPU {vcpuid} received shutdown signal");
                    Ok(VcpuEmulation::Stopped)
                }
                VcpuExit::SystemRegister => {
                    debug!("vCPU {vcpuid} accessed a system register");
                    Ok(VcpuEmulation::Handled)
                }
                VcpuExit::VtimerActivated => {
                    debug!("vCPU {vcpuid} VtimerActivated");
                    self.vcpu_list.set_vtimer_irq(vcpuid);
                    Ok(VcpuEmulation::Handled)
                }
                VcpuExit::WaitForEvent => {
                    debug!("vCPU {vcpuid} WaitForEvent");
                    Ok(VcpuEmulation::WaitForEvent)
                }
                VcpuExit::WaitForEventExpired => {
                    debug!("vCPU {vcpuid} WaitForEventExpired");
                    Ok(VcpuEmulation::WaitForEventExpired)
                }
                VcpuExit::WaitForEventTimeout(duration) => {
                    debug!("vCPU {vcpuid} WaitForEventTimeout timeout={duration:?}");
                    Ok(VcpuEmulation::WaitForEventTimeout(duration))
                }
            },
            Err(e) => panic!("Error running HVF vCPU: {e:?}"),
        }
    }

    /// Main loop of the vCPU thread.
    pub fn run(&mut self, init_tls_sender: Sender<u64>) {
        let mut hvf_vcpu =
            HvfVcpu::new(self.mpidr, self.nested_enabled).expect("Can't create HVF vCPU");
        let hvf_vcpuid = hvf_vcpu.id();

        init_tls_sender
            .send(hvf_vcpuid)
            .expect("Cannot notify vcpu TLS initialization.");

        let (wfe_sender, wfe_receiver) = unbounded();
        self.vcpu_list.register(hvf_vcpuid, wfe_sender);

        // All backends expose the same construction barrier: a newly started vCPU thread must not
        // execute guest instructions until the VMM explicitly resumes it.
        if !self.wait_for_resume(&mut hvf_vcpu) {
            return;
        }

        // Register with the enforcement kicker: a hard-spinning guest takes no VM exits on HVF (even the vtimer is hardware-virtualized), so when enforcement drops below
        // this vCPU's index the kicker must be able to force it out of guest mode for the enforcement check below to run at all — and to bound its slice while throttled.
        let kick_slot = self.enforcement.as_ref().map(|enforcement| {
            enforcement.register_kicker(
                self.id as u32,
                Box::new(move || {
                    let _ = vcpu_request_exit(hvf_vcpuid);
                }),
            )
        });

        if self.restored_state {
            // A CPU that was offline at capture remains offline. CPU_ON deliberately resets it to
            // the guest-provided entry point, exactly like an ordinary PSCI restart.
            if self.is_parked() {
                let Some(boot_receiver) = self.boot_receiver.clone() else {
                    self.exit(FC_EXIT_CODE_GENERIC_ERROR);
                    return;
                };
                let Some(entry) = self.wait_for_boot_entry(&mut hvf_vcpu, &boot_receiver) else {
                    return;
                };
                self.set_parked(false);
                hvf_vcpu
                    .set_initial_state(entry, self.fdt_addr)
                    .unwrap_or_else(|_| panic!("Can't restart restored HVF vCPU {hvf_vcpuid}"));
            }
        } else {
            // The boot CPU starts immediately at the kernel entry point; every other CPU parks on
            // its boot channel until PSCI CPU_ON supplies an entry point.
            let entry_addr = if self.id == 0 {
                self.boot_entry_addr
            } else if let Some(boot_receiver) = self.boot_receiver.clone() {
                self.set_parked(true);
                let Some(entry) = self.wait_for_boot_entry(&mut hvf_vcpu, &boot_receiver) else {
                    return;
                };
                self.set_parked(false);
                entry
            } else {
                self.boot_entry_addr
            };

            hvf_vcpu
                .set_initial_state(entry_addr, self.fdt_addr)
                .unwrap_or_else(|_| panic!("Can't set HVF vCPU {hvf_vcpuid} initial state"));
        }

        let mut last_exec_time_ns = hvf_vcpu.exec_time_ns().unwrap_or(0);
        let mut enforcement_deadline: Option<std::time::Instant> = None;
        loop {
            // Host-side enforcement: stop scheduling this vCPU while its index
            // is at or above the enforced online count. A cooperative guest
            // offlines it via PSCI first; an uncooperative one just stops
            // receiving execution time here.
            if let Some(enforcement) = &self.enforcement {
                if enforcement.runnable(self.id as u32) {
                    enforcement_deadline = None;
                } else {
                    // Give the guest a grace window to offline this CPU
                    // gracefully (the dying CPU must run its own PSCI CPU_OFF
                    // path); throttle it only if the guest refuses. Throttling
                    // (one emulation step per park interval) rather than fully
                    // parking keeps an uncooperative guest live: IPIs and TLB
                    // shootdowns aimed at this CPU still complete.
                    let deadline = *enforcement_deadline.get_or_insert_with(|| {
                        std::time::Instant::now() + devices::virtio::ENFORCEMENT_GRACE
                    });
                    if std::time::Instant::now() >= deadline {
                        enforcement.throttle(self.id as u32);
                    }
                }
            }
            if let Some(slot) = &kick_slot {
                slot.enter_guest();
            }
            let emulation = self.run_emulation(&mut hvf_vcpu);
            if let Some(slot) = &kick_slot {
                slot.leave_guest();
            }
            if let Some(exec_time_ns) = hvf_vcpu.exec_time_ns() {
                self.metrics
                    .add_vcpu_time_ns(exec_time_ns.saturating_sub(last_exec_time_ns));
                last_exec_time_ns = exec_time_ns;
            }

            match emulation {
                // Emulation ran successfully, continue.
                Ok(VcpuEmulation::Handled) => (),
                // The guest offlined this CPU (PSCI CPU_OFF). Park until a later
                // CPU_ON supplies a fresh entry point, then restart from it.
                Ok(VcpuEmulation::CpuOff) => {
                    if let Some(boot_receiver) = self.boot_receiver.clone() {
                        self.set_parked(true);
                        let Some(entry) = self.wait_for_boot_entry(&mut hvf_vcpu, &boot_receiver)
                        else {
                            break;
                        };
                        self.set_parked(false);
                        hvf_vcpu
                            .set_initial_state(entry, self.fdt_addr)
                            .unwrap_or_else(|_| {
                                panic!("Can't reset HVF vCPU {hvf_vcpuid} state after CPU_OFF")
                            });
                    } else {
                        error!(
                            "vCPU {} received CPU_OFF without a boot channel; stopping it",
                            self.id
                        );
                        break;
                    }
                }
                // Emulation was interrupted by a breakpoint.
                Ok(VcpuEmulation::Interrupted) => {
                    if !self.handle_pending_control(&mut hvf_vcpu) {
                        break;
                    }
                }
                // Wait for an external event.
                Ok(VcpuEmulation::WaitForEvent) => {
                    if !self.wait_for_event(&mut hvf_vcpu, &wfe_receiver, None) {
                        break;
                    }
                }
                Ok(VcpuEmulation::WaitForEventExpired) => (),
                Ok(VcpuEmulation::WaitForEventTimeout(timeout)) => {
                    if !self.wait_for_event(&mut hvf_vcpu, &wfe_receiver, Some(timeout)) {
                        break;
                    }
                }
                // The guest was rebooted or halted.
                Ok(VcpuEmulation::Stopped) => {
                    self.exit(FC_EXIT_CODE_OK);
                    break;
                }
                // Emulation errors lead to vCPU exit.
                Err(_) => {
                    self.exit(FC_EXIT_CODE_GENERIC_ERROR);
                    break;
                }
            }
        }
    }

    fn wait_for_event(
        &mut self,
        hvf_vcpu: &mut HvfVcpu,
        receiver: &Receiver<u32>,
        timeout: Option<Duration>,
    ) -> bool {
        let hvf_vcpuid = hvf_vcpu.id();
        if !self.vcpu_list.should_wait(hvf_vcpuid) {
            return true;
        }

        let control_receiver = self.event_receiver.clone();
        let deadline = timeout.map(|duration| Instant::now() + duration);

        loop {
            if let Some(deadline) = deadline {
                let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                    return true;
                };
                crossbeam_channel::select! {
                    recv(receiver) -> wake => {
                        wake.expect("WFE channel closed unexpectedly");
                        return true;
                    }
                    recv(control_receiver) -> event => {
                        let Ok(event) = event else {
                            return false;
                        };
                        if !self.handle_control_event(hvf_vcpu, event) {
                            return false;
                        }
                    }
                    default(remaining) => return true,
                }
            } else {
                crossbeam_channel::select! {
                    recv(receiver) -> wake => {
                        wake.expect("WFE channel closed unexpectedly");
                        return true;
                    }
                    recv(control_receiver) -> event => {
                        let Ok(event) = event else {
                            return false;
                        };
                        if !self.handle_control_event(hvf_vcpu, event) {
                            return false;
                        }
                    }
                }
            }
        }
    }

    fn wait_for_boot_entry(
        &mut self,
        hvf_vcpu: &mut HvfVcpu,
        receiver: &Receiver<u64>,
    ) -> Option<u64> {
        let control_receiver = self.event_receiver.clone();
        loop {
            crossbeam_channel::select! {
                recv(receiver) -> entry => return entry.ok(),
                recv(control_receiver) -> event => {
                    let Ok(event) = event else {
                        return None;
                    };
                    if !self.handle_control_event(hvf_vcpu, event) {
                        return None;
                    }
                }
            }
        }
    }

    fn handle_pending_control(&mut self, hvf_vcpu: &mut HvfVcpu) -> bool {
        match self.event_receiver.try_recv() {
            Ok(event) => self.handle_control_event(hvf_vcpu, event),
            Err(crossbeam_channel::TryRecvError::Empty) => true,
            Err(crossbeam_channel::TryRecvError::Disconnected) => false,
        }
    }

    fn handle_control_event(&mut self, hvf_vcpu: &mut HvfVcpu, event: VcpuEvent) -> bool {
        match event {
            VcpuEvent::Terminate => false,
            VcpuEvent::Pause { request_id } => {
                self.response_sender
                    .send(VcpuResponse::Paused { request_id })
                    .expect("failed to send pause status");
                self.wait_for_resume(hvf_vcpu)
            }
            VcpuEvent::Resume { request_id } => {
                self.response_sender
                    .send(VcpuResponse::Resumed { request_id })
                    .expect("failed to send resume status");
                true
            }
            VcpuEvent::CaptureState { request_id } => {
                let result = self.capture_execution_state(hvf_vcpu);
                self.response_sender
                    .send(VcpuResponse::StateCaptured { request_id, result })
                    .expect("failed to send execution-state capture status");
                true
            }
            VcpuEvent::RestoreState { request_id, bytes } => {
                let result = self.restore_execution_state(hvf_vcpu, &bytes);
                self.response_sender
                    .send(VcpuResponse::StateRestored { request_id, result })
                    .expect("failed to send execution-state restore status");
                true
            }
        }
    }

    fn wait_for_resume(&mut self, hvf_vcpu: &mut HvfVcpu) -> bool {
        loop {
            match self.event_receiver.recv() {
                Ok(VcpuEvent::Terminate) => return false,
                Ok(VcpuEvent::Resume { request_id }) => {
                    self.response_sender
                        .send(VcpuResponse::Resumed { request_id })
                        .expect("failed to send resume status");
                    return true;
                }
                Ok(VcpuEvent::Pause { request_id }) => {
                    self.response_sender
                        .send(VcpuResponse::Paused { request_id })
                        .expect("failed to send pause status");
                }
                Ok(VcpuEvent::CaptureState { request_id }) => {
                    let result = self.capture_execution_state(hvf_vcpu);
                    self.response_sender
                        .send(VcpuResponse::StateCaptured { request_id, result })
                        .expect("failed to send execution-state capture status");
                }
                Ok(VcpuEvent::RestoreState { request_id, bytes }) => {
                    let result = self.restore_execution_state(hvf_vcpu, &bytes);
                    self.response_sender
                        .send(VcpuResponse::StateRestored { request_id, result })
                        .expect("failed to send execution-state restore status");
                }
                Err(_) => return false,
            }
        }
    }

    fn capture_execution_state(&self, hvf_vcpu: &HvfVcpu) -> result::Result<Vec<u8>, String> {
        let state = MacVcpuExecutionState {
            hvf: hvf_vcpu
                .capture_state()
                .map_err(|error| error.to_string())?,
            parked: self.is_parked(),
        };
        bincode::serde::encode_to_vec(&state, bincode::config::standard())
            .map_err(|error| error.to_string())
    }

    fn restore_execution_state(
        &mut self,
        hvf_vcpu: &mut HvfVcpu,
        bytes: &[u8],
    ) -> result::Result<(), String> {
        let (state, consumed): (MacVcpuExecutionState, usize) =
            bincode::serde::decode_from_slice(bytes, bincode::config::standard())
                .map_err(|error| error.to_string())?;
        if consumed != bytes.len() {
            return Err("trailing vCPU state bytes".to_string());
        }
        hvf_vcpu
            .restore_state(&state.hvf)
            .map_err(|error| error.to_string())?;
        self.set_parked(state.parked);
        self.restored_state = true;
        Ok(())
    }

    fn exit(&mut self, exit_code: u8) {
        self.response_sender
            .send(VcpuResponse::Exited(exit_code))
            .expect("failed to send Exited status");

        if let Err(e) = self.exit_evt.write(1) {
            error!("Failed signaling vcpu exit event: {e}");
        }
    }
}

impl Drop for Vcpu {
    fn drop(&mut self) {
        let _ = self.reset_thread_local_data();
    }
}

// Allow currently unused Pause and Exit events. These will be used by the vmm later on.
#[allow(unused)]
#[derive(Debug)]
/// List of events that the Vcpu can receive.
pub enum VcpuEvent {
    /// Stop the vCPU thread without reporting a guest exit.
    Terminate,
    /// Pause the Vcpu.
    Pause {
        /// Correlates this command with its acknowledgement.
        request_id: VcpuControlRequestId,
    },
    /// Event that should resume the Vcpu.
    Resume {
        /// Correlates this command with its acknowledgement.
        request_id: VcpuControlRequestId,
    },
    /// Capture execution state on the owning vCPU thread.
    CaptureState {
        /// Correlates this command with its acknowledgement.
        request_id: VcpuControlRequestId,
    },
    /// Restore execution state on the owning vCPU thread.
    RestoreState {
        /// Correlates this command with its acknowledgement.
        request_id: VcpuControlRequestId,
        /// Backend-private execution-state bytes.
        bytes: Vec<u8>,
    },
}

#[derive(Debug, Eq, PartialEq)]
/// List of responses that the Vcpu reports.
pub enum VcpuResponse {
    /// Vcpu is paused.
    Paused {
        /// Request acknowledged by this response.
        request_id: VcpuControlRequestId,
    },
    /// Vcpu is resumed.
    Resumed {
        /// Request acknowledged by this response.
        request_id: VcpuControlRequestId,
    },
    /// A vCPU execution-state capture completed.
    StateCaptured {
        /// Request acknowledged by this response.
        request_id: VcpuControlRequestId,
        /// Encoded backend state or an explanatory backend error.
        result: result::Result<Vec<u8>, String>,
    },
    /// A vCPU execution-state restore completed.
    StateRestored {
        /// Request acknowledged by this response.
        request_id: VcpuControlRequestId,
        /// Restore result from the backend.
        result: result::Result<(), String>,
    },
    /// Vcpu is stopped.
    Exited(u8),
}

/// Wrapper over Vcpu that hides the underlying interactions with the Vcpu thread.
pub struct VcpuHandle {
    hvf_vcpuid: u64,
    event_sender: Sender<VcpuEvent>,
    response_receiver: Receiver<VcpuResponse>,
    vcpu_thread: Option<thread::JoinHandle<()>>,
}

impl VcpuHandle {
    pub fn new(
        hvf_vcpuid: u64,
        event_sender: Sender<VcpuEvent>,
        response_receiver: Receiver<VcpuResponse>,
        vcpu_thread: thread::JoinHandle<()>,
    ) -> Self {
        Self {
            hvf_vcpuid,
            event_sender,
            response_receiver,
            vcpu_thread: Some(vcpu_thread),
        }
    }

    pub fn send_event(&self, event: VcpuEvent) -> Result<()> {
        let should_interrupt = matches!(event, VcpuEvent::Pause { .. } | VcpuEvent::Terminate);
        // Use expect() to crash if the other thread closed this channel.
        self.event_sender
            .send(event)
            .expect("event sender channel closed on vcpu end.");
        if should_interrupt {
            self.kick()?;
        }
        Ok(())
    }

    pub fn kick(&self) -> Result<()> {
        vcpu_request_exit(self.hvf_vcpuid).map_err(Error::VcpuHvf)
    }

    pub fn response_receiver(&self) -> &Receiver<VcpuResponse> {
        &self.response_receiver
    }
}

impl Drop for VcpuHandle {
    fn drop(&mut self) {
        // Restore/startup failure can unwind while the vCPU is at the initial pause boundary or
        // running in HVF. Explicit termination avoids detaching a thread that still owns a vCPU.
        let _ = self.event_sender.send(VcpuEvent::Terminate);
        let _ = self.kick();
        if let Some(thread) = self.vcpu_thread.take() {
            let _ = thread.join();
        }
    }
}

enum VcpuEmulation {
    CpuOff,
    Handled,
    Interrupted,
    Stopped,
    WaitForEvent,
    WaitForEventExpired,
    WaitForEventTimeout(Duration),
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    #[cfg(target_arch = "x86_64")]
    use crossbeam_channel::{unbounded, RecvTimeoutError};
    use std::fs::File;
    #[cfg(target_arch = "x86_64")]
    use std::os::unix::io::AsRawFd;
    use std::sync::{Arc, Barrier};
    #[cfg(target_arch = "x86_64")]
    use std::time::Duration;

    use super::super::devices;
    use super::*;

    use utils::signal::validate_signal_num;

    // In tests we need to close any pending Vcpu threads on test completion.
    impl Drop for VcpuHandle {
        fn drop(&mut self) {
            // Make sure the Vcpu is out of KVM_RUN.
            self.send_event(VcpuEvent::Pause).unwrap();
            // Close the original channel so that the Vcpu thread errors and goes to exit state.
            let (event_sender, _event_receiver) = unbounded();
            self.event_sender = event_sender;
            // Wait for the Vcpu thread to finish execution
            self.vcpu_thread.take().unwrap().join().unwrap();
        }
    }

    // Auxiliary function being used throughout the tests.
    fn setup_vcpu(mem_size: usize) -> (Vm, Vcpu, GuestMemoryMmap) {
        let kvm = KvmContext::new().unwrap();
        let gm = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), mem_size)]).unwrap();
        let mut vm = Vm::new(kvm.fd()).expect("Cannot create new vm");
        assert!(vm.memory_init(&gm, kvm.max_memslots()).is_ok());

        let exit_evt = EventFd::new(utils::eventfd::EFD_NONBLOCK).unwrap();

        let vcpu;
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        {
            vm.setup_irqchip().unwrap();
            vcpu = Vcpu::new_x86_64(
                1,
                vm.fd(),
                vm.supported_cpuid().clone(),
                vm.supported_msrs().clone(),
                devices::Bus::new(),
                exit_evt,
            )
            .unwrap();
        }
        #[cfg(target_arch = "aarch64")]
        {
            vcpu = Vcpu::new_aarch64(1, vm.fd(), exit_evt).unwrap();
            vm.setup_irqchip(1).expect("Cannot setup irqchip");
        }

        (vm, vcpu, gm)
    }

    #[test]
    fn test_set_mmio_bus() {
        let (_, mut vcpu, _) = setup_vcpu(0x1000);
        assert!(vcpu.mmio_bus.is_none());
        vcpu.set_mmio_bus(devices::Bus::new());
        assert!(vcpu.mmio_bus.is_some());
    }

    #[test]
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    fn test_get_supported_cpuid() {
        let kvm = KvmContext::new().unwrap();
        let vm = Vm::new(kvm.fd()).expect("Cannot create new vm");
        let cpuid = kvm
            .kvm
            .get_supported_cpuid(KVM_MAX_CPUID_ENTRIES)
            .expect("Cannot get supported cpuid");
        assert_eq!(vm.supported_cpuid().as_slice(), cpuid.as_slice());
    }

    #[test]
    fn test_vm_memory_init() {
        let mut kvm_context = KvmContext::new().unwrap();
        let mut vm = Vm::new(kvm_context.fd()).expect("Cannot create new vm");

        // Create valid memory region and test that the initialization is successful.
        let gm = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x1000)]).unwrap();
        assert!(vm.memory_init(&gm, kvm_context.max_memslots()).is_ok());

        // Set the maximum number of memory slots to 1 in KvmContext to check the error
        // path of memory_init. Create 2 non-overlapping memory slots.
        kvm_context.max_memslots = 1;
        let gm = GuestMemoryMmap::from_ranges(&[
            (GuestAddress(0x0), 0x1000),
            (GuestAddress(0x1001), 0x2000),
        ])
        .unwrap();
        assert!(vm.memory_init(&gm, kvm_context.max_memslots()).is_err());
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn test_setup_irqchip() {
        let kvm_context = KvmContext::new().unwrap();
        let vm = Vm::new(kvm_context.fd()).expect("Cannot create new vm");

        vm.setup_irqchip().expect("Cannot setup irqchip");
        // Trying to setup two irqchips will result in EEXIST error. At the moment
        // there is no good way of testing the actual error because io::Error does not implement
        // PartialEq.
        assert!(vm.setup_irqchip().is_err());

        let _vcpu = Vcpu::new_x86_64(
            1,
            vm.fd(),
            vm.supported_cpuid().clone(),
            vm.supported_msrs().clone(),
            devices::Bus::new(),
            EventFd::new(utils::eventfd::EFD_NONBLOCK).unwrap(),
        )
        .unwrap();
        // Trying to setup irqchip after KVM_VCPU_CREATE was called will result in error.
        assert!(vm.setup_irqchip().is_err());
    }

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn test_setup_irqchip() {
        let kvm = KvmContext::new().unwrap();

        let mut vm = Vm::new(kvm.fd()).expect("Cannot create new vm");
        let vcpu_count = 1;
        let _vcpu = Vcpu::new_aarch64(
            1,
            vm.fd(),
            EventFd::new(utils::eventfd::EFD_NONBLOCK).unwrap(),
        )
        .unwrap();

        vm.setup_irqchip(vcpu_count).expect("Cannot setup irqchip");
        // Trying to setup two irqchips will result in EEXIST error.
        assert!(vm.setup_irqchip(vcpu_count).is_err());
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn test_configure_vcpu() {
        let (_vm, mut vcpu, vm_mem) = setup_vcpu(0x10000);

        let mut vcpu_config = VcpuConfig {
            vcpu_count: 1,
            max_vcpu_count: 1,
            ht_enabled: false,
            cpu_template: None,
        };

        assert!(vcpu
            .configure_x86_64(&vm_mem, GuestAddress(0), &vcpu_config)
            .is_ok());

        // Test configure while using the T2 template.
        vcpu_config.cpu_template = Some(CpuFeaturesTemplate::T2);
        assert!(vcpu
            .configure_x86_64(&vm_mem, GuestAddress(0), &vcpu_config)
            .is_ok());

        // Test configure while using the C3 template.
        vcpu_config.cpu_template = Some(CpuFeaturesTemplate::C3);
        assert!(vcpu
            .configure_x86_64(&vm_mem, GuestAddress(0), &vcpu_config)
            .is_ok());
    }

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn test_configure_vcpu() {
        let kvm = KvmContext::new().unwrap();
        let gm = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap();
        let mut vm = Vm::new(kvm.fd()).expect("new vm failed");
        assert!(vm.memory_init(&gm, kvm.max_memslots()).is_ok());

        // Try it for when vcpu id is 0.
        let mut vcpu = Vcpu::new_aarch64(
            0,
            vm.fd(),
            EventFd::new(utils::eventfd::EFD_NONBLOCK).unwrap(),
        )
        .unwrap();

        assert!(vcpu
            .configure_aarch64(vm.fd(), &gm, GuestAddress(0))
            .is_ok());

        // Try it for when vcpu id is NOT 0.
        let mut vcpu = Vcpu::new_aarch64(
            1,
            vm.fd(),
            EventFd::new(utils::eventfd::EFD_NONBLOCK).unwrap(),
        )
        .unwrap();

        assert!(vcpu
            .configure_aarch64(vm.fd(), &gm, GuestAddress(0))
            .is_ok());
    }

    #[test]
    fn test_kvm_context() {
        use std::os::unix::fs::MetadataExt;
        use std::os::unix::io::{AsRawFd, FromRawFd};

        let c = KvmContext::new().unwrap();

        assert!(c.max_memslots >= 32);

        let kvm = Kvm::new().unwrap();
        let f = unsafe { File::from_raw_fd(kvm.as_raw_fd()) };
        let m1 = f.metadata().unwrap();
        let m2 = File::open("/dev/kvm").unwrap().metadata().unwrap();

        assert_eq!(m1.dev(), m2.dev());
        assert_eq!(m1.ino(), m2.ino());
    }

    #[test]
    fn test_vcpu_tls() {
        let (_, mut vcpu, _) = setup_vcpu(0x1000);

        // Running on the TLS vcpu should fail before we actually initialize it.
        unsafe {
            assert!(Vcpu::run_on_thread_local(|_| ()).is_err());
        }

        // Initialize vcpu TLS.
        vcpu.init_thread_local_data().unwrap();

        // Validate TLS vcpu is the local vcpu by changing the `id` then validating against
        // the one in TLS.
        vcpu.id = 12;
        unsafe {
            assert!(Vcpu::run_on_thread_local(|v| assert_eq!(v.id, 12)).is_ok());
        }

        // Reset vcpu TLS.
        assert!(vcpu.reset_thread_local_data().is_ok());

        // Running on the TLS vcpu after TLS reset should fail.
        unsafe {
            assert!(Vcpu::run_on_thread_local(|_| ()).is_err());
        }

        // Second reset should return error.
        assert!(vcpu.reset_thread_local_data().is_err());
    }

    #[test]
    fn test_invalid_tls() {
        let (_, mut vcpu, _) = setup_vcpu(0x1000);
        // Initialize vcpu TLS.
        vcpu.init_thread_local_data().unwrap();
        // Trying to initialize non-empty TLS should error.
        vcpu.init_thread_local_data().unwrap_err();
    }

    #[test]
    fn test_vcpu_kick() {
        Vcpu::register_kick_signal_handler();
        let (vm, mut vcpu, _mem) = setup_vcpu(0x1000);

        let kvm_run =
            KvmRunWrapper::mmap_from_fd(&vcpu.fd, vm.fd.run_size()).expect("cannot mmap kvm-run");
        let success = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let vcpu_success = success.clone();
        let barrier = Arc::new(Barrier::new(2));
        let vcpu_barrier = barrier.clone();
        // Start Vcpu thread which will be kicked with a signal.
        let handle = std::thread::Builder::new()
            .name("test_vcpu_kick".to_string())
            .spawn(move || {
                vcpu.init_thread_local_data().unwrap();
                // Notify TLS was populated.
                vcpu_barrier.wait();
                // Loop for max 1 second to check if the signal handler has run.
                for _ in 0..10 {
                    if kvm_run.as_mut_ref().immediate_exit == 1 {
                        // Signal handler has run and set immediate_exit to 1.
                        vcpu_success.store(true, Ordering::Release);
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            })
            .expect("cannot start thread");

        // Wait for the vcpu to initialize its TLS.
        barrier.wait();
        // Kick the Vcpu using the custom signal.
        handle
            .kill(sigrtmin() + VCPU_RTSIG_OFFSET)
            .expect("failed to signal thread");
        handle.join().expect("failed to join thread");
        // Verify that the Vcpu saw its kvm immediate-exit as set.
        assert!(success.load(Ordering::Acquire));
    }

    #[cfg(target_arch = "x86_64")]
    // Sends an event to a vcpu and expects a particular response.
    fn queue_event_expect_response(handle: &VcpuHandle, event: VcpuEvent, response: VcpuResponse) {
        handle
            .send_event(event)
            .expect("failed to send event to vcpu");
        assert_eq!(
            handle
                .response_receiver()
                .recv_timeout(Duration::from_millis(100))
                .expect("did not receive event response from vcpu"),
            response
        );
    }

    #[cfg(target_arch = "x86_64")]
    // Sends an event to a vcpu and expects no response.
    fn queue_event_expect_timeout(handle: &VcpuHandle, event: VcpuEvent) {
        handle
            .send_event(event)
            .expect("failed to send event to vcpu");
        assert_eq!(
            handle
                .response_receiver()
                .recv_timeout(Duration::from_millis(100)),
            Err(RecvTimeoutError::Timeout)
        );
    }

    #[test]
    fn test_vcpu_rtsig_offset() {
        assert!(validate_signal_num(sigrtmin() + VCPU_RTSIG_OFFSET).is_ok());
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn test_vm_save_restore_state() {
        let kvm_fd = Kvm::new().unwrap();
        let vm = Vm::new(&kvm_fd).expect("new vm failed");
        // Irqchips, clock and pitstate are not configured so trying to save state should fail.
        assert!(vm.save_state().is_err());

        let (vm, _, _mem) = setup_vcpu(0x1000);
        let vm_state = vm.save_state().unwrap();
        assert_eq!(
            vm_state.pitstate.flags | KVM_PIT_SPEAKER_DUMMY,
            KVM_PIT_SPEAKER_DUMMY
        );
        assert_eq!(vm_state.clock.flags & KVM_CLOCK_TSC_STABLE, 0);
        assert_eq!(vm_state.pic_master.chip_id, KVM_IRQCHIP_PIC_MASTER);
        assert_eq!(vm_state.pic_slave.chip_id, KVM_IRQCHIP_PIC_SLAVE);
        assert_eq!(vm_state.ioapic.chip_id, KVM_IRQCHIP_IOAPIC);

        let (vm, _, _mem) = setup_vcpu(0x1000);
        assert!(vm.restore_state(&vm_state).is_ok());
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn test_vcpu_save_restore_state() {
        let (_vm, vcpu, _mem) = setup_vcpu(0x1000);
        let state = vcpu.save_state();
        assert!(state.is_ok());
        assert!(vcpu.restore_state(state.unwrap()).is_ok());

        unsafe { libc::close(vcpu.fd.as_raw_fd()) };
        let state = VcpuState {
            cpuid: CpuId::new(1),
            msrs: Msrs::new(1),
            debug_regs: Default::default(),
            lapic: Default::default(),
            mp_state: Default::default(),
            regs: Default::default(),
            sregs: Default::default(),
            vcpu_events: Default::default(),
            xcrs: Default::default(),
            xsave: Default::default(),
        };
        // Setting default state should always fail.
        assert!(vcpu.restore_state(state).is_err());
    }
}

#[cfg(all(test, target_arch = "aarch64"))]
mod tests {
    use super::*;

    fn test_vcpu(id: u8) -> Vcpu {
        Vcpu::new_aarch64(
            id,
            GuestAddress(0x1000),
            None,
            EventFd::new(utils::eventfd::EFD_NONBLOCK).unwrap(),
            Arc::new(VcpuList::new(1)),
            Arc::new(HvfDirtyTracker {
                regions: Mutex::new(Vec::new()),
                dirty_pages: Mutex::new(BTreeSet::new()),
                page_size: 0x4000,
                active: AtomicBool::new(false),
            }),
            false,
            MetricsWriter::default(),
        )
        .unwrap()
    }

    #[test]
    fn test_set_mmio_bus() {
        let mut vcpu = test_vcpu(0);

        assert!(vcpu.mmio_bus.is_none());
        vcpu.set_mmio_bus(devices::Bus::new());
        assert!(vcpu.mmio_bus.is_some());
    }

    #[test]
    fn test_configure_aarch64_records_fdt_addr() {
        let mut vcpu = test_vcpu(0);
        let mem_info = ArchMemoryInfo {
            fdt_addr: 0x2000,
            ..Default::default()
        };

        vcpu.configure_aarch64(&mem_info).unwrap();

        assert_eq!(vcpu.fdt_addr, 0x2000);
    }

    #[test]
    fn test_vcpu_tls_lifecycle() {
        let mut vcpu = test_vcpu(0);

        assert!(vcpu.reset_thread_local_data().is_err());
        assert!(vcpu.init_thread_local_data().is_ok());
        assert!(vcpu.init_thread_local_data().is_err());
        assert!(vcpu.reset_thread_local_data().is_ok());
        assert!(vcpu.reset_thread_local_data().is_err());
    }

    fn fake_vcpu_handle(paused: bool) -> (VcpuHandle, Receiver<()>) {
        let (event_sender, event_receiver) = unbounded();
        let (_response_sender, response_receiver) = unbounded();
        let (ready_sender, ready_receiver) = unbounded();
        let (stopped_sender, stopped_receiver) = unbounded();
        let vcpu_thread = thread::spawn(move || {
            ready_sender.send(()).unwrap();
            if paused {
                assert!(matches!(event_receiver.recv(), Ok(VcpuEvent::Terminate)));
            } else {
                loop {
                    match event_receiver.try_recv() {
                        Ok(VcpuEvent::Terminate) => break,
                        Ok(_) | Err(crossbeam_channel::TryRecvError::Empty) => {
                            thread::yield_now();
                        }
                        Err(crossbeam_channel::TryRecvError::Disconnected) => {
                            panic!("vCPU control channel disconnected before termination")
                        }
                    }
                }
            }
            stopped_sender.send(()).unwrap();
        });
        ready_receiver.recv().unwrap();
        (
            VcpuHandle::new(u64::MAX, event_sender, response_receiver, vcpu_thread),
            stopped_receiver,
        )
    }

    #[test]
    fn failed_startup_unwind_terminates_initially_paused_vcpu() {
        let (handle, stopped) = fake_vcpu_handle(true);

        drop(handle);

        assert_eq!(stopped.recv_timeout(Duration::from_secs(1)), Ok(()));
    }

    #[test]
    fn failed_restore_unwind_terminates_running_vcpu() {
        let (handle, stopped) = fake_vcpu_handle(false);

        drop(handle);

        assert_eq!(stopped.recv_timeout(Duration::from_secs(1)), Ok(()));
    }
}
