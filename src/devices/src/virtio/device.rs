// Copyright 2020 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the THIRD-PARTY file.

use std::sync::Arc;
use std::{fmt, io};

use super::{ActivateResult, InterruptTransport, Queue};
use crate::virtio::AsAny;
use utils::eventfd::EventFd;
use vm_memory::GuestMemoryMmap;

/// Configuration for a single virtqueue.
/// This is used by devices to declare their queue requirements,
/// and by the transport to construct the actual queues.
#[derive(Clone, Copy, Debug)]
pub struct QueueConfig {
    /// Maximum size of the queue.
    pub size: u16,
}

impl QueueConfig {
    pub const fn new(size: u16) -> Self {
        Self { size }
    }
}

/// A virtqueue combined with its notification eventfd.
/// This is passed to devices during activation.
pub struct DeviceQueue {
    pub queue: Queue,
    pub event: Arc<EventFd>,
}

/// Errors returned while moving a virtio device across a reversible state boundary.
#[derive(Debug)]
pub enum VirtioStateError {
    /// The device does not implement reversible state.
    Unsupported(String),
    /// The device is not in the lifecycle state required by the operation.
    InvalidLifecycle(&'static str),
    /// A queue is not at a terminal descriptor boundary.
    Queue(super::queue::Error),
    /// Device-specific quiescence or durability work failed.
    Device(io::Error),
    /// Saved state is incompatible with the constructed device.
    Incompatible(String),
}

impl fmt::Display for VirtioStateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported(device) => {
                write!(f, "{device} does not support reversible virtio state")
            }
            Self::InvalidLifecycle(message) => write!(f, "invalid virtio lifecycle: {message}"),
            Self::Queue(error) => write!(f, "virtio queue state error: {error}"),
            Self::Device(error) => write!(f, "virtio device state error: {error}"),
            Self::Incompatible(message) => write!(f, "incompatible virtio state: {message}"),
        }
    }
}

impl std::error::Error for VirtioStateError {}

impl From<super::queue::Error> for VirtioStateError {
    fn from(error: super::queue::Error) -> Self {
        Self::Queue(error)
    }
}

impl From<io::Error> for VirtioStateError {
    fn from(error: io::Error) -> Self {
        Self::Device(error)
    }
}

impl DeviceQueue {
    pub fn new(queue: Queue, event: Arc<EventFd>) -> Self {
        Self { queue, event }
    }
}

/// Enum that indicates if a VirtioDevice is inactive or has been activated
/// and memory attached to it.
pub enum DeviceState {
    Inactive,
    Activated(GuestMemoryMmap, InterruptTransport),
}

impl DeviceState {
    pub fn signal_used_queue(&self) {
        match self {
            Self::Inactive => {
                warn!("DeviceState::signal_used_queue() called, but device is not activated")
            }
            Self::Activated(_, ref interrupt) => interrupt.signal_used_queue(),
        }
    }
}

impl DeviceState {
    pub fn is_activated(&self) -> bool {
        matches!(self, DeviceState::Activated(..))
    }
}

#[derive(Clone)]
pub struct VirtioShmRegion {
    pub host_addr: u64,
    pub guest_addr: u64,
    pub size: usize,
}

/// Trait for virtio devices to be driven by a virtio transport.
///
/// The lifecycle of a virtio device is to be moved to a virtio transport, which will then query the
/// device. The transport constructs queues based on queue_config() and passes them to the device
/// during activation, transferring ownership. After reset, the transport recreates queues
/// from queue_config() for the next negotiation cycle.
pub trait VirtioDevice: AsAny + Send {
    /// Get the available features offered by device.
    fn avail_features(&self) -> u64;

    /// Get acknowledged features of the driver.
    fn acked_features(&self) -> u64;

    /// Set acknowledged features of the driver.
    /// This function must maintain the following invariant:
    /// - self.avail_features() & self.acked_features() = self.get_acked_features()
    fn set_acked_features(&mut self, acked_features: u64);

    /// The virtio device type.
    fn device_type(&self) -> u32;

    /// Device name used for logging information about the device at the transport layer
    fn device_name(&self) -> &str;

    /// Returns the queue configuration for this device.
    /// The transport uses this to construct the queues during initialization and after reset.
    fn queue_config(&self) -> &[QueueConfig];

    /// The set of feature bits shifted by `page * 32`.
    fn avail_features_by_page(&self, page: u32) -> u32 {
        let avail_features = self.avail_features();
        match page {
            // Get the lower 32-bits of the features bitfield.
            0 => avail_features as u32,
            // Get the upper 32-bits of the features bitfield.
            1 => (avail_features >> 32) as u32,
            _ => {
                warn!("Received request for unknown features page.");
                0u32
            }
        }
    }

    /// Acknowledges that this set of features should be enabled.
    fn ack_features_by_page(&mut self, page: u32, value: u32) {
        let mut v = match page {
            0 => u64::from(value),
            1 => u64::from(value) << 32,
            _ => {
                warn!("Cannot acknowledge unknown features page: {page}");
                0u64
            }
        };

        // Check if the guest is ACK'ing a feature that we didn't claim to have.
        let avail_features = self.avail_features();
        let unrequested_features = v & !avail_features;
        if unrequested_features != 0 {
            warn!("Received acknowledge request for unknown feature: {v:x}");
            // Don't count these features as acked.
            v &= !unrequested_features;
        }
        self.set_acked_features(self.acked_features() | v);
    }

    /// Reads this device configuration space at `offset`.
    fn read_config(&self, offset: u64, data: &mut [u8]);

    /// Writes to this device configuration space at `offset`.
    fn write_config(&mut self, offset: u64, data: &[u8]);

    /// Performs the formal activation for a device, which can be verified also with `is_activated`.
    /// Ownership of the queues is transferred to the device.
    fn activate(
        &mut self,
        mem: GuestMemoryMmap,
        interrupt: InterruptTransport,
        queues: Vec<DeviceQueue>,
    ) -> ActivateResult;

    /// Checks if the resources of this device are activated.
    fn is_activated(&self) -> bool;

    /// Optionally deactivates this device. The device should drop its queues.
    /// After reset, the transport will recreate queues from queue_config().
    fn reset(&mut self) -> bool {
        false
    }

    /// Reports whether the device can establish a reversible queue-ownership boundary.
    ///
    /// Resource admission uses this before pausing vCPUs. Implementations must return `true` only
    /// when [`quiesce`](Self::quiesce) either succeeds or leaves enough local state for retry/resume.
    fn supports_quiesce(&self) -> bool {
        false
    }

    /// Stops dequeuing new work and returns every queue after consumed work is terminal.
    ///
    /// Devices that implement this operation must leave themselves inactive. The transport owns
    /// the returned queues until it activates the device again.
    fn quiesce(&mut self) -> Result<Vec<DeviceQueue>, VirtioStateError> {
        Err(VirtioStateError::Unsupported(
            self.device_name().to_string(),
        ))
    }

    /// Captures bounded device-specific state after queue ownership returns to the transport.
    ///
    /// Most devices reconstruct their configuration from the destination resource plan and need
    /// no additional bytes. Devices with protocol state that survives in guest memory can override
    /// this hook so their host-side half advances from the same boundary after restore.
    fn capture_device_state(&self) -> Result<Vec<u8>, VirtioStateError> {
        Ok(Vec::new())
    }

    /// Validates device-specific bytes without changing the constructed destination device.
    fn validate_device_state(&self, state: &[u8]) -> Result<(), VirtioStateError> {
        if state.is_empty() {
            Ok(())
        } else {
            Err(VirtioStateError::Incompatible(format!(
                "{} does not accept device-specific state",
                self.device_name()
            )))
        }
    }

    /// Restores previously validated device-specific bytes before device activation.
    fn restore_device_state(&mut self, state: &[u8]) -> Result<(), VirtioStateError> {
        self.validate_device_state(state)
    }

    /// Get base and size of the SHM region
    fn shm_region(&self) -> Option<&VirtioShmRegion> {
        None
    }
}

pub trait VmmExitObserver: Send {
    /// Callback to finish processing or cleanup the device resources.
    ///
    /// `exit_code` is the final exit code chosen by the VMM (from guest
    /// vCPU or the shared `exit_code` Arc).
    fn on_vmm_exit(&mut self, _exit_code: i32) {}
}

impl<F: Fn(i32) + Send> VmmExitObserver for F {
    fn on_vmm_exit(&mut self, exit_code: i32) {
        self(exit_code)
    }
}

impl std::fmt::Debug for dyn VirtioDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "VirtioDevice type {}", self.device_type())
    }
}
