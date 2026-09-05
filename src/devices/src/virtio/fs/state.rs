// Copyright 2026 Microsandbox Authors. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Bounded durable state for one virtio-fs session.

use std::io;

//--------------------------------------------------------------------------------------------------
// Constants
//--------------------------------------------------------------------------------------------------

const MAGIC: &[u8; 8] = b"MSBKFS\0\0";
const VERSION: u16 = 1;
pub(super) const MAX_BACKEND_STATE_BYTES: usize = 4 * 1024 * 1024;
pub(super) const MAX_DEVICE_STATE_BYTES: usize = 8 * 1024 * 1024;

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

pub(super) struct FsDeviceState {
    pub(super) session_options: u64,
    pub(super) backend_state: Vec<u8>,
}

//--------------------------------------------------------------------------------------------------
// Methods
//--------------------------------------------------------------------------------------------------

impl FsDeviceState {
    pub(super) fn encode(&self) -> io::Result<Vec<u8>> {
        if self.backend_state.len() > MAX_BACKEND_STATE_BYTES {
            return Err(invalid_data("virtio-fs backend state exceeds 4 MiB"));
        }
        let mut bytes = Vec::with_capacity(22 + self.backend_state.len());
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&VERSION.to_le_bytes());
        bytes.extend_from_slice(&self.session_options.to_le_bytes());
        bytes.extend_from_slice(
            &u32::try_from(self.backend_state.len())
                .map_err(|_| invalid_data("virtio-fs backend state length does not fit u32"))?
                .to_le_bytes(),
        );
        bytes.extend_from_slice(&self.backend_state);
        if bytes.len() > MAX_DEVICE_STATE_BYTES {
            return Err(invalid_data("virtio-fs device state exceeds 8 MiB"));
        }
        Ok(bytes)
    }

    pub(super) fn decode(bytes: &[u8]) -> io::Result<Self> {
        const HEADER_LEN: usize = 22;
        if bytes.len() > MAX_DEVICE_STATE_BYTES {
            return Err(invalid_data("virtio-fs device state exceeds 8 MiB"));
        }
        if bytes.len() < HEADER_LEN || &bytes[..MAGIC.len()] != MAGIC {
            return Err(invalid_data("invalid virtio-fs state magic"));
        }
        if u16::from_le_bytes(bytes[8..10].try_into().unwrap()) != VERSION {
            return Err(invalid_data("unsupported virtio-fs state version"));
        }
        let session_options = u64::from_le_bytes(bytes[10..18].try_into().unwrap());
        let backend_len = u32::from_le_bytes(bytes[18..22].try_into().unwrap()) as usize;
        if backend_len > MAX_BACKEND_STATE_BYTES || HEADER_LEN + backend_len != bytes.len() {
            return Err(invalid_data("invalid virtio-fs backend state length"));
        }
        Ok(Self {
            session_options,
            backend_state: bytes[HEADER_LEN..].to_vec(),
        })
    }
}

//--------------------------------------------------------------------------------------------------
// Functions
//--------------------------------------------------------------------------------------------------

fn invalid_data(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_round_trip_and_bounds() {
        let encoded = FsDeviceState {
            session_options: 0x1234,
            backend_state: vec![1, 2, 3],
        }
        .encode()
        .unwrap();
        let decoded = FsDeviceState::decode(&encoded).unwrap();
        assert_eq!(decoded.session_options, 0x1234);
        assert_eq!(decoded.backend_state, vec![1, 2, 3]);
        assert!(FsDeviceState {
            session_options: 0,
            backend_state: vec![0; MAX_BACKEND_STATE_BYTES + 1],
        }
        .encode()
        .is_err());
        let mut oversized = vec![0; MAX_DEVICE_STATE_BYTES + 1];
        oversized[..MAGIC.len()].copy_from_slice(MAGIC);
        assert!(FsDeviceState::decode(&oversized).is_err());
    }
}
