// Copyright 2020 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the THIRD-PARTY file.
use crate::virtio::net::Result;
use crate::virtio::net::{BASE_QUEUE_PAIRS, MAX_EXPERIMENTAL_QUEUE_PAIRS, QUEUE_SIZE};
use crate::virtio::queue::Error as QueueError;
use crate::virtio::{
    ActivateError, ActivateResult, DeviceQueue, DeviceState, InterruptTransport, QueueConfig,
    VirtioDevice, TYPE_NET,
};
use crate::Error as DeviceError;

use super::backend::{NetBackend, ReadError, WriteError};
use super::worker::NetWorker;

use std::cmp;
use std::io::Write;
#[cfg(unix)]
use std::os::fd::RawFd;
#[cfg(unix)]
use std::path::PathBuf;
use virtio_bindings::virtio_net::{VIRTIO_NET_F_CTRL_VQ, VIRTIO_NET_F_MAC, VIRTIO_NET_F_MQ};
use virtio_bindings::virtio_ring::VIRTIO_RING_F_EVENT_IDX;
use vm_memory::{ByteValued, GuestMemoryError, GuestMemoryMmap};

const VIRTIO_F_VERSION_1: u32 = 32;

#[derive(Debug)]
pub enum FrontendError {
    DescriptorChainTooSmall,
    EmptyQueue,
    GuestMemory(GuestMemoryError),
    QueueError(QueueError),
    ReadOnlyDescriptor,
}

#[derive(Debug)]
pub enum RxError {
    Backend(ReadError),
    DeviceError(DeviceError),
}

#[derive(Debug)]
pub enum TxError {
    Backend(WriteError),
    DeviceError(DeviceError),
    QueueError(QueueError),
}

#[derive(Copy, Clone, Debug, Default)]
#[repr(C, packed)]
struct VirtioNetConfig {
    mac: [u8; 6],
    status: u16,
    max_virtqueue_pairs: u16,
}

// Safe because it only has data and has no implicit padding.
unsafe impl ByteValued for VirtioNetConfig {}

pub enum VirtioNetBackend {
    #[cfg(unix)]
    UnixstreamFd(RawFd),
    #[cfg(unix)]
    UnixstreamPath(PathBuf),
    #[cfg(unix)]
    UnixgramFd(RawFd),
    #[cfg(unix)]
    UnixgramPath(PathBuf, bool),
    #[cfg(target_os = "linux")]
    Tap(String),
    #[cfg(windows)]
    NamedPipe(String),
    Custom(Box<dyn NetBackend + Send>),
}

pub struct Net {
    id: String,
    pub cfg_backend: Option<VirtioNetBackend>,

    avail_features: u64,
    acked_features: u64,

    pub(crate) device_state: DeviceState,

    config: VirtioNetConfig,
    queue_pairs: u16,
    queue_config: Vec<QueueConfig>,
}

impl Net {
    /// Create a new virtio network device using the backend
    pub fn new(
        id: String,
        cfg_backend: VirtioNetBackend,
        mac: [u8; 6],
        features: u32,
    ) -> Result<Self> {
        let (backend_features, queue_pairs) = match &cfg_backend {
            VirtioNetBackend::Custom(backend) => (
                backend.supported_features(),
                backend
                    .max_queue_pairs()
                    .clamp(BASE_QUEUE_PAIRS, MAX_EXPERIMENTAL_QUEUE_PAIRS),
            ),
            _ => (0, BASE_QUEUE_PAIRS),
        };
        let mut avail_features = features as u64
            | backend_features
            | (1 << VIRTIO_NET_F_MAC)
            | (1 << VIRTIO_RING_F_EVENT_IDX)
            | (1 << VIRTIO_F_VERSION_1);
        if queue_pairs > 1 {
            avail_features |= (1 << VIRTIO_NET_F_CTRL_VQ) | (1 << VIRTIO_NET_F_MQ);
        }

        let config = VirtioNetConfig {
            mac,
            status: 0,
            max_virtqueue_pairs: queue_pairs,
        };
        let queue_count = usize::from(queue_pairs) * 2 + usize::from(queue_pairs > 1);
        let queue_config = vec![QueueConfig::new(QUEUE_SIZE); queue_count];

        Ok(Net {
            id,
            cfg_backend: Some(cfg_backend),

            avail_features,
            acked_features: 0u64,

            device_state: DeviceState::Inactive,
            config,
            queue_pairs,
            queue_config,
        })
    }

    /// Provides the ID of this net device.
    pub fn id(&self) -> &str {
        &self.id
    }
}

impl VirtioDevice for Net {
    fn avail_features(&self) -> u64 {
        self.avail_features
    }

    fn acked_features(&self) -> u64 {
        self.acked_features
    }

    fn set_acked_features(&mut self, acked_features: u64) {
        self.acked_features = acked_features;
    }

    fn device_type(&self) -> u32 {
        TYPE_NET
    }

    fn device_name(&self) -> &str {
        "net"
    }

    fn queue_config(&self) -> &[QueueConfig] {
        &self.queue_config
    }

    fn read_config(&self, offset: u64, mut data: &mut [u8]) {
        let config_slice = self.config.as_slice();
        let config_len = config_slice.len() as u64;
        if offset >= config_len {
            error!("Failed to read config space");
            return;
        }
        if let Some(end) = offset.checked_add(data.len() as u64) {
            // This write can't fail, offset and end are checked against config_len.
            data.write_all(&config_slice[offset as usize..cmp::min(end, config_len) as usize])
                .unwrap();
        }
    }

    fn write_config(&mut self, offset: u64, data: &[u8]) {
        log::warn!(
            "Net: guest driver attempted to write device config (offset={:x}, len={:x})",
            offset,
            data.len()
        );
    }

    fn activate(
        &mut self,
        mem: GuestMemoryMmap,
        interrupt: InterruptTransport,
        queues: Vec<DeviceQueue>,
    ) -> ActivateResult {
        let expected_queues = usize::from(self.queue_pairs) * 2 + usize::from(self.queue_pairs > 1);
        if queues.len() != expected_queues {
            error!("Cannot perform activate. Expected {expected_queues} queue(s)");
            return Err(ActivateError::BadActivate);
        }
        let mut queues = queues.into_iter();
        let mut rx_queues = Vec::with_capacity(usize::from(self.queue_pairs));
        let mut tx_queues = Vec::with_capacity(usize::from(self.queue_pairs));
        for _ in 0..self.queue_pairs {
            rx_queues.push(queues.next().ok_or(ActivateError::BadActivate)?);
            tx_queues.push(queues.next().ok_or(ActivateError::BadActivate)?);
        }
        let control_queue = (self.queue_pairs > 1)
            .then(|| queues.next().ok_or(ActivateError::BadActivate))
            .transpose()?;
        if queues.next().is_some() {
            return Err(ActivateError::BadActivate);
        }

        let cfg_backend = self.cfg_backend.take().ok_or_else(|| {
            error!("Cannot activate net device: backend already taken");
            ActivateError::BadActivate
        })?;

        match NetWorker::new(
            rx_queues,
            tx_queues,
            control_queue,
            interrupt.clone(),
            mem.clone(),
            self.acked_features,
            cfg_backend,
        ) {
            Ok(worker) => {
                worker.run();
                self.device_state = DeviceState::Activated(mem, interrupt);
                Ok(())
            }
            Err(err) => {
                error!(
                    "Error activating virtio-net ({}) backend: {err:?}",
                    self.id()
                );
                Err(ActivateError::BadActivate)
            }
        }
    }

    fn is_activated(&self) -> bool {
        self.device_state.is_activated()
    }
}

#[cfg(test)]
mod tests {
    use super::super::backend::{ReadError, WriteError};
    use super::*;

    struct TestBackend {
        queue_pairs: u16,
    }

    impl NetBackend for TestBackend {
        fn max_queue_pairs(&self) -> u16 {
            self.queue_pairs
        }

        fn read_frame(&mut self, _buf: &mut [u8]) -> std::result::Result<usize, ReadError> {
            Err(ReadError::NothingRead)
        }

        fn write_frame(
            &mut self,
            _hdr_len: usize,
            _buf: &mut [u8],
        ) -> std::result::Result<(), WriteError> {
            Ok(())
        }

        fn has_unfinished_write(&self) -> bool {
            false
        }

        fn try_finish_write(
            &mut self,
            _hdr_len: usize,
            _buf: &[u8],
        ) -> std::result::Result<(), WriteError> {
            Ok(())
        }

        #[cfg(unix)]
        fn raw_socket_fd(&self) -> RawFd {
            -1
        }

        #[cfg(windows)]
        fn event_source(&self, token: utils::event::EventToken) -> utils::event::EventSource {
            utils::event::EventSource::waitable_handle(std::ptr::null_mut(), token)
        }
    }

    #[test]
    fn backend_multiqueue_capability_adds_pairs_control_queue_and_features() {
        let device = Net::new(
            "net".to_string(),
            VirtioNetBackend::Custom(Box::new(TestBackend { queue_pairs: 2 })),
            [0x02, 0, 0, 0, 0, 1],
            0,
        )
        .unwrap();

        assert_eq!(device.queue_config().len(), 5);
        assert_ne!(device.avail_features() & (1 << VIRTIO_NET_F_MQ), 0);
        assert_ne!(device.avail_features() & (1 << VIRTIO_NET_F_CTRL_VQ), 0);
        let mut max_pairs = [0u8; 2];
        device.read_config(8, &mut max_pairs);
        assert_eq!(u16::from_le_bytes(max_pairs), 2);
    }
}
