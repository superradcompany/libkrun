// Copyright 2026 Microsandbox Authors. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Typed reversible state for host-emulated devices.

#[cfg(feature = "blk")]
use std::fmt::{Display, Formatter};

#[cfg(feature = "blk")]
use devices::virtio::{
    BlockState, CacheType, QueueState, VirtioMmioState, BLOCK_STATE_VERSION, QUEUE_STATE_VERSION,
    VIRTIO_MMIO_STATE_VERSION,
};

//--------------------------------------------------------------------------------------------------
// Constants
//--------------------------------------------------------------------------------------------------

#[cfg(feature = "blk")]
const BLOCK_DEVICE_STATE_MAGIC: &[u8; 9] = b"MSBKBLK\0\0";
#[cfg(feature = "blk")]
const VIRTIO_DEVICE_STATE_MAGIC: &[u8; 9] = b"MSBKVIO\0\0";
#[cfg(feature = "blk")]
const BLOCK_DEVICE_STATE_SCHEMA: u16 = 1;
#[cfg(feature = "blk")]
const MAX_DEVICE_STATE_BYTES: usize = 1024 * 1024;
#[cfg(feature = "blk")]
const MAX_DEVICE_STRING_BYTES: usize = 4096;
#[cfg(feature = "blk")]
const MAX_DISK_ID_BYTES: usize = 256;
#[cfg(feature = "blk")]
const MAX_QUEUES: usize = 256;

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

/// Complete host-maintained state for one virtio-block device.
#[cfg(feature = "blk")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlockDeviceState {
    /// Source pause generation shared with execution and memory capture orchestration.
    pub pause_generation: u64,
    /// Generic virtio-mmio registers and exact queue cursors.
    pub transport: VirtioMmioState,
    /// Guest-visible block identity, capacity, policy, and negotiated features.
    pub device: BlockState,
}

/// Generic host-maintained state for a quiesced, destination-recreated virtio device.
///
/// Device-specific configuration is reconstructed from the admitted sandbox resource plan. This
/// envelope preserves the exact transport and queue boundary plus the logical device binding.
#[cfg(feature = "blk")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VirtioDeviceState {
    /// Source pause generation shared with execution and memory capture orchestration.
    pub pause_generation: u64,
    /// Stable device identifier used by the runtime resource plan.
    pub device_id: String,
    /// Generic virtio-mmio registers and exact queue cursors.
    pub transport: VirtioMmioState,
}

/// Framing and compatibility errors for virtio-block state artifacts.
#[cfg(feature = "blk")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    /// Magic or an embedded schema version is unsupported.
    UnsupportedFormat,
    /// A declared value exceeds its bound or the available input.
    InvalidLength,
    /// A boolean or enum tag is not recognized.
    InvalidValue,
    /// A saved identifier is not valid UTF-8.
    InvalidUtf8,
    /// Bytes follow the complete artifact.
    TrailingBytes,
}

/// Result type for device-state framing operations.
#[cfg(feature = "blk")]
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(feature = "blk")]
struct Writer {
    bytes: Vec<u8>,
}

#[cfg(feature = "blk")]
struct Reader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

//--------------------------------------------------------------------------------------------------
// Methods
//--------------------------------------------------------------------------------------------------

#[cfg(feature = "blk")]
impl BlockDeviceState {
    /// Encodes this typed state into deterministic, bounded bytes.
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut writer = Writer { bytes: Vec::new() };
        writer.bytes(BLOCK_DEVICE_STATE_MAGIC);
        writer.u16(BLOCK_DEVICE_STATE_SCHEMA);
        writer.u64(self.pause_generation);
        writer.u16(self.transport.version);
        writer.u32(self.transport.device_type);
        writer.u32(self.transport.features_select);
        writer.u32(self.transport.acked_features_select);
        writer.u32(self.transport.queue_select);
        writer.u32(self.transport.device_status);
        writer.u32(self.transport.config_generation);
        writer.u32(self.transport.shm_region_select);
        writer
            .u64(u64::try_from(self.transport.interrupt_status).map_err(|_| Error::InvalidValue)?);
        writer.option_u32(self.transport.irq_line);
        writer.u64(self.transport.acked_features);
        writer.len(self.transport.queues.len(), MAX_QUEUES)?;
        for queue in &self.transport.queues {
            writer.u16(queue.version);
            writer.u16(queue.max_size);
            writer.u16(queue.size);
            writer.bool(queue.ready);
            writer.u64(queue.desc_table);
            writer.u64(queue.avail_ring);
            writer.u64(queue.used_ring);
            writer.u16(queue.next_avail);
            writer.u16(queue.next_used);
            writer.bool(queue.event_idx_enabled);
            writer.u16(queue.num_added);
        }

        writer.u16(self.device.version);
        writer.string(&self.device.id, MAX_DEVICE_STRING_BYTES)?;
        writer.option_string(self.device.partuuid.as_deref(), MAX_DEVICE_STRING_BYTES)?;
        writer.u64(self.device.capacity_sectors);
        writer.sized_bytes(&self.device.disk_image_id, MAX_DISK_ID_BYTES)?;
        writer.u64(self.device.avail_features);
        writer.u8(match self.device.cache_type {
            CacheType::Unsafe => 0,
            CacheType::Writeback => 1,
        });
        writer.bool(self.device.read_only);
        if writer.bytes.len() > MAX_DEVICE_STATE_BYTES {
            return Err(Error::InvalidLength);
        }
        Ok(writer.bytes)
    }

    /// Decodes and validates one complete typed state artifact.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_DEVICE_STATE_BYTES {
            return Err(Error::InvalidLength);
        }
        let mut reader = Reader { bytes, offset: 0 };
        if reader.take(BLOCK_DEVICE_STATE_MAGIC.len())? != BLOCK_DEVICE_STATE_MAGIC
            || reader.u16()? != BLOCK_DEVICE_STATE_SCHEMA
        {
            return Err(Error::UnsupportedFormat);
        }
        let pause_generation = reader.u64()?;

        let transport_version = reader.u16()?;
        if transport_version != VIRTIO_MMIO_STATE_VERSION {
            return Err(Error::UnsupportedFormat);
        }
        let device_type = reader.u32()?;
        let features_select = reader.u32()?;
        let acked_features_select = reader.u32()?;
        let queue_select = reader.u32()?;
        let device_status = reader.u32()?;
        let config_generation = reader.u32()?;
        let shm_region_select = reader.u32()?;
        let interrupt_status = usize::try_from(reader.u64()?).map_err(|_| Error::InvalidValue)?;
        let irq_line = reader.option_u32()?;
        let transport_acked_features = reader.u64()?;
        let queue_count = reader.len(MAX_QUEUES)?;
        let mut queues = Vec::with_capacity(queue_count);
        for _ in 0..queue_count {
            let version = reader.u16()?;
            if version != QUEUE_STATE_VERSION {
                return Err(Error::UnsupportedFormat);
            }
            queues.push(QueueState {
                version,
                max_size: reader.u16()?,
                size: reader.u16()?,
                ready: reader.bool()?,
                desc_table: reader.u64()?,
                avail_ring: reader.u64()?,
                used_ring: reader.u64()?,
                next_avail: reader.u16()?,
                next_used: reader.u16()?,
                event_idx_enabled: reader.bool()?,
                num_added: reader.u16()?,
            });
        }

        let block_version = reader.u16()?;
        if block_version != BLOCK_STATE_VERSION {
            return Err(Error::UnsupportedFormat);
        }
        let id = reader.string(MAX_DEVICE_STRING_BYTES)?;
        let partuuid = reader.option_string(MAX_DEVICE_STRING_BYTES)?;
        let capacity_sectors = reader.u64()?;
        let disk_image_id = reader.sized_bytes(MAX_DISK_ID_BYTES)?.to_vec();
        let avail_features = reader.u64()?;
        let cache_type = match reader.u8()? {
            0 => CacheType::Unsafe,
            1 => CacheType::Writeback,
            _ => return Err(Error::InvalidValue),
        };
        let read_only = reader.bool()?;
        if reader.offset != bytes.len() {
            return Err(Error::TrailingBytes);
        }

        Ok(Self {
            pause_generation,
            transport: VirtioMmioState {
                version: transport_version,
                device_type,
                features_select,
                acked_features_select,
                queue_select,
                device_status,
                config_generation,
                shm_region_select,
                interrupt_status,
                irq_line,
                acked_features: transport_acked_features,
                queues,
            },
            device: BlockState {
                version: block_version,
                id,
                partuuid,
                capacity_sectors,
                disk_image_id,
                avail_features,
                cache_type,
                read_only,
            },
        })
    }
}

#[cfg(feature = "blk")]
impl VirtioDeviceState {
    /// Encodes this typed state into deterministic, bounded bytes.
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut writer = Writer { bytes: Vec::new() };
        writer.bytes(VIRTIO_DEVICE_STATE_MAGIC);
        writer.u16(BLOCK_DEVICE_STATE_SCHEMA);
        writer.u64(self.pause_generation);
        writer.string(&self.device_id, MAX_DEVICE_STRING_BYTES)?;
        encode_transport(&mut writer, &self.transport)?;
        if writer.bytes.len() > MAX_DEVICE_STATE_BYTES {
            return Err(Error::InvalidLength);
        }
        Ok(writer.bytes)
    }

    /// Decodes and validates one generic virtio state artifact.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_DEVICE_STATE_BYTES {
            return Err(Error::InvalidLength);
        }
        let mut reader = Reader { bytes, offset: 0 };
        if reader.take(VIRTIO_DEVICE_STATE_MAGIC.len())? != VIRTIO_DEVICE_STATE_MAGIC
            || reader.u16()? != BLOCK_DEVICE_STATE_SCHEMA
        {
            return Err(Error::UnsupportedFormat);
        }
        let pause_generation = reader.u64()?;
        let device_id = reader.string(MAX_DEVICE_STRING_BYTES)?;
        let transport = decode_transport(&mut reader)?;
        if reader.offset != bytes.len() {
            return Err(Error::TrailingBytes);
        }
        Ok(Self {
            pause_generation,
            device_id,
            transport,
        })
    }
}

#[cfg(feature = "blk")]
impl Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

#[cfg(feature = "blk")]
impl std::error::Error for Error {}

#[cfg(feature = "blk")]
impl Writer {
    fn u8(&mut self, value: u8) {
        self.bytes.push(value);
    }

    fn u16(&mut self, value: u16) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn u32(&mut self, value: u32) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn bool(&mut self, value: bool) {
        self.u8(u8::from(value));
    }

    fn bytes(&mut self, value: &[u8]) {
        self.bytes.extend_from_slice(value);
    }

    fn len(&mut self, length: usize, maximum: usize) -> Result<()> {
        if length > maximum {
            return Err(Error::InvalidLength);
        }
        self.u32(u32::try_from(length).map_err(|_| Error::InvalidLength)?);
        Ok(())
    }

    fn sized_bytes(&mut self, value: &[u8], maximum: usize) -> Result<()> {
        self.len(value.len(), maximum)?;
        self.bytes(value);
        Ok(())
    }

    fn string(&mut self, value: &str, maximum: usize) -> Result<()> {
        self.sized_bytes(value.as_bytes(), maximum)
    }

    fn option_u32(&mut self, value: Option<u32>) {
        self.bool(value.is_some());
        if let Some(value) = value {
            self.u32(value);
        }
    }

    fn option_string(&mut self, value: Option<&str>, maximum: usize) -> Result<()> {
        self.bool(value.is_some());
        if let Some(value) = value {
            self.string(value, maximum)?;
        }
        Ok(())
    }
}

#[cfg(feature = "blk")]
impl<'a> Reader<'a> {
    fn take(&mut self, length: usize) -> Result<&'a [u8]> {
        let end = self
            .offset
            .checked_add(length)
            .ok_or(Error::InvalidLength)?;
        let value = self
            .bytes
            .get(self.offset..end)
            .ok_or(Error::InvalidLength)?;
        self.offset = end;
        Ok(value)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(
            self.take(2)?.try_into().expect("checked length"),
        ))
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(
            self.take(4)?.try_into().expect("checked length"),
        ))
    }

    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(
            self.take(8)?.try_into().expect("checked length"),
        ))
    }

    fn bool(&mut self) -> Result<bool> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(Error::InvalidValue),
        }
    }

    fn len(&mut self, maximum: usize) -> Result<usize> {
        let length = self.u32()? as usize;
        if length > maximum {
            return Err(Error::InvalidLength);
        }
        Ok(length)
    }

    fn sized_bytes(&mut self, maximum: usize) -> Result<&'a [u8]> {
        let length = self.len(maximum)?;
        self.take(length)
    }

    fn string(&mut self, maximum: usize) -> Result<String> {
        std::str::from_utf8(self.sized_bytes(maximum)?)
            .map(str::to_string)
            .map_err(|_| Error::InvalidUtf8)
    }

    fn option_u32(&mut self) -> Result<Option<u32>> {
        if self.bool()? {
            Ok(Some(self.u32()?))
        } else {
            Ok(None)
        }
    }

    fn option_string(&mut self, maximum: usize) -> Result<Option<String>> {
        if self.bool()? {
            Ok(Some(self.string(maximum)?))
        } else {
            Ok(None)
        }
    }
}

//--------------------------------------------------------------------------------------------------
// Functions
//--------------------------------------------------------------------------------------------------

#[cfg(feature = "blk")]
fn encode_transport(writer: &mut Writer, transport: &VirtioMmioState) -> Result<()> {
    writer.u16(transport.version);
    writer.u32(transport.device_type);
    writer.u32(transport.features_select);
    writer.u32(transport.acked_features_select);
    writer.u32(transport.queue_select);
    writer.u32(transport.device_status);
    writer.u32(transport.config_generation);
    writer.u32(transport.shm_region_select);
    writer.u64(u64::try_from(transport.interrupt_status).map_err(|_| Error::InvalidValue)?);
    writer.option_u32(transport.irq_line);
    writer.u64(transport.acked_features);
    writer.len(transport.queues.len(), MAX_QUEUES)?;
    for queue in &transport.queues {
        writer.u16(queue.version);
        writer.u16(queue.max_size);
        writer.u16(queue.size);
        writer.bool(queue.ready);
        writer.u64(queue.desc_table);
        writer.u64(queue.avail_ring);
        writer.u64(queue.used_ring);
        writer.u16(queue.next_avail);
        writer.u16(queue.next_used);
        writer.bool(queue.event_idx_enabled);
        writer.u16(queue.num_added);
    }
    Ok(())
}

#[cfg(feature = "blk")]
fn decode_transport(reader: &mut Reader<'_>) -> Result<VirtioMmioState> {
    let version = reader.u16()?;
    if version != VIRTIO_MMIO_STATE_VERSION {
        return Err(Error::UnsupportedFormat);
    }
    let device_type = reader.u32()?;
    let features_select = reader.u32()?;
    let acked_features_select = reader.u32()?;
    let queue_select = reader.u32()?;
    let device_status = reader.u32()?;
    let config_generation = reader.u32()?;
    let shm_region_select = reader.u32()?;
    let interrupt_status = usize::try_from(reader.u64()?).map_err(|_| Error::InvalidValue)?;
    let irq_line = reader.option_u32()?;
    let acked_features = reader.u64()?;
    let queue_count = reader.len(MAX_QUEUES)?;
    let mut queues = Vec::with_capacity(queue_count);
    for _ in 0..queue_count {
        let version = reader.u16()?;
        if version != QUEUE_STATE_VERSION {
            return Err(Error::UnsupportedFormat);
        }
        queues.push(QueueState {
            version,
            max_size: reader.u16()?,
            size: reader.u16()?,
            ready: reader.bool()?,
            desc_table: reader.u64()?,
            avail_ring: reader.u64()?,
            used_ring: reader.u64()?,
            next_avail: reader.u16()?,
            next_used: reader.u16()?,
            event_idx_enabled: reader.bool()?,
            num_added: reader.u16()?,
        });
    }
    Ok(VirtioMmioState {
        version,
        device_type,
        features_select,
        acked_features_select,
        queue_select,
        device_status,
        config_generation,
        shm_region_select,
        interrupt_status,
        irq_line,
        acked_features,
        queues,
    })
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(all(test, feature = "blk"))]
mod tests {
    use devices::virtio::TYPE_BLOCK;

    use super::*;

    fn state() -> BlockDeviceState {
        BlockDeviceState {
            pause_generation: 42,
            transport: VirtioMmioState {
                version: VIRTIO_MMIO_STATE_VERSION,
                device_type: TYPE_BLOCK,
                features_select: 1,
                acked_features_select: 0,
                queue_select: 0,
                device_status: 15,
                config_generation: 4,
                shm_region_select: 0,
                interrupt_status: 1,
                irq_line: Some(7),
                acked_features: 0x120,
                queues: vec![QueueState {
                    version: QUEUE_STATE_VERSION,
                    max_size: 256,
                    size: 256,
                    ready: true,
                    desc_table: 0x1000,
                    avail_ring: 0x3000,
                    used_ring: 0x4000,
                    next_avail: u16::MAX,
                    next_used: 42,
                    event_idx_enabled: true,
                    num_added: 3,
                }],
            },
            device: BlockState {
                version: BLOCK_STATE_VERSION,
                id: "root".to_string(),
                partuuid: Some("part".to_string()),
                capacity_sectors: 8192,
                disk_image_id: b"root\0".to_vec(),
                avail_features: 0x1ff,
                cache_type: CacheType::Writeback,
                read_only: false,
            },
        }
    }

    fn generic_state() -> VirtioDeviceState {
        let block = state();
        VirtioDeviceState {
            pause_generation: block.pause_generation,
            device_id: "console".to_string(),
            transport: VirtioMmioState {
                device_type: 3,
                ..block.transport
            },
        }
    }

    #[test]
    fn block_device_state_round_trip_is_deterministic() {
        let state = state();
        let encoded = state.encode().unwrap();
        assert_eq!(BlockDeviceState::decode(&encoded).unwrap(), state);
        assert_eq!(state.encode().unwrap(), encoded);
    }

    #[test]
    fn block_device_state_rejects_trailing_and_truncated_bytes() {
        let mut encoded = state().encode().unwrap();
        assert!(matches!(
            BlockDeviceState::decode(&encoded[..encoded.len() - 1]),
            Err(Error::InvalidLength)
        ));
        encoded.push(0);
        assert_eq!(
            BlockDeviceState::decode(&encoded),
            Err(Error::TrailingBytes)
        );
    }

    #[test]
    fn generic_virtio_state_round_trip_is_deterministic() {
        let state = generic_state();
        let encoded = state.encode().unwrap();
        assert_eq!(VirtioDeviceState::decode(&encoded).unwrap(), state);
        assert_eq!(state.encode().unwrap(), encoded);
    }

    #[test]
    fn generic_virtio_state_rejects_trailing_and_truncated_bytes() {
        let mut encoded = generic_state().encode().unwrap();
        assert!(matches!(
            VirtioDeviceState::decode(&encoded[..encoded.len() - 1]),
            Err(Error::InvalidLength)
        ));
        encoded.push(0);
        assert_eq!(
            VirtioDeviceState::decode(&encoded),
            Err(Error::TrailingBytes)
        );
    }
}
