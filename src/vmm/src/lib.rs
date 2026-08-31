// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the THIRD-PARTY file.

//! Virtual Machine Monitor that leverages the Linux Kernel-based Virtual Machine (KVM),
//! and other virtualization features to run a single lightweight micro-virtual
//! machine (microVM).
//#![deny(missing_docs)]

#[macro_use]
extern crate log;

#[cfg(feature = "blk")]
use std::collections::BTreeSet;

/// Handles setup and initialization a `Vmm` object.
pub mod builder;
pub(crate) mod device_manager;
/// Typed reversible state for host-emulated devices.
pub mod device_state;
/// Cross-platform exit signal handlers (SIGTERM, SIGUSR1).
#[cfg(unix)]
pub mod exit_signal;
/// Resource store for configured microVM resources.
pub mod resources;
/// Signal handling utilities.
#[cfg(target_os = "linux")]
pub mod signal_handler;
/// Wrappers over structures used to configure the VMM.
pub mod vmm_config;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use crate::linux::vstate;
/// Versioned, backend-qualified execution-state artifacts.
pub mod execution_state;
#[cfg(target_os = "macos")]
mod macos;
/// Backend-neutral memory generation and incremental-baseline contracts.
pub mod memory_state;
mod metrics;
#[cfg(unix)]
mod terminal;
#[cfg(target_os = "windows")]
mod windows;
pub mod worker;

#[cfg(target_os = "macos")]
use macos::vstate;
#[cfg(target_os = "windows")]
use windows::vstate;

use std::fmt::{Display, Formatter};
use std::io;
#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(windows)]
use std::os::windows::io::{AsRawHandle, RawHandle};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[cfg(all(target_arch = "x86_64", not(target_os = "windows")))]
use crate::device_manager::legacy::PortIODeviceManager;
use crate::device_manager::mmio::MMIODeviceManager;
use crate::execution_state::{
    ExecutionArchitecture, ExecutionBackend, ExecutionState, VcpuExecutionState,
};
use crate::memory_state::{
    GuestMemoryRange, IncrementalCaptureDecision, MemoryBaselineToken, MemoryCaptureKind,
    MemoryCaptureOptions, MemoryCapturePlan, MemoryCaptureSink, MemoryCaptureStats,
    MemoryGenerationLedger,
};
use crate::vstate::VcpuEvent;
use crate::vstate::{Vcpu, VcpuHandle, VcpuResponse, Vm};
#[cfg(feature = "blk")]
use devices::virtio::{Block, MmioTransport, PreparedBlockBackend, TYPE_BLOCK};
use devices::virtio::{MemoryAccessDomain, MemoryAccessMode};

use crate::resources::VcpuPlacementResult;
use arch::{ArchMemoryInfo, InitrdConfig};
#[cfg(any(target_os = "macos", target_os = "windows"))]
use crossbeam_channel::Sender;
#[cfg(all(
    any(target_arch = "aarch64", target_arch = "riscv64"),
    any(target_arch = "aarch64", not(target_os = "windows"))
))]
use devices::fdt;
use devices::legacy::IrqChip;
use devices::virtio::VmmExitObserver;
use devices::{BusDevice, DeviceType};
use kernel::cmdline::Cmdline as KernelCmdline;
use polly::event_manager::{self, EventManager, Subscriber};
use utils::epoll::{EpollEvent, EventSet};
use utils::eventfd::EventFd;
use vm_memory::{
    Address, Bytes, GuestAddress, GuestMemoryBackend, GuestMemoryMmap, GuestMemoryRegion,
};

/// Success exit code.
pub const FC_EXIT_CODE_OK: u8 = 0;
/// Generic error exit code.
pub const FC_EXIT_CODE_GENERIC_ERROR: u8 = 1;
/// Generic exit code for an error considered not possible to occur if the program logic is sound.
pub const FC_EXIT_CODE_UNEXPECTED_ERROR: u8 = 2;
/// Firecracker was shut down after intercepting a restricted system call.
pub const FC_EXIT_CODE_BAD_SYSCALL: u8 = 148;
/// Firecracker was shut down after intercepting `SIGBUS`.
pub const FC_EXIT_CODE_SIGBUS: u8 = 149;
/// Firecracker was shut down after intercepting `SIGSEGV`.
pub const FC_EXIT_CODE_SIGSEGV: u8 = 150;
/// Bad configuration for microvm's resources, when using a single json.
pub const FC_EXIT_CODE_BAD_CONFIGURATION: u8 = 152;
/// Command line arguments parsing error.
pub const FC_EXIT_CODE_ARG_PARSING: u8 = 153;

const VCPU_CONTROL_TIMEOUT: Duration = Duration::from_millis(1000);
const DEFAULT_MAX_INCREMENTAL_DIRTY_PERCENT: u64 = 60;
pub(crate) const VCPU_CONTROL_MAILBOX_CAPACITY: usize = 4;

/// Correlates one asynchronous command sent to every vCPU thread.
///
/// Request ids are local to one `Vmm` instance. They prevent a delayed acknowledgement from an
/// earlier command from satisfying a later pause or resume barrier.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct VcpuControlRequestId(u64);

impl VcpuControlRequestId {
    const INITIAL: Self = Self(0);
    pub(crate) const TEARDOWN: Self = Self(u64::MAX);

    /// Returns the numeric request id for diagnostics.
    pub fn get(self) -> u64 {
        self.0
    }
}

/// Identifies one successfully established VM-wide paused boundary.
///
/// The value is meaningful only within the current `Vmm` instance. It is not a snapshot identity,
/// ownership token, or portable artifact field.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct PauseGeneration(VcpuControlRequestId);

impl PauseGeneration {
    const INITIAL: Self = Self(VcpuControlRequestId::INITIAL);

    /// Returns the control request that established this pause boundary.
    pub fn request_id(self) -> VcpuControlRequestId {
        self.0
    }
}

/// Current knowledge of the VM-wide execution boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VmmExecutionState {
    /// Every vCPU has acknowledged the contained pause generation.
    Paused(PauseGeneration),
    /// Every vCPU has acknowledged resume from the contained pause generation.
    Running {
        /// Pause generation from which execution most recently resumed.
        resumed_from: PauseGeneration,
    },
    /// A partially completed or timed-out barrier prevents safe state capture or resume.
    Indeterminate,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum VcpuControlTarget {
    Paused,
    Resumed,
}

fn wait_for_vcpu_response(
    receiver: &crossbeam_channel::Receiver<VcpuResponse>,
    request_id: VcpuControlRequestId,
    target: VcpuControlTarget,
    operation: &'static str,
    vcpu_index: usize,
    deadline: Instant,
) -> Result<()> {
    loop {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return Err(Error::VcpuControlTimeout {
                operation,
                request_id,
                vcpu_index,
            });
        };

        match receiver.recv_timeout(remaining) {
            Ok(VcpuResponse::Paused {
                request_id: response_id,
            }) if target == VcpuControlTarget::Paused && response_id == request_id => return Ok(()),
            Ok(VcpuResponse::Resumed {
                request_id: response_id,
            }) if target == VcpuControlTarget::Resumed && response_id == request_id => {
                return Ok(());
            }
            Ok(VcpuResponse::Paused {
                request_id: response_id,
            })
            | Ok(VcpuResponse::Resumed {
                request_id: response_id,
            }) if response_id < request_id => {
                debug!(
                    "ignoring stale vCPU {vcpu_index} response {} while waiting for {}",
                    response_id.get(),
                    request_id.get()
                );
            }
            Ok(VcpuResponse::Exited(exit_code)) => {
                return Err(Error::VcpuControlExited {
                    operation,
                    request_id,
                    vcpu_index,
                    exit_code,
                });
            }
            Ok(_) => {
                return Err(Error::VcpuControlProtocol {
                    operation,
                    request_id,
                    vcpu_index,
                });
            }
            Err(_) => {
                return Err(Error::VcpuControlTimeout {
                    operation,
                    request_id,
                    vcpu_index,
                });
            }
        }
    }
}

fn wait_for_captured_state(
    receiver: &crossbeam_channel::Receiver<VcpuResponse>,
    request_id: VcpuControlRequestId,
    vcpu_index: usize,
    deadline: Instant,
) -> Result<Vec<u8>> {
    loop {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return Err(Error::VcpuControlTimeout {
                operation: "execution-state capture",
                request_id,
                vcpu_index,
            });
        };
        match receiver.recv_timeout(remaining) {
            Ok(VcpuResponse::StateCaptured {
                request_id: response_id,
                result,
            }) if response_id == request_id => return result.map_err(Error::ExecutionStateBackend),
            Ok(VcpuResponse::Paused {
                request_id: response_id,
            })
            | Ok(VcpuResponse::Resumed {
                request_id: response_id,
            })
            | Ok(VcpuResponse::StateCaptured {
                request_id: response_id,
                ..
            })
            | Ok(VcpuResponse::StateRestored {
                request_id: response_id,
                ..
            }) if response_id < request_id => {
                debug!(
                    "ignoring stale vCPU {vcpu_index} response {} while capturing execution state",
                    response_id.get()
                );
            }
            Ok(VcpuResponse::Exited(exit_code)) => {
                return Err(Error::VcpuControlExited {
                    operation: "execution-state capture",
                    request_id,
                    vcpu_index,
                    exit_code,
                });
            }
            Ok(_) => {
                return Err(Error::VcpuControlProtocol {
                    operation: "execution-state capture",
                    request_id,
                    vcpu_index,
                });
            }
            Err(_) => {
                return Err(Error::VcpuControlTimeout {
                    operation: "execution-state capture",
                    request_id,
                    vcpu_index,
                });
            }
        }
    }
}

fn wait_for_restored_state(
    receiver: &crossbeam_channel::Receiver<VcpuResponse>,
    request_id: VcpuControlRequestId,
    vcpu_index: usize,
    deadline: Instant,
) -> Result<()> {
    loop {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return Err(Error::VcpuControlTimeout {
                operation: "execution-state restore",
                request_id,
                vcpu_index,
            });
        };
        match receiver.recv_timeout(remaining) {
            Ok(VcpuResponse::StateRestored {
                request_id: response_id,
                result,
            }) if response_id == request_id => return result.map_err(Error::ExecutionStateBackend),
            Ok(VcpuResponse::Paused {
                request_id: response_id,
            })
            | Ok(VcpuResponse::Resumed {
                request_id: response_id,
            })
            | Ok(VcpuResponse::StateCaptured {
                request_id: response_id,
                ..
            })
            | Ok(VcpuResponse::StateRestored {
                request_id: response_id,
                ..
            }) if response_id < request_id => {
                debug!(
                    "ignoring stale vCPU {vcpu_index} response {} while restoring execution state",
                    response_id.get()
                );
            }
            Ok(VcpuResponse::Exited(exit_code)) => {
                return Err(Error::VcpuControlExited {
                    operation: "execution-state restore",
                    request_id,
                    vcpu_index,
                    exit_code,
                });
            }
            Ok(_) => {
                return Err(Error::VcpuControlProtocol {
                    operation: "execution-state restore",
                    request_id,
                    vcpu_index,
                });
            }
            Err(_) => {
                return Err(Error::VcpuControlTimeout {
                    operation: "execution-state restore",
                    request_id,
                    vcpu_index,
                });
            }
        }
    }
}

#[cfg(target_arch = "x86_64")]
fn current_execution_architecture() -> ExecutionArchitecture {
    ExecutionArchitecture::X86_64
}

#[cfg(target_arch = "aarch64")]
fn current_execution_architecture() -> ExecutionArchitecture {
    ExecutionArchitecture::Aarch64
}

#[cfg(target_arch = "riscv64")]
fn current_execution_architecture() -> ExecutionArchitecture {
    ExecutionArchitecture::Riscv64
}

#[cfg(target_os = "linux")]
fn current_execution_backend() -> ExecutionBackend {
    ExecutionBackend::Kvm
}

#[cfg(target_os = "macos")]
fn current_execution_backend() -> ExecutionBackend {
    ExecutionBackend::Hvf
}

#[cfg(target_os = "windows")]
fn current_execution_backend() -> ExecutionBackend {
    ExecutionBackend::Whp
}

fn incremental_capture_is_not_beneficial(
    dirty_bytes: u64,
    memory_bytes: u64,
    max_dirty_percent: u64,
) -> bool {
    max_dirty_percent == 0
        || dirty_bytes.saturating_mul(100)
            >= memory_bytes.saturating_mul(max_dirty_percent.min(100))
}

/// Errors associated with the VMM internal logic. These errors cannot be generated by direct user
/// input, but can result from bad configuration of the host (for example if Firecracker doesn't
/// have permissions to open the KVM fd).
#[derive(Debug)]
pub enum Error {
    /// This error is thrown by the minimal boot loader implementation.
    ConfigureSystem(arch::Error),
    /// Legacy devices work with Event file descriptors and the creation can fail because
    /// of resource exhaustion.
    #[cfg(all(target_arch = "x86_64", not(target_os = "windows")))]
    CreateLegacyDevice(device_manager::legacy::Error),
    /// Cannot read from an Event file descriptor.
    EventFd(io::Error),
    /// Polly error wrapper.
    EventManager(event_manager::Error),
    /// I8042 Error.
    I8042Error(devices::legacy::I8042DeviceError),
    /// Cannot access kernel file.
    KernelFile(io::Error),
    /// Cannot open /dev/kvm. Either the host does not have KVM or Firecracker does not have
    /// permission to open the file descriptor.
    KvmContext(vstate::Error),
    #[cfg(all(target_arch = "x86_64", not(target_os = "windows")))]
    /// Cannot add devices to the Legacy I/O Bus.
    LegacyIOBus(device_manager::legacy::Error),
    /// Cannot add devices to the legacy port I/O bus.
    #[cfg(all(target_arch = "x86_64", target_os = "windows"))]
    LegacyPioBus(devices::BusError),
    /// Cannot load command line.
    LoadCommandline(kernel::cmdline::Error),
    /// Cannot add a device to the MMIO Bus.
    RegisterMMIODevice(device_manager::mmio::Error),
    /// Write to the serial console failed.
    Serial(io::Error),
    #[cfg(all(
        any(target_arch = "aarch64", target_arch = "riscv64"),
        any(target_arch = "aarch64", not(target_os = "windows"))
    ))]
    /// Cannot generate or write FDT
    SetupFDT(devices::fdt::Error),
    /// Cannot create Timer file descriptor.
    TimerFd(io::Error),
    /// Vcpu error.
    Vcpu(vstate::Error),
    /// Cannot send event to vCPU.
    VcpuEvent(vstate::Error),
    /// Cannot create a vCPU handle.
    VcpuHandle(vstate::Error),
    /// vCPU resume failed.
    VcpuResume,
    /// The VM-wide execution state cannot be changed safely after an uncertain control barrier.
    VcpuControlIndeterminate,
    /// The local vCPU control request counter is exhausted.
    VcpuControlRequestExhausted,
    /// Resume named a pause generation other than the current execution boundary.
    PauseGenerationMismatch {
        /// Generation supplied by the caller.
        requested: PauseGeneration,
        /// Generation associated with the current paused or running state.
        current: PauseGeneration,
    },
    /// A vCPU exited while acknowledging a VM-wide control barrier.
    VcpuControlExited {
        /// Requested operation.
        operation: &'static str,
        /// Correlation id of the requested operation.
        request_id: VcpuControlRequestId,
        /// Index of the vCPU handle that reported exit.
        vcpu_index: usize,
        /// Exit code reported by the vCPU.
        exit_code: u8,
    },
    /// A vCPU did not acknowledge a VM-wide control barrier before its deadline.
    VcpuControlTimeout {
        /// Requested operation.
        operation: &'static str,
        /// Correlation id of the requested operation.
        request_id: VcpuControlRequestId,
        /// Index of the vCPU handle that missed the deadline.
        vcpu_index: usize,
    },
    /// A vCPU returned a response that cannot belong to the active control barrier.
    VcpuControlProtocol {
        /// Requested operation.
        operation: &'static str,
        /// Correlation id of the requested operation.
        request_id: VcpuControlRequestId,
        /// Index of the vCPU handle that returned the invalid response.
        vcpu_index: usize,
    },
    /// A memory-generation state transition was invalid.
    MemoryState(memory_state::Error),
    /// An execution-state artifact was malformed.
    ExecutionState(execution_state::Error),
    /// A backend execution-state operation failed.
    ExecutionStateBackend(String),
    /// Execution-state capture and restore require a complete paused boundary.
    ExecutionStateRequiresPause,
    /// Execution-state restore is permitted only before first activation.
    ExecutionRestoreAfterActivation,
    /// Memory restore is permitted only before first activation.
    MemoryRestoreAfterActivation,
    /// The artifact targets a different backend, architecture, or backend ABI.
    ExecutionStateIncompatible,
    /// The artifact vCPU topology differs from the constructed VMM.
    ExecutionStateTopologyMismatch,
    /// Guest memory could not be read or written for capture/materialization.
    MemoryAccess(vm_memory::GuestMemoryError),
    /// A streaming memory sink rejected a chunk.
    MemorySink(io::Error),
    /// Memory generation work requires a complete VM-wide paused boundary.
    MemoryCaptureRequiresPause,
    /// Execution cannot resume while a memory generation awaits publication or abandonment.
    MemoryCapturePending,
    /// Materialized byte length did not match its declared guest-memory range.
    MemoryLengthMismatch,
    /// The host guest-memory access epoch could not transition safely.
    MemoryAccessDomain(devices::virtio::memory_access::Error),
    /// Device-state work requires a complete VM-wide paused boundary.
    DeviceStateRequiresPause,
    /// A typed device-state operation failed.
    DeviceState(String),
    /// The requested reversible device does not exist in this VMM.
    DeviceStateNotFound(String),
    /// Cannot spawn a new Vcpu thread.
    VcpuSpawn(std::io::Error),
    /// Vm error.
    Vm(vstate::Error),
    /// Error thrown by observer object on Vmm initialization.
    VmmObserverInit(utils::errno::Error),
    /// Error thrown by observer object on Vmm teardown.
    VmmObserverTeardown(utils::errno::Error),
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        use self::Error::*;

        match self {
            ConfigureSystem(e) => write!(f, "System configuration error: {e:?}"),
            #[cfg(all(target_arch = "x86_64", not(target_os = "windows")))]
            CreateLegacyDevice(e) => write!(f, "Error creating legacy device: {e:?}"),
            EventFd(e) => write!(f, "Event fd error: {e}"),
            EventManager(e) => write!(f, "Event manager error: {e:?}"),
            I8042Error(e) => write!(f, "I8042 error: {e}"),
            KernelFile(e) => write!(f, "Cannot access kernel file: {e}"),
            KvmContext(e) => write!(f, "Failed to validate KVM support: {e:?}"),
            #[cfg(all(target_arch = "x86_64", not(target_os = "windows")))]
            LegacyIOBus(e) => write!(f, "Cannot add devices to the legacy I/O Bus. {e}"),
            #[cfg(all(target_arch = "x86_64", target_os = "windows"))]
            LegacyPioBus(e) => write!(f, "Cannot add devices to the legacy port I/O bus. {e}"),
            LoadCommandline(e) => write!(f, "Cannot load command line: {e}"),
            RegisterMMIODevice(e) => write!(f, "Cannot add a device to the MMIO Bus. {e}"),
            Serial(e) => write!(f, "Error writing to the serial console: {e:?}"),
            #[cfg(all(
                any(target_arch = "aarch64", target_arch = "riscv64"),
                any(target_arch = "aarch64", not(target_os = "windows"))
            ))]
            SetupFDT(e) => write!(f, "Error generating or writing FDT: {e:?}"),
            TimerFd(e) => write!(f, "Error creating timer fd: {e}"),
            Vcpu(e) => write!(f, "Vcpu error: {e}"),
            VcpuEvent(e) => write!(f, "Cannot send event to vCPU. {e:?}"),
            VcpuHandle(e) => write!(f, "Cannot create a vCPU handle. {e}"),
            VcpuResume => write!(f, "vCPUs resume failed."),
            VcpuControlIndeterminate => write!(
                f,
                "vCPU execution state is indeterminate after an incomplete control barrier"
            ),
            VcpuControlRequestExhausted => write!(f, "vCPU control request ids are exhausted"),
            PauseGenerationMismatch { requested, current } => write!(
                f,
                "pause generation {} does not match current generation {}",
                requested.request_id().get(),
                current.request_id().get()
            ),
            VcpuControlExited {
                operation,
                request_id,
                vcpu_index,
                exit_code,
            } => write!(
                f,
                "vCPU {vcpu_index} exited with code {exit_code} during {operation} request {}",
                request_id.get()
            ),
            VcpuControlTimeout {
                operation,
                request_id,
                vcpu_index,
            } => write!(
                f,
                "vCPU {vcpu_index} timed out during {operation} request {}",
                request_id.get()
            ),
            VcpuControlProtocol {
                operation,
                request_id,
                vcpu_index,
            } => write!(
                f,
                "vCPU {vcpu_index} returned an invalid response during {operation} request {}",
                request_id.get()
            ),
            MemoryState(e) => write!(f, "Memory generation error: {e}"),
            ExecutionState(e) => write!(f, "Execution-state artifact error: {e}"),
            ExecutionStateBackend(e) => write!(f, "Backend execution-state error: {e}"),
            ExecutionStateRequiresPause => {
                write!(f, "execution-state work requires a complete VM-wide pause")
            }
            ExecutionRestoreAfterActivation => write!(
                f,
                "execution state may be restored only before the VMM is first activated"
            ),
            MemoryRestoreAfterActivation => write!(
                f,
                "memory may be materialized only before the VMM is first activated"
            ),
            ExecutionStateIncompatible => write!(
                f,
                "execution state is incompatible with this backend or architecture"
            ),
            ExecutionStateTopologyMismatch => write!(
                f,
                "execution-state vCPU topology does not match the constructed VMM"
            ),
            MemoryAccess(e) => write!(f, "Guest memory access failed: {e:?}"),
            MemorySink(e) => write!(f, "Memory capture sink failed: {e}"),
            MemoryCaptureRequiresPause => {
                write!(f, "memory capture requires a complete VM-wide pause")
            }
            MemoryCapturePending => write!(
                f,
                "cannot resume while a memory capture awaits publication or abandonment"
            ),
            MemoryLengthMismatch => write!(
                f,
                "materialized memory length does not match its guest-physical range"
            ),
            MemoryAccessDomain(error) => {
                write!(f, "guest-memory access transition failed: {error}")
            }
            DeviceStateRequiresPause => {
                write!(f, "device-state work requires a complete VM-wide pause")
            }
            DeviceState(error) => write!(f, "device-state operation failed: {error}"),
            DeviceStateNotFound(id) => write!(f, "reversible device {id} was not found"),
            VcpuSpawn(e) => write!(f, "Cannot spawn Vcpu thread: {e}"),
            Vm(e) => write!(f, "Vm error: {e}"),
            VmmObserverInit(e) => write!(
                f,
                "Error thrown by observer object on Vmm initialization: {e}"
            ),
            VmmObserverTeardown(e) => {
                write!(f, "Error thrown by observer object on Vmm teardown: {e}")
            }
        }
    }
}

/// Trait for objects that need custom initialization and teardown during the Vmm lifetime.
pub trait VmmEventsObserver {
    /// This function will be called during microVm boot.
    fn on_vmm_boot(&mut self) -> std::result::Result<(), utils::errno::Error> {
        Ok(())
    }
    /// This function will be called on microVm teardown.
    fn on_vmm_stop(&mut self) -> std::result::Result<(), utils::errno::Error> {
        Ok(())
    }
}

/// Shorthand result type for internal VMM commands.
pub type Result<T> = std::result::Result<T, Error>;

/// Contains the state and associated methods required for the Firecracker VMM.
pub struct Vmm {
    // Guest VM core resources.
    guest_memory: GuestMemoryMmap,
    arch_memory_info: ArchMemoryInfo,

    kernel_cmdline: KernelCmdline,

    vcpus_handles: Vec<VcpuHandle>,
    control_request_id: VcpuControlRequestId,
    execution_state: VmmExecutionState,
    execution_restore_allowed: bool,
    memory_restore_allowed: bool,
    memory_ledger: MemoryGenerationLedger,
    memory_access: MemoryAccessDomain,
    memory_tracking_active: bool,
    pending_dirty_ranges: Vec<GuestMemoryRange>,
    carried_dirty_ranges: Vec<GuestMemoryRange>,
    pending_access_mode: Option<MemoryAccessMode>,
    exit_evt: EventFd,
    vm: Vm,
    exit_observers: Vec<Arc<Mutex<dyn VmmExitObserver>>>,
    exit_code: Arc<AtomicI32>,

    // Guest VM devices.
    mmio_device_manager: MMIODeviceManager,
    #[cfg(feature = "blk")]
    quiesced_block_devices: BTreeSet<String>,
    #[cfg(all(target_arch = "x86_64", not(target_os = "windows")))]
    pio_device_manager: PortIODeviceManager,
}

impl Vmm {
    /// Gets the the specified bus device.
    pub fn get_bus_device(
        &self,
        device_type: DeviceType,
        device_id: &str,
    ) -> Option<&Mutex<dyn BusDevice>> {
        self.mmio_device_manager.get_device(device_type, device_id)
    }

    /// Captures one virtio-block device and keeps its worker quiesced until VM resume.
    #[cfg(feature = "blk")]
    pub fn capture_block_device_state(
        &mut self,
        device_id: &str,
    ) -> Result<device_state::BlockDeviceState> {
        self.require_paused_device_boundary()?;
        let pause_generation = match self.execution_state {
            VmmExecutionState::Paused(generation) => generation.request_id().get(),
            _ => unreachable!("paused boundary was checked before capture"),
        };
        let bus_device = self
            .mmio_device_manager
            .get_device(DeviceType::Virtio(TYPE_BLOCK), device_id)
            .ok_or_else(|| Error::DeviceStateNotFound(device_id.to_string()))?;
        let mut bus_device = bus_device
            .lock()
            .map_err(|_| Error::DeviceState("MMIO device mutex is poisoned".to_string()))?;
        let transport = bus_device
            .as_mut_any()
            .downcast_mut::<MmioTransport>()
            .ok_or_else(|| Error::DeviceState("block device is not on virtio-mmio".to_string()))?;

        // Record intent immediately before entering the participant. If drain or flush fails after
        // the worker has stopped, ordinary VM resume must reconcile this device instead of
        // releasing vCPUs against a silently parked backend.
        self.quiesced_block_devices.insert(device_id.to_string());
        transport
            .quiesce()
            .map_err(|error| Error::DeviceState(error.to_string()))?;
        let transport_state = transport
            .capture_state()
            .map_err(|error| Error::DeviceState(error.to_string()))?;
        let device = transport.device();
        let device = device
            .lock()
            .map_err(|_| Error::DeviceState("virtio device mutex is poisoned".to_string()))?;
        let block = device
            .as_any()
            .downcast_ref::<Block>()
            .ok_or_else(|| Error::DeviceState("virtio device is not block".to_string()))?;
        let device_state = block
            .capture_state()
            .map_err(|error| Error::DeviceState(error.to_string()))?;
        Ok(device_state::BlockDeviceState {
            pause_generation,
            transport: transport_state,
            device: device_state,
        })
    }

    /// Restores one virtio-block device before the paused VM is allowed to run.
    #[cfg(feature = "blk")]
    pub fn restore_block_device_state(
        &mut self,
        device_id: &str,
        state: &device_state::BlockDeviceState,
    ) -> Result<()> {
        self.require_paused_device_boundary()?;
        let bus_device = self
            .mmio_device_manager
            .get_device(DeviceType::Virtio(TYPE_BLOCK), device_id)
            .ok_or_else(|| Error::DeviceStateNotFound(device_id.to_string()))?;
        let mut bus_device = bus_device
            .lock()
            .map_err(|_| Error::DeviceState("MMIO device mutex is poisoned".to_string()))?;
        let transport = bus_device
            .as_mut_any()
            .downcast_mut::<MmioTransport>()
            .ok_or_else(|| Error::DeviceState("block device is not on virtio-mmio".to_string()))?;
        self.quiesced_block_devices.insert(device_id.to_string());
        transport
            .quiesce()
            .map_err(|error| Error::DeviceState(error.to_string()))?;

        let device = transport.device();
        {
            let mut device = device
                .lock()
                .map_err(|_| Error::DeviceState("virtio device mutex is poisoned".to_string()))?;
            let block = device
                .as_mut_any()
                .downcast_mut::<Block>()
                .ok_or_else(|| Error::DeviceState("virtio device is not block".to_string()))?;
            block
                .validate_state(&state.device)
                .map_err(|error| Error::DeviceState(error.to_string()))?;
        }
        transport
            .restore_state(&state.transport)
            .map_err(|error| Error::DeviceState(error.to_string()))
    }

    /// Installs an already opened backend behind one quiesced virtio-block device.
    #[cfg(feature = "blk")]
    pub fn replace_block_backend(
        &mut self,
        device_id: &str,
        backend: PreparedBlockBackend,
    ) -> Result<()> {
        self.require_paused_device_boundary()?;
        let bus_device = self
            .mmio_device_manager
            .get_device(DeviceType::Virtio(TYPE_BLOCK), device_id)
            .ok_or_else(|| Error::DeviceStateNotFound(device_id.to_string()))?;
        let mut bus_device = bus_device
            .lock()
            .map_err(|_| Error::DeviceState("MMIO device mutex is poisoned".to_string()))?;
        let transport = bus_device
            .as_mut_any()
            .downcast_mut::<MmioTransport>()
            .ok_or_else(|| Error::DeviceState("block device is not on virtio-mmio".to_string()))?;
        self.quiesced_block_devices.insert(device_id.to_string());
        transport
            .quiesce()
            .map_err(|error| Error::DeviceState(error.to_string()))?;

        let device = transport.device();
        {
            let mut device = device
                .lock()
                .map_err(|_| Error::DeviceState("virtio device mutex is poisoned".to_string()))?;
            let block = device
                .as_mut_any()
                .downcast_mut::<Block>()
                .ok_or_else(|| Error::DeviceState("virtio device is not block".to_string()))?;
            block
                .replace_backend(backend)
                .map_err(|error| Error::DeviceState(error.to_string()))
        }
    }

    #[cfg(feature = "blk")]
    fn require_paused_device_boundary(&self) -> Result<()> {
        if !matches!(self.execution_state, VmmExecutionState::Paused(_)) {
            return Err(Error::DeviceStateRequiresPause);
        }
        Ok(())
    }

    #[cfg(feature = "blk")]
    fn resume_quiesced_block_devices(&mut self) -> Result<()> {
        for device_id in self
            .quiesced_block_devices
            .iter()
            .cloned()
            .collect::<Vec<_>>()
        {
            let bus_device = self
                .mmio_device_manager
                .get_device(DeviceType::Virtio(TYPE_BLOCK), &device_id)
                .ok_or_else(|| Error::DeviceStateNotFound(device_id.clone()))?;
            let mut bus_device = bus_device
                .lock()
                .map_err(|_| Error::DeviceState("MMIO device mutex is poisoned".to_string()))?;
            let transport = bus_device
                .as_mut_any()
                .downcast_mut::<MmioTransport>()
                .ok_or_else(|| {
                    Error::DeviceState("block device is not on virtio-mmio".to_string())
                })?;
            // Quiesce is idempotent and also reconciles a prior stop whose durability fence failed.
            transport
                .quiesce()
                .map_err(|error| Error::DeviceState(error.to_string()))?;
            transport
                .resume()
                .map_err(|error| Error::DeviceState(error.to_string()))?;
            self.quiesced_block_devices.remove(&device_id);
        }
        Ok(())
    }

    /// Starts the microVM vcpus.
    pub fn start_vcpus(&mut self, vcpus: Vec<Vcpu>) -> Result<()> {
        self.start_vcpus_paused(vcpus)?;

        // The vcpus start off in the `Paused` state, let them run.
        self.resume_vcpus()?;

        Ok(())
    }

    /// Starts every vCPU thread and applies host affinity, but keeps the guest paused.
    ///
    /// The returned report is a barrier: every entry describes the effective affinity before any
    /// guest instruction can execute. Callers may reconcile cooperative reservations and then call
    /// [`resume_vcpus`](Self::resume_vcpus).
    pub fn start_vcpus_paused(&mut self, mut vcpus: Vec<Vcpu>) -> Result<Vec<VcpuPlacementResult>> {
        let vcpu_count = vcpus.len();
        let mut placement = Vec::with_capacity(vcpu_count);

        Vcpu::register_kick_signal_handler();

        self.vcpus_handles.reserve(vcpu_count);

        for mut vcpu in vcpus.drain(..) {
            vcpu.set_mmio_bus(self.mmio_device_manager.bus.clone());

            #[cfg(any(target_os = "linux", target_os = "windows"))]
            {
                let (handle, result) = vcpu.start_threaded().map_err(Error::VcpuHandle)?;
                self.vcpus_handles.push(handle);
                placement.push(result);
            }
            #[cfg(target_os = "macos")]
            {
                let vcpu_index = vcpu.cpu_index();
                self.vcpus_handles
                    .push(vcpu.start_threaded().map_err(Error::VcpuHandle)?);
                placement.push(VcpuPlacementResult::Inherited {
                    vcpu_index,
                    requested_host_cpu: None,
                    reason: None,
                });
            }
        }

        Ok(placement)
    }

    /// Pauses every vCPU and returns the generation shared by the completed barrier.
    pub fn pause_vcpus(&mut self) -> Result<PauseGeneration> {
        match self.execution_state {
            VmmExecutionState::Paused(generation) => return Ok(generation),
            VmmExecutionState::Running { .. } => {}
            VmmExecutionState::Indeterminate => return Err(Error::VcpuControlIndeterminate),
        }

        let request_id = self.next_control_request_id()?;
        self.execution_state = VmmExecutionState::Indeterminate;

        for handle in &self.vcpus_handles {
            handle
                .send_event(VcpuEvent::Pause { request_id })
                .map_err(Error::VcpuEvent)?;
        }

        self.wait_for_vcpu_barrier(request_id, VcpuControlTarget::Paused)?;
        let generation = PauseGeneration(request_id);
        self.execution_state = VmmExecutionState::Paused(generation);
        Ok(generation)
    }

    /// Resumes every vCPU from the currently established paused generation.
    pub fn resume_vcpus(&mut self) -> Result<()> {
        let generation = match self.execution_state {
            VmmExecutionState::Running { .. } => return Ok(()),
            VmmExecutionState::Paused(generation) => generation,
            VmmExecutionState::Indeterminate => return Err(Error::VcpuControlIndeterminate),
        };
        self.resume_vcpus_from(generation)
    }

    /// Resumes every vCPU only if `generation` is the current paused boundary.
    ///
    /// Repeating the command after a successful resume is idempotent for the same generation.
    pub fn resume_vcpus_from(&mut self, generation: PauseGeneration) -> Result<()> {
        if self.memory_ledger.has_pending_capture() {
            return Err(Error::MemoryCapturePending);
        }
        match self.execution_state {
            VmmExecutionState::Running { resumed_from } if resumed_from == generation => {
                return Ok(());
            }
            VmmExecutionState::Running { resumed_from } => {
                return Err(Error::PauseGenerationMismatch {
                    requested: generation,
                    current: resumed_from,
                });
            }
            VmmExecutionState::Paused(current) if current == generation => {}
            VmmExecutionState::Paused(current) => {
                return Err(Error::PauseGenerationMismatch {
                    requested: generation,
                    current,
                });
            }
            VmmExecutionState::Indeterminate => return Err(Error::VcpuControlIndeterminate),
        }

        // Device participants reopen before vCPUs so saved queue work cannot race a guest that is
        // already executing. Any reconciliation failure leaves the VM at the paused generation.
        #[cfg(feature = "blk")]
        self.resume_quiesced_block_devices()?;

        let request_id = self.next_control_request_id()?;
        self.execution_state = VmmExecutionState::Indeterminate;

        for handle in &self.vcpus_handles {
            handle
                .send_event(VcpuEvent::Resume { request_id })
                .map_err(Error::VcpuEvent)?;
        }

        self.wait_for_vcpu_barrier(request_id, VcpuControlTarget::Resumed)?;
        self.execution_state = VmmExecutionState::Running {
            resumed_from: generation,
        };
        self.execution_restore_allowed = false;
        self.memory_restore_allowed = false;
        Ok(())
    }

    /// Returns the current VM-wide execution-state knowledge.
    pub fn execution_state(&self) -> VmmExecutionState {
        self.execution_state
    }

    /// Captures backend execution state from every owning vCPU thread.
    pub fn capture_execution_state(&mut self) -> Result<ExecutionState> {
        let pause_generation = self.require_paused_execution_boundary()?;
        let request_id = self.next_control_request_id()?;
        for handle in &self.vcpus_handles {
            handle
                .send_event(VcpuEvent::CaptureState { request_id })
                .map_err(Error::VcpuEvent)?;
        }

        let deadline = Instant::now() + VCPU_CONTROL_TIMEOUT;
        let mut vcpus = Vec::with_capacity(self.vcpus_handles.len());
        for (vcpu_index, handle) in self.vcpus_handles.iter().enumerate() {
            let bytes = wait_for_captured_state(
                handle.response_receiver(),
                request_id,
                vcpu_index,
                deadline,
            )?;
            let bytes = self
                .vm
                .complete_vcpu_execution_capture(vcpu_index as u32, bytes)
                .map_err(Error::Vm)?;
            vcpus.push(
                VcpuExecutionState::new(vcpu_index as u32, bytes).map_err(Error::ExecutionState)?,
            );
        }
        let vm_state = self.vm.capture_execution_state().map_err(Error::Vm)?;
        ExecutionState::new(
            current_execution_architecture(),
            current_execution_backend(),
            1,
            pause_generation.request_id().get(),
            vm_state,
            vcpus,
        )
        .map_err(Error::ExecutionState)
    }

    /// Restores execution state into a constructed VMM before its first activation.
    pub fn restore_execution_state(&mut self, state: &ExecutionState) -> Result<()> {
        self.require_paused_execution_boundary()?;
        if !self.execution_restore_allowed {
            return Err(Error::ExecutionRestoreAfterActivation);
        }
        if state.architecture() != current_execution_architecture()
            || state.backend() != current_execution_backend()
            || state.backend_state_abi() != 1
        {
            return Err(Error::ExecutionStateIncompatible);
        }
        if state.vcpus().len() != self.vcpus_handles.len()
            || state
                .vcpus()
                .iter()
                .enumerate()
                .any(|(index, vcpu)| vcpu.id() != index as u32)
        {
            return Err(Error::ExecutionStateTopologyMismatch);
        }

        self.execution_state = VmmExecutionState::Indeterminate;
        self.vm
            .restore_execution_state(state.vm_state())
            .map_err(Error::Vm)?;
        let request_id = self.next_control_request_id()?;
        for (handle, vcpu) in self.vcpus_handles.iter().zip(state.vcpus()) {
            let bytes = self
                .vm
                .prepare_vcpu_execution_restore(vcpu.id(), vcpu.bytes())
                .map_err(Error::Vm)?;
            handle
                .send_event(VcpuEvent::RestoreState { request_id, bytes })
                .map_err(Error::VcpuEvent)?;
        }
        let deadline = Instant::now() + VCPU_CONTROL_TIMEOUT;
        for (vcpu_index, handle) in self.vcpus_handles.iter().enumerate() {
            wait_for_restored_state(handle.response_receiver(), request_id, vcpu_index, deadline)?;
        }
        self.execution_restore_allowed = false;
        self.execution_state = VmmExecutionState::Paused(PauseGeneration::INITIAL);
        Ok(())
    }

    /// Plans a complete guest-memory generation at the current paused boundary.
    pub fn plan_full_memory_capture(&mut self) -> Result<MemoryCapturePlan> {
        self.require_paused_memory_boundary()?;
        if self.memory_ledger.has_pending_capture() {
            return Err(Error::MemoryCapturePending);
        }
        let previous = self
            .memory_access
            .freeze(VCPU_CONTROL_TIMEOUT)
            .map_err(Error::MemoryAccessDomain)?;
        let plan = match self
            .memory_ledger
            .plan_full_capture()
            .map_err(Error::MemoryState)
        {
            Ok(plan) => plan,
            Err(error) => {
                self.memory_access.resume_mode(previous);
                return Err(error);
            }
        };
        self.pending_access_mode = Some(previous);
        Ok(plan)
    }

    /// Plans a delta relative to the latest retained baseline.
    ///
    /// A rejected or unavailable baseline returns `FullRequired` without harvesting the active
    /// backend generation, so the caller may immediately select a complete capture.
    pub fn plan_incremental_memory_capture(
        &mut self,
        baseline: MemoryBaselineToken,
    ) -> Result<IncrementalCaptureDecision> {
        self.plan_incremental_memory_capture_with_threshold(
            baseline,
            DEFAULT_MAX_INCREMENTAL_DIRTY_PERCENT,
        )
    }

    /// Plans a delta but selects complete capture when changed coverage reaches `max_dirty_percent`.
    pub fn plan_incremental_memory_capture_with_threshold(
        &mut self,
        baseline: MemoryBaselineToken,
        max_dirty_percent: u64,
    ) -> Result<IncrementalCaptureDecision> {
        self.require_paused_memory_boundary()?;
        if self.memory_ledger.has_pending_capture() {
            return Err(Error::MemoryCapturePending);
        }
        if let Some(reason) = self.memory_ledger.incremental_full_reason(baseline) {
            return Ok(IncrementalCaptureDecision::FullRequired(reason));
        }

        let previous = self
            .memory_access
            .freeze(VCPU_CONTROL_TIMEOUT)
            .map_err(Error::MemoryAccessDomain)?;
        let mut changed_ranges = match self.vm.take_dirty_ranges().map_err(Error::Vm) {
            Ok(ranges) => ranges,
            Err(error) => {
                self.memory_access.resume_mode(previous);
                return Err(error);
            }
        };
        changed_ranges.append(&mut self.carried_dirty_ranges);
        for range in self.memory_access.take_dirty_ranges() {
            changed_ranges.push(
                GuestMemoryRange::new(range.start, range.length)
                    .expect("virtqueue inventories contain checked non-empty ranges"),
            );
        }
        self.pending_dirty_ranges = changed_ranges.clone();
        let decision = match self
            .memory_ledger
            .plan_incremental_capture(baseline, changed_ranges)
            .map_err(Error::MemoryState)
        {
            Ok(decision) => decision,
            Err(error) => {
                // The backend generation has already been harvested. Preserve it and reopen the
                // exact preceding host-access mode so a later retry cannot miss these writes.
                self.carried_dirty_ranges
                    .append(&mut self.pending_dirty_ranges);
                self.memory_access.resume_mode(previous);
                return Err(error);
            }
        };
        match decision {
            IncrementalCaptureDecision::Incremental(plan) => {
                self.pending_dirty_ranges = plan.changed_ranges().to_vec();
                self.pending_access_mode = Some(previous);
                let dirty_bytes = plan
                    .changed_ranges()
                    .iter()
                    .map(|range| range.length())
                    .sum::<u64>();
                let memory_bytes = self
                    .guest_memory
                    .iter()
                    .map(|region| region.len())
                    .sum::<u64>();
                let selects_complete = incremental_capture_is_not_beneficial(
                    dirty_bytes,
                    memory_bytes,
                    max_dirty_percent,
                );
                if selects_complete {
                    if let Err(error) = self
                        .memory_ledger
                        .abandon(&plan)
                        .map_err(Error::MemoryState)
                    {
                        self.carried_dirty_ranges
                            .append(&mut self.pending_dirty_ranges);
                        self.memory_access.resume_mode(previous);
                        return Err(error);
                    }
                    let capture = match self
                        .memory_ledger
                        .plan_full_capture()
                        .map_err(Error::MemoryState)
                    {
                        Ok(capture) => capture,
                        Err(error) => {
                            self.carried_dirty_ranges
                                .append(&mut self.pending_dirty_ranges);
                            self.memory_access.resume_mode(previous);
                            return Err(error);
                        }
                    };
                    Ok(IncrementalCaptureDecision::Complete {
                        capture,
                        reason: memory_state::FullCaptureReason::DeltaNotBeneficial,
                    })
                } else {
                    Ok(IncrementalCaptureDecision::Incremental(plan))
                }
            }
            IncrementalCaptureDecision::FullRequired(reason) => {
                // Readiness was checked before harvesting, so this path can only result from an
                // internal contract violation. Preserve all harvested ranges conservatively.
                self.carried_dirty_ranges
                    .append(&mut self.pending_dirty_ranges);
                self.memory_access.resume_mode(previous);
                Ok(IncrementalCaptureDecision::FullRequired(reason))
            }
            IncrementalCaptureDecision::Complete { .. } => {
                unreachable!("the ledger does not apply capture policy")
            }
        }
    }

    /// Streams a pending memory generation in bounded guest-physical order.
    pub fn capture_memory(
        &self,
        capture: &MemoryCapturePlan,
        options: MemoryCaptureOptions,
        sink: &mut dyn MemoryCaptureSink,
    ) -> Result<MemoryCaptureStats> {
        self.require_paused_memory_boundary()?;
        self.memory_ledger
            .validate_pending(capture)
            .map_err(Error::MemoryState)?;

        let ranges = match capture.kind() {
            MemoryCaptureKind::Full => self
                .guest_memory
                .iter()
                .map(|region| {
                    GuestMemoryRange::new(region.start_addr().raw_value(), region.len())
                        .expect("registered guest-memory regions are non-empty and bounded")
                })
                .collect::<Vec<_>>(),
            MemoryCaptureKind::Incremental { .. } => capture.changed_ranges().to_vec(),
        };

        let mut buffer = vec![0_u8; options.chunk_size()];
        let mut stats = MemoryCaptureStats::default();
        for range in ranges {
            let mut start = range.start();
            let mut remaining = range.length();
            while remaining > 0 {
                let length = remaining.min(options.chunk_size() as u64) as usize;
                let bytes = &mut buffer[..length];
                self.guest_memory
                    .read_slice(bytes, GuestAddress(start))
                    .map_err(Error::MemoryAccess)?;
                let chunk_range = GuestMemoryRange::new(start, length as u64)
                    .expect("bounded memory chunks cannot overflow");
                if options.detects_zero() && bytes.iter().all(|byte| *byte == 0) {
                    sink.write_zero(chunk_range).map_err(Error::MemorySink)?;
                    stats.zero_bytes += length as u64;
                } else {
                    sink.write_bytes(chunk_range, bytes)
                        .map_err(Error::MemorySink)?;
                    stats.emitted_bytes += length as u64;
                }
                stats.logical_bytes += length as u64;
                stats.chunks += 1;
                start += length as u64;
                remaining -= length as u64;
            }
        }
        Ok(stats)
    }

    /// Materializes exact bytes into an inert or paused guest-memory range.
    pub fn materialize_memory(&mut self, range: GuestMemoryRange, bytes: &[u8]) -> Result<()> {
        self.require_paused_memory_boundary()?;
        if !self.memory_restore_allowed {
            return Err(Error::MemoryRestoreAfterActivation);
        }
        if range.length() != bytes.len() as u64 {
            return Err(Error::MemoryLengthMismatch);
        }
        self.guest_memory
            .write_slice(bytes, GuestAddress(range.start()))
            .map_err(Error::MemoryAccess)
    }

    /// Materializes an all-zero range into inert or paused guest memory.
    pub fn materialize_zero_memory(&mut self, range: GuestMemoryRange) -> Result<()> {
        self.require_paused_memory_boundary()?;
        if !self.memory_restore_allowed {
            return Err(Error::MemoryRestoreAfterActivation);
        }
        let mut start = range.start();
        let mut remaining = range.length();
        let zeroes = vec![0_u8; (remaining.min(2 * 1024 * 1024)) as usize];
        while remaining > 0 {
            let length = remaining.min(zeroes.len() as u64) as usize;
            self.guest_memory
                .write_slice(&zeroes[..length], GuestAddress(start))
                .map_err(Error::MemoryAccess)?;
            start += length as u64;
            remaining -= length as u64;
        }
        Ok(())
    }

    /// Accepts a captured generation after the caller durably publishes its objects and manifest.
    pub fn publish_memory_capture(
        &mut self,
        capture: &MemoryCapturePlan,
    ) -> Result<MemoryBaselineToken> {
        self.require_paused_memory_boundary()?;
        self.memory_ledger
            .validate_pending(capture)
            .map_err(Error::MemoryState)?;
        if self.memory_tracking_active {
            if capture.kind() == MemoryCaptureKind::Full {
                // Discard the preceding generation only after the complete replacement has been
                // produced. The backend starts its next generation atomically with this harvest.
                let _ = self.vm.take_dirty_ranges().map_err(Error::Vm)?;
            }
        } else {
            self.vm.begin_dirty_tracking().map_err(Error::Vm)?;
            self.memory_tracking_active = true;
        }

        // Open host writers only after the backend CPU tracker covers the next generation.
        self.memory_access
            .begin_tracking()
            .map_err(Error::MemoryAccessDomain)?;

        let baseline = self
            .memory_ledger
            .publish(capture)
            .map_err(Error::MemoryState)?;
        self.memory_ledger
            .retain_dirty_coverage(baseline)
            .map_err(Error::MemoryState)?;
        self.pending_dirty_ranges.clear();
        self.carried_dirty_ranges.clear();
        self.pending_access_mode = None;
        Ok(baseline)
    }

    /// Abandons a candidate while retaining every harvested dirty range for a safe retry.
    pub fn abandon_memory_capture(&mut self, capture: &MemoryCapturePlan) -> Result<()> {
        self.require_paused_memory_boundary()?;
        self.memory_ledger
            .abandon(capture)
            .map_err(Error::MemoryState)?;
        self.carried_dirty_ranges
            .append(&mut self.pending_dirty_ranges);
        if let Some(previous) = self.pending_access_mode.take() {
            self.memory_access.resume_mode(previous);
        }
        Ok(())
    }

    /// Releases the retained baseline and removes backend tracking overhead.
    pub fn release_memory_baseline(&mut self) -> Result<()> {
        self.require_paused_memory_boundary()?;
        if self.memory_ledger.has_pending_capture() {
            return Err(Error::MemoryCapturePending);
        }
        let previous = self
            .memory_access
            .freeze(VCPU_CONTROL_TIMEOUT)
            .map_err(Error::MemoryAccessDomain)?;
        if self.memory_tracking_active {
            if let Err(error) = self.vm.stop_dirty_tracking().map_err(Error::Vm) {
                self.memory_access.resume_mode(previous);
                return Err(error);
            }
            self.memory_tracking_active = false;
        }
        self.memory_access
            .resume_resident()
            .map_err(Error::MemoryAccessDomain)?;
        self.memory_ledger.invalidate_dirty_coverage();
        self.pending_dirty_ranges.clear();
        self.carried_dirty_ranges.clear();
        self.pending_access_mode = None;
        Ok(())
    }

    /// Returns the currently retained incremental baseline, if any.
    pub fn retained_memory_baseline(&self) -> Option<MemoryBaselineToken> {
        self.memory_ledger.retained_baseline()
    }

    /// Returns the shared host-memory access epoch used when attaching virtio transports.
    pub(crate) fn memory_access_domain(&self) -> MemoryAccessDomain {
        self.memory_access.clone()
    }

    fn require_paused_memory_boundary(&self) -> Result<PauseGeneration> {
        match self.execution_state {
            VmmExecutionState::Paused(generation) => Ok(generation),
            VmmExecutionState::Running { .. } | VmmExecutionState::Indeterminate => {
                Err(Error::MemoryCaptureRequiresPause)
            }
        }
    }

    fn require_paused_execution_boundary(&self) -> Result<PauseGeneration> {
        match self.execution_state {
            VmmExecutionState::Paused(generation) => Ok(generation),
            VmmExecutionState::Running { .. } | VmmExecutionState::Indeterminate => {
                Err(Error::ExecutionStateRequiresPause)
            }
        }
    }

    fn next_control_request_id(&mut self) -> Result<VcpuControlRequestId> {
        let next = self
            .control_request_id
            .get()
            .checked_add(1)
            .filter(|next| *next < VcpuControlRequestId::TEARDOWN.get())
            .ok_or(Error::VcpuControlRequestExhausted)?;
        let request_id = VcpuControlRequestId(next);
        self.control_request_id = request_id;
        Ok(request_id)
    }

    fn wait_for_vcpu_barrier(
        &self,
        request_id: VcpuControlRequestId,
        target: VcpuControlTarget,
    ) -> Result<()> {
        let deadline = Instant::now() + VCPU_CONTROL_TIMEOUT;
        let operation = match target {
            VcpuControlTarget::Paused => "pause",
            VcpuControlTarget::Resumed => "resume",
        };

        for (vcpu_index, handle) in self.vcpus_handles.iter().enumerate() {
            loop {
                let retry_deadline = deadline.min(Instant::now() + Duration::from_millis(10));
                match wait_for_vcpu_response(
                    handle.response_receiver(),
                    request_id,
                    target,
                    operation,
                    vcpu_index,
                    retry_deadline,
                ) {
                    Ok(()) => break,
                    Err(Error::VcpuControlTimeout { .. })
                        if target == VcpuControlTarget::Paused && Instant::now() < deadline =>
                    {
                        // HVF only exits vCPUs that are inside hv_vcpu_run at the instant of the
                        // request. Re-kicking closes the host-side gap between consecutive runs;
                        // it is harmless on KVM and WHP and keeps the barrier backend-neutral.
                        handle.kick().map_err(Error::VcpuEvent)?;
                    }
                    Err(error) => return Err(error),
                }
            }
        }

        Ok(())
    }

    /// Configures the system for boot.
    pub fn configure_system(
        &self,
        vcpus: &[Vcpu],
        _intc: &IrqChip,
        initrd: &Option<InitrdConfig>,
        _smbios_oem_strings: &Option<Vec<String>>,
    ) -> Result<()> {
        #[cfg(all(target_os = "windows", not(target_arch = "aarch64")))]
        let _ = (vcpus, initrd);

        #[cfg(target_arch = "x86_64")]
        {
            let ioapic_num_pins = _intc.lock().unwrap().num_pins();
            let cmdline_len = if cfg!(feature = "tee") {
                arch::x86_64::layout::CMDLINE_SEV_SIZE
            } else {
                self.kernel_cmdline.len() + 1
            };

            arch::x86_64::configure_system(
                &self.guest_memory,
                &self.arch_memory_info,
                vm_memory::GuestAddress(arch::x86_64::layout::CMDLINE_START),
                cmdline_len,
                initrd,
                vcpus.len() as u8,
                ioapic_num_pins,
            )
            .map_err(Error::ConfigureSystem)?;
        }

        #[cfg(target_arch = "aarch64")]
        {
            let vcpu_mpidr = vcpus.iter().map(|cpu| cpu.get_mpidr()).collect();
            fdt::create_fdt(
                &self.guest_memory,
                &self.arch_memory_info,
                vcpu_mpidr,
                self.kernel_cmdline.as_str(),
                self.mmio_device_manager.get_device_info(),
                _intc,
                initrd,
            )
            .map_err(Error::SetupFDT)?;
        }

        #[cfg(target_arch = "aarch64")]
        {
            arch::aarch64::configure_system(
                &self.guest_memory,
                &self.arch_memory_info,
                _smbios_oem_strings,
            )
            .map_err(Error::ConfigureSystem)?;
        }

        #[cfg(all(target_arch = "riscv64", not(target_os = "windows")))]
        {
            fdt::create_fdt(
                &self.guest_memory,
                &self.arch_memory_info,
                vcpus.len() as u32,
                self.kernel_cmdline.as_str(),
                self.mmio_device_manager.get_device_info(),
                _intc,
                initrd,
            )
            .map_err(Error::SetupFDT)?;

            arch::riscv64::configure_system(&self.guest_memory, _smbios_oem_strings)
                .map_err(Error::ConfigureSystem)?;
        }

        Ok(())
    }

    /// Returns a reference to the inner `GuestMemoryMmap` object if present, or `None` otherwise.
    pub fn guest_memory(&self) -> &GuestMemoryMmap {
        &self.guest_memory
    }

    /// Adds an exit observer that will be called on graceful guest-initiated shutdown.
    pub fn add_exit_observer(&mut self, observer: impl VmmExitObserver + 'static) {
        self.exit_observers.push(Arc::new(Mutex::new(observer)));
    }

    /// Injects CTRL+ALT+DEL keystroke combo in the i8042 device.
    #[cfg(all(target_arch = "x86_64", not(target_os = "windows")))]
    pub fn send_ctrl_alt_del(&mut self) -> Result<()> {
        self.pio_device_manager
            .i8042
            .lock()
            .expect("i8042 lock was poisoned")
            .trigger_ctrl_alt_del()
            .map_err(Error::I8042Error)
    }

    /// Invokes all registered exit observers.
    ///
    /// Each observer is wrapped in `catch_unwind` so that a panic in one
    /// observer does not prevent subsequent observers from running.
    pub fn notify_exit_observers(&mut self, exit_code: i32) {
        for observer in &self.exit_observers {
            let obs = Arc::clone(observer);
            if let Err(e) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                obs.lock()
                    .expect("Poisoned mutex for exit observer")
                    .on_vmm_exit(exit_code);
            })) {
                error!("Exit observer panicked: {e:?}");
            }
        }
    }

    /// Invokes exit observers and terminates the process.
    pub fn stop(&mut self, exit_code: i32) {
        info!("Vmm is stopping.");

        self.notify_exit_observers(exit_code);

        // Exit from Firecracker using the provided exit code. Safe because we're terminating
        // the process anyway.
        #[cfg(unix)]
        unsafe {
            libc::_exit(exit_code);
        }
        #[cfg(windows)]
        std::process::exit(exit_code);
    }

    /// Returns a reference to the inner KVM Vm object.
    pub fn kvm_vm(&self) -> &Vm {
        &self.vm
    }

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    pub fn add_mapping(
        &self,
        reply_sender: Sender<bool>,
        host_addr: u64,
        guest_addr: u64,
        len: u64,
    ) {
        self.vm
            .add_mapping(reply_sender, host_addr, guest_addr, len);
    }

    #[cfg(target_os = "windows")]
    pub fn add_mapping_with_writable(
        &self,
        reply_sender: Sender<bool>,
        host_addr: u64,
        guest_addr: u64,
        len: u64,
        writable: bool,
    ) {
        self.vm
            .add_mapping_with_writable(reply_sender, host_addr, guest_addr, len, writable);
    }

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    pub fn remove_mapping(&self, reply_sender: Sender<bool>, guest_addr: u64, len: u64) {
        self.vm.remove_mapping(reply_sender, guest_addr, len);
    }

    #[cfg(unix)]
    fn is_exit_event_source(&self, source: std::os::fd::RawFd) -> bool {
        source == self.exit_evt.as_raw_fd()
    }

    #[cfg(windows)]
    fn is_exit_event_source(&self, source: RawHandle) -> bool {
        source == self.exit_evt.as_raw_handle()
    }

    #[cfg(unix)]
    fn exit_event_token(&self) -> u64 {
        self.exit_evt.as_raw_fd() as u64
    }

    #[cfg(windows)]
    fn exit_event_token(&self) -> u64 {
        self.exit_evt.as_raw_handle() as usize as u64
    }
}

impl Subscriber for Vmm {
    /// Handle a read event (EPOLLIN).
    fn process(&mut self, event: &EpollEvent, _: &mut EventManager) {
        let source = event.fd();
        let event_set = event.event_set();

        if self.is_exit_event_source(source) && event_set == EventSet::IN {
            let _ = self.exit_evt.read();
            // Query each vcpu for the exit_code.
            // If the exit_code can't be found on any vcpu, it means that the exit signal
            // has been issued by the i8042 controller in which case we exit with
            // FC_EXIT_CODE_OK.
            //
            // The exit code set up by the guest takes preference over the one reported
            // by either a vcpu or the i8042 controller.
            let vcpu_exit_code = self
                .vcpus_handles
                .iter()
                .find_map(|handle| match handle.response_receiver().try_recv() {
                    Ok(VcpuResponse::Exited(exit_code)) => Some(exit_code),
                    _ => None,
                })
                .unwrap_or(FC_EXIT_CODE_OK);
            let vmm_exit_code = self.exit_code.load(Ordering::SeqCst);
            let exit_code = if vmm_exit_code != i32::MAX {
                debug!("using vmm exit code: {vmm_exit_code}");
                vmm_exit_code
            } else {
                debug!("using vcpu exit code: {vcpu_exit_code}");
                vcpu_exit_code as i32
            };
            self.stop(exit_code);
        } else {
            error!("Spurious EventManager event for handler: Vmm");
        }
    }

    fn interest_list(&self) -> Vec<EpollEvent> {
        vec![EpollEvent::new(EventSet::IN, self.exit_event_token())]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request_id(value: u64) -> VcpuControlRequestId {
        VcpuControlRequestId(value)
    }

    #[test]
    fn control_barrier_ignores_stale_acknowledgements() {
        let (sender, receiver) = crossbeam_channel::bounded(2);
        sender
            .send(VcpuResponse::Paused {
                request_id: request_id(4),
            })
            .unwrap();
        sender
            .send(VcpuResponse::Paused {
                request_id: request_id(5),
            })
            .unwrap();

        assert!(wait_for_vcpu_response(
            &receiver,
            request_id(5),
            VcpuControlTarget::Paused,
            "pause",
            0,
            Instant::now() + Duration::from_millis(10),
        )
        .is_ok());
    }

    #[test]
    fn control_barrier_rejects_wrong_transition() {
        let (sender, receiver) = crossbeam_channel::bounded(1);
        sender
            .send(VcpuResponse::Resumed {
                request_id: request_id(5),
            })
            .unwrap();

        assert!(matches!(
            wait_for_vcpu_response(
                &receiver,
                request_id(5),
                VcpuControlTarget::Paused,
                "pause",
                1,
                Instant::now() + Duration::from_millis(10),
            ),
            Err(Error::VcpuControlProtocol {
                operation: "pause",
                request_id: VcpuControlRequestId(5),
                vcpu_index: 1,
            })
        ));
    }

    #[test]
    fn control_barrier_reports_vcpu_exit() {
        let (sender, receiver) = crossbeam_channel::bounded(1);
        sender.send(VcpuResponse::Exited(7)).unwrap();

        assert!(matches!(
            wait_for_vcpu_response(
                &receiver,
                request_id(8),
                VcpuControlTarget::Resumed,
                "resume",
                2,
                Instant::now() + Duration::from_millis(10),
            ),
            Err(Error::VcpuControlExited {
                operation: "resume",
                request_id: VcpuControlRequestId(8),
                vcpu_index: 2,
                exit_code: 7,
            })
        ));
    }

    #[test]
    fn incremental_policy_keeps_sparse_deltas_and_bounds_dense_ones() {
        assert!(!incremental_capture_is_not_beneficial(0, 1 << 30, 60));
        assert!(!incremental_capture_is_not_beneficial(
            512 << 20,
            1 << 30,
            60
        ));
        assert!(incremental_capture_is_not_beneficial(
            614_400, 1_024_000, 60
        ));
        assert!(incremental_capture_is_not_beneficial(
            900 << 20,
            1 << 30,
            60
        ));
    }

    #[test]
    fn incremental_policy_allows_callers_to_force_complete_capture() {
        assert!(incremental_capture_is_not_beneficial(0, 1 << 30, 0));
    }
}
