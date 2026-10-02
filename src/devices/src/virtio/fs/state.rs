// Copyright 2026 Microsandbox Authors. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Bounded durable state for one virtio-fs session.

use std::io;

use super::super::FS_DEVICE_STATE_HEADER_BYTES;

//--------------------------------------------------------------------------------------------------
// Constants
//--------------------------------------------------------------------------------------------------

const MAGIC: &[u8; 8] = b"MSBKFS\0\0";
const VERSION: u16 = 1;

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
    pub(super) fn encode(&self, max_backend_state_bytes: usize) -> io::Result<Vec<u8>> {
        let max_device_state_bytes = max_device_state_bytes(max_backend_state_bytes)?;
        if self.backend_state.len() > max_backend_state_bytes {
            return Err(invalid_data("virtio-fs backend state exceeds its limit"));
        }
        let mut bytes = Vec::with_capacity(FS_DEVICE_STATE_HEADER_BYTES + self.backend_state.len());
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&VERSION.to_le_bytes());
        bytes.extend_from_slice(&self.session_options.to_le_bytes());
        bytes.extend_from_slice(
            &u32::try_from(self.backend_state.len())
                .map_err(|_| invalid_data("virtio-fs backend state length does not fit u32"))?
                .to_le_bytes(),
        );
        bytes.extend_from_slice(&self.backend_state);
        if bytes.len() > max_device_state_bytes {
            return Err(invalid_data("virtio-fs device state exceeds its limit"));
        }
        Ok(bytes)
    }

    pub(super) fn decode(bytes: &[u8], max_backend_state_bytes: usize) -> io::Result<Self> {
        if bytes.len() > max_device_state_bytes(max_backend_state_bytes)? {
            return Err(invalid_data("virtio-fs device state exceeds its limit"));
        }
        if bytes.len() < FS_DEVICE_STATE_HEADER_BYTES || &bytes[..MAGIC.len()] != MAGIC {
            return Err(invalid_data("invalid virtio-fs state magic"));
        }
        if u16::from_le_bytes(bytes[8..10].try_into().unwrap()) != VERSION {
            return Err(invalid_data("unsupported virtio-fs state version"));
        }
        let session_options = u64::from_le_bytes(bytes[10..18].try_into().unwrap());
        let backend_len = u32::from_le_bytes(bytes[18..22].try_into().unwrap()) as usize;
        if backend_len > max_backend_state_bytes
            || FS_DEVICE_STATE_HEADER_BYTES + backend_len != bytes.len()
        {
            return Err(invalid_data("invalid virtio-fs backend state length"));
        }
        Ok(Self {
            session_options,
            backend_state: bytes[FS_DEVICE_STATE_HEADER_BYTES..].to_vec(),
        })
    }
}

//--------------------------------------------------------------------------------------------------
// Functions
//--------------------------------------------------------------------------------------------------

/// Largest encoded device state for a backend state budget; the budget must fit the u32 length
/// field.
fn max_device_state_bytes(max_backend_state_bytes: usize) -> io::Result<usize> {
    u32::try_from(max_backend_state_bytes)
        .ok()
        .and_then(|_| FS_DEVICE_STATE_HEADER_BYTES.checked_add(max_backend_state_bytes))
        .ok_or_else(|| invalid_data("virtio-fs backend state limit does not fit u32"))
}

fn invalid_data(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use super::super::super::DEFAULT_MAX_FS_BACKEND_STATE_BYTES;

    const DEFAULT_LIMIT: usize = DEFAULT_MAX_FS_BACKEND_STATE_BYTES;

    #[test]
    fn state_round_trip_and_bounds() {
        let encoded = FsDeviceState {
            session_options: 0x1234,
            backend_state: vec![1, 2, 3],
        }
        .encode(DEFAULT_LIMIT)
        .unwrap();
        let decoded = FsDeviceState::decode(&encoded, DEFAULT_LIMIT).unwrap();
        assert_eq!(decoded.session_options, 0x1234);
        assert_eq!(decoded.backend_state, vec![1, 2, 3]);
        assert!(FsDeviceState {
            session_options: 0,
            backend_state: vec![0; DEFAULT_LIMIT + 1],
        }
        .encode(DEFAULT_LIMIT)
        .is_err());
        let mut oversized = vec![0; FS_DEVICE_STATE_HEADER_BYTES + DEFAULT_LIMIT + 1];
        oversized[..MAGIC.len()].copy_from_slice(MAGIC);
        assert!(FsDeviceState::decode(&oversized, DEFAULT_LIMIT).is_err());
    }

    #[test]
    fn state_above_default_budget_needs_a_larger_budget() {
        let limit = DEFAULT_LIMIT + 1024;
        let state = FsDeviceState {
            session_options: 7,
            backend_state: vec![0xa5; DEFAULT_LIMIT + 1],
        };
        assert!(state.encode(DEFAULT_LIMIT).is_err());
        let encoded = state.encode(limit).unwrap();
        assert_eq!(
            encoded.len(),
            FS_DEVICE_STATE_HEADER_BYTES + DEFAULT_LIMIT + 1
        );
        assert!(FsDeviceState::decode(&encoded, DEFAULT_LIMIT).is_err());
        let decoded = FsDeviceState::decode(&encoded, limit).unwrap();
        assert_eq!(decoded.backend_state, state.backend_state);
    }

    #[test]
    fn decode_bounds_the_whole_state_by_header_plus_budget() {
        let limit = 64;
        let mut at_limit = FsDeviceState {
            session_options: 0,
            backend_state: vec![0; limit],
        }
        .encode(limit)
        .unwrap();
        assert!(FsDeviceState::decode(&at_limit, limit).is_ok());
        at_limit.push(0);
        assert!(FsDeviceState::decode(&at_limit, limit).is_err());
    }

    #[test]
    fn budgets_that_do_not_fit_the_length_field_are_rejected() {
        let state = FsDeviceState {
            session_options: 0,
            backend_state: Vec::new(),
        };
        let encoded = state.encode(DEFAULT_LIMIT).unwrap();
        if let Some(too_large) = (u32::MAX as usize).checked_add(1) {
            assert!(state.encode(too_large).is_err());
            assert!(FsDeviceState::decode(&encoded, too_large).is_err());
        }
        assert!(max_device_state_bytes(u32::MAX as usize - FS_DEVICE_STATE_HEADER_BYTES).is_ok());
    }
}
