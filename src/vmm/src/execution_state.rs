// Copyright 2026 Super Rad Company.
// SPDX-License-Identifier: Apache-2.0

//! Versioned, backend-qualified execution-state envelope.
//!
//! Backend codecs own the meaning of their opaque VM and vCPU payloads. This module owns strict
//! framing, bounds, topology checks, and an architecture/backend identity that prevents bytes from
//! being restored through the wrong hypervisor surface.

use std::collections::BTreeSet;
use std::fmt::{Display, Formatter};

//--------------------------------------------------------------------------------------------------
// Constants
//--------------------------------------------------------------------------------------------------

const EXECUTION_STATE_MAGIC: &[u8; 9] = b"MSBKEXEC\0";
const EXECUTION_STATE_SCHEMA: u16 = 1;
const MAX_VCPUS: usize = 1024;
const MAX_VM_STATE_BYTES: usize = 64 * 1024 * 1024;
const MAX_VCPU_STATE_BYTES: usize = 16 * 1024 * 1024;
const MAX_EXECUTION_STATE_BYTES: usize = 512 * 1024 * 1024;

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

/// Guest instruction-set architecture represented by an execution-state artifact.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum ExecutionArchitecture {
    /// 64-bit x86 execution state.
    X86_64 = 1,
    /// 64-bit Arm execution state.
    Aarch64 = 2,
    /// 64-bit RISC-V execution state.
    Riscv64 = 3,
}

/// Hypervisor backend that produced an execution-state artifact.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum ExecutionBackend {
    /// Linux Kernel-based Virtual Machine.
    Kvm = 1,
    /// Apple Hypervisor framework.
    Hvf = 2,
    /// Windows Hypervisor Platform.
    Whp = 3,
}

/// Opaque state for one vCPU, interpreted only by its matching backend codec.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VcpuExecutionState {
    id: u32,
    bytes: Vec<u8>,
}

/// Complete hypervisor-owned execution state at one VM-wide pause generation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionState {
    architecture: ExecutionArchitecture,
    backend: ExecutionBackend,
    backend_state_abi: u32,
    pause_generation: u64,
    vm_state: Vec<u8>,
    vcpus: Vec<VcpuExecutionState>,
}

/// Framing or compatibility errors for execution-state artifacts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    /// The artifact magic or schema version is not recognized.
    UnsupportedFormat,
    /// The artifact architecture tag is not recognized.
    UnsupportedArchitecture,
    /// The artifact backend tag is not recognized.
    UnsupportedBackend,
    /// A declared byte length exceeds its bound or the available input.
    InvalidLength,
    /// The artifact contains too many vCPUs.
    TooManyVcpus,
    /// A vCPU id occurs more than once.
    DuplicateVcpu,
    /// Trailing bytes follow an otherwise complete artifact.
    TrailingBytes,
}

/// Result type for execution-state framing operations.
pub type Result<T> = std::result::Result<T, Error>;

/// Small checked writer shared by backend-local execution codecs.
pub(crate) struct StateWriter {
    bytes: Vec<u8>,
}

/// Small checked reader shared by backend-local execution codecs.
pub(crate) struct StateReader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

//--------------------------------------------------------------------------------------------------
// Methods
//--------------------------------------------------------------------------------------------------

impl VcpuExecutionState {
    pub(crate) fn new(id: u32, bytes: Vec<u8>) -> Result<Self> {
        if bytes.len() > MAX_VCPU_STATE_BYTES {
            return Err(Error::InvalidLength);
        }
        Ok(Self { id, bytes })
    }

    /// Returns the backend vCPU index.
    pub fn id(&self) -> u32 {
        self.id
    }

    /// Returns the opaque backend payload.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

impl ExecutionState {
    pub(crate) fn new(
        architecture: ExecutionArchitecture,
        backend: ExecutionBackend,
        backend_state_abi: u32,
        pause_generation: u64,
        vm_state: Vec<u8>,
        mut vcpus: Vec<VcpuExecutionState>,
    ) -> Result<Self> {
        if vm_state.len() > MAX_VM_STATE_BYTES {
            return Err(Error::InvalidLength);
        }
        if vcpus.len() > MAX_VCPUS {
            return Err(Error::TooManyVcpus);
        }
        vcpus.sort_unstable_by_key(VcpuExecutionState::id);
        if vcpus.windows(2).any(|pair| pair[0].id == pair[1].id) {
            return Err(Error::DuplicateVcpu);
        }
        Ok(Self {
            architecture,
            backend,
            backend_state_abi,
            pause_generation,
            vm_state,
            vcpus,
        })
    }

    /// Returns the guest architecture represented by this artifact.
    pub fn architecture(&self) -> ExecutionArchitecture {
        self.architecture
    }

    /// Returns the hypervisor backend that owns the opaque payloads.
    pub fn backend(&self) -> ExecutionBackend {
        self.backend
    }

    /// Returns the version of the backend payload contract.
    pub fn backend_state_abi(&self) -> u32 {
        self.backend_state_abi
    }

    /// Returns the VM-wide pause generation at which state was captured.
    pub fn pause_generation(&self) -> u64 {
        self.pause_generation
    }

    /// Returns the opaque VM-global backend payload.
    pub fn vm_state(&self) -> &[u8] {
        &self.vm_state
    }

    /// Returns the ordered per-vCPU payloads.
    pub fn vcpus(&self) -> &[VcpuExecutionState] {
        &self.vcpus
    }

    /// Encodes the complete envelope into deterministic bounded bytes.
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut writer = StateWriter::new();
        writer.write_bytes(EXECUTION_STATE_MAGIC);
        writer.write_u16(EXECUTION_STATE_SCHEMA);
        writer.write_u8(self.architecture as u8);
        writer.write_u8(self.backend as u8);
        writer.write_u32(self.backend_state_abi);
        writer.write_u64(self.pause_generation);
        writer.write_len_prefixed(&self.vm_state)?;
        writer.write_u32(self.vcpus.len() as u32);
        for vcpu in &self.vcpus {
            writer.write_u32(vcpu.id);
            writer.write_len_prefixed(&vcpu.bytes)?;
        }
        let bytes = writer.finish();
        if bytes.len() > MAX_EXECUTION_STATE_BYTES {
            return Err(Error::InvalidLength);
        }
        Ok(bytes)
    }

    /// Decodes and validates a complete execution-state envelope.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_EXECUTION_STATE_BYTES {
            return Err(Error::InvalidLength);
        }
        let mut reader = StateReader::new(bytes);
        if reader.read_exact(EXECUTION_STATE_MAGIC.len())? != EXECUTION_STATE_MAGIC {
            return Err(Error::UnsupportedFormat);
        }
        if reader.read_u16()? != EXECUTION_STATE_SCHEMA {
            return Err(Error::UnsupportedFormat);
        }
        let architecture = match reader.read_u8()? {
            1 => ExecutionArchitecture::X86_64,
            2 => ExecutionArchitecture::Aarch64,
            3 => ExecutionArchitecture::Riscv64,
            _ => return Err(Error::UnsupportedArchitecture),
        };
        let backend = match reader.read_u8()? {
            1 => ExecutionBackend::Kvm,
            2 => ExecutionBackend::Hvf,
            3 => ExecutionBackend::Whp,
            _ => return Err(Error::UnsupportedBackend),
        };
        let backend_state_abi = reader.read_u32()?;
        let pause_generation = reader.read_u64()?;
        let vm_state = reader.read_len_prefixed(MAX_VM_STATE_BYTES)?.to_vec();
        let vcpu_count = reader.read_u32()? as usize;
        if vcpu_count > MAX_VCPUS {
            return Err(Error::TooManyVcpus);
        }
        let mut ids = BTreeSet::new();
        let mut vcpus = Vec::with_capacity(vcpu_count);
        for _ in 0..vcpu_count {
            let id = reader.read_u32()?;
            if !ids.insert(id) {
                return Err(Error::DuplicateVcpu);
            }
            let payload = reader.read_len_prefixed(MAX_VCPU_STATE_BYTES)?.to_vec();
            vcpus.push(VcpuExecutionState::new(id, payload)?);
        }
        if !reader.is_empty() {
            return Err(Error::TrailingBytes);
        }
        Self::new(
            architecture,
            backend,
            backend_state_abi,
            pause_generation,
            vm_state,
            vcpus,
        )
    }
}

impl StateWriter {
    pub(crate) fn new() -> Self {
        Self { bytes: Vec::new() }
    }

    pub(crate) fn write_u8(&mut self, value: u8) {
        self.bytes.push(value);
    }

    pub(crate) fn write_u16(&mut self, value: u16) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    pub(crate) fn write_u32(&mut self, value: u32) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    pub(crate) fn write_u64(&mut self, value: u64) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    pub(crate) fn write_bytes(&mut self, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
    }

    pub(crate) fn write_len_prefixed(&mut self, bytes: &[u8]) -> Result<()> {
        let length = u32::try_from(bytes.len()).map_err(|_| Error::InvalidLength)?;
        self.write_u32(length);
        self.write_bytes(bytes);
        Ok(())
    }

    pub(crate) fn finish(self) -> Vec<u8> {
        self.bytes
    }
}

impl<'a> StateReader<'a> {
    pub(crate) fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    pub(crate) fn read_u8(&mut self) -> Result<u8> {
        Ok(self.read_exact(1)?[0])
    }

    pub(crate) fn read_u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(
            self.read_exact(2)?.try_into().expect("fixed-width slice"),
        ))
    }

    pub(crate) fn read_u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(
            self.read_exact(4)?.try_into().expect("fixed-width slice"),
        ))
    }

    pub(crate) fn read_u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(
            self.read_exact(8)?.try_into().expect("fixed-width slice"),
        ))
    }

    pub(crate) fn read_exact(&mut self, length: usize) -> Result<&'a [u8]> {
        let end = self
            .offset
            .checked_add(length)
            .ok_or(Error::InvalidLength)?;
        let bytes = self
            .bytes
            .get(self.offset..end)
            .ok_or(Error::InvalidLength)?;
        self.offset = end;
        Ok(bytes)
    }

    pub(crate) fn read_len_prefixed(&mut self, maximum: usize) -> Result<&'a [u8]> {
        let length = self.read_u32()? as usize;
        if length > maximum {
            return Err(Error::InvalidLength);
        }
        self.read_exact(length)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.offset == self.bytes.len()
    }
}

//--------------------------------------------------------------------------------------------------
// Trait Implementations
//--------------------------------------------------------------------------------------------------

impl Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedFormat => write!(f, "unsupported execution-state format"),
            Self::UnsupportedArchitecture => write!(f, "unsupported execution architecture"),
            Self::UnsupportedBackend => write!(f, "unsupported execution backend"),
            Self::InvalidLength => write!(f, "invalid execution-state length"),
            Self::TooManyVcpus => write!(f, "execution state contains too many vCPUs"),
            Self::DuplicateVcpu => write!(f, "execution state contains a duplicate vCPU id"),
            Self::TrailingBytes => write!(f, "execution state contains trailing bytes"),
        }
    }
}

impl std::error::Error for Error {}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_round_trip_is_deterministic() {
        let state = ExecutionState::new(
            ExecutionArchitecture::Aarch64,
            ExecutionBackend::Hvf,
            3,
            42,
            vec![9, 8, 7],
            vec![
                VcpuExecutionState::new(1, vec![4, 5]).unwrap(),
                VcpuExecutionState::new(0, vec![1, 2, 3]).unwrap(),
            ],
        )
        .unwrap();

        let bytes = state.encode().unwrap();
        let decoded = ExecutionState::decode(&bytes).unwrap();
        assert_eq!(decoded, state);
        assert_eq!(decoded.vcpus()[0].id(), 0);
        assert_eq!(decoded.encode().unwrap(), bytes);
    }

    #[test]
    fn envelope_rejects_truncation_duplicates_and_trailing_bytes() {
        let duplicate = ExecutionState::new(
            ExecutionArchitecture::X86_64,
            ExecutionBackend::Kvm,
            1,
            1,
            Vec::new(),
            vec![
                VcpuExecutionState::new(0, Vec::new()).unwrap(),
                VcpuExecutionState::new(0, Vec::new()).unwrap(),
            ],
        );
        assert_eq!(duplicate, Err(Error::DuplicateVcpu));

        let valid = ExecutionState::new(
            ExecutionArchitecture::X86_64,
            ExecutionBackend::Kvm,
            1,
            1,
            Vec::new(),
            vec![VcpuExecutionState::new(0, vec![1]).unwrap()],
        )
        .unwrap()
        .encode()
        .unwrap();
        assert_eq!(
            ExecutionState::decode(&valid[..valid.len() - 1]),
            Err(Error::InvalidLength)
        );
        let mut trailing = valid;
        trailing.push(0);
        assert_eq!(ExecutionState::decode(&trailing), Err(Error::TrailingBytes));
    }
}
