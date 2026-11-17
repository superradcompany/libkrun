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
const NESTED_EXECUTION_STATE_SCHEMA: u16 = 2;
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
    nested_enabled: bool,
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
    /// Guest counter frequency required before constructing a Windows x86 partition.
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    pub fn tsc_frequency_hz(&self) -> std::result::Result<u64, String> {
        if self.architecture == ExecutionArchitecture::X86_64
            && self.backend == ExecutionBackend::Whp
            && self.backend_state_abi == if self.nested_enabled { 3 } else { 1 }
        {
            return Err(crate::Error::ExecutionStateMissingPit.to_string());
        }
        if self.architecture != ExecutionArchitecture::X86_64
            || self.backend != ExecutionBackend::Whp
            || self.backend_state_abi != if self.nested_enabled { 4 } else { 2 }
        {
            return Err("unsupported Windows execution state for clock restore".into());
        }
        let (vm, used): (crate::WindowsX86VmExecutionState, usize) =
            bincode::serde::decode_from_slice(
                &self.vm_state,
                bincode::config::standard().with_limit::<{ 1024 * 1024 }>(),
            )
            .map_err(|e| e.to_string())?;
        if used != self.vm_state.len() {
            return Err("trailing WHP VM state".into());
        }
        crate::windows::vstate::x86_state::saved_frequency(&vm.vm, self.nested_enabled)
            .map_err(|e| e.to_string())
    }

    /// Recover an older Windows x86 snapshot by explicitly supplying a periodic PIT reload.
    ///
    /// Only ordinary ABI 1 and enlightened nested ABI 3 can be recovered. The original
    /// timer phase, other PIT channel programming, and any partially written reload were
    /// not saved. This opt-in starts channel 0 at a fresh full period; it cannot promise
    /// exact timer continuity. Capture a new snapshot after validating the recovered guest.
    pub fn recover_windows_periodic_pit(self, divisor: u32) -> std::result::Result<Self, String> {
        #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
        {
            if self.architecture != ExecutionArchitecture::X86_64
                || self.backend != ExecutionBackend::Whp
                || self.backend_state_abi != if self.nested_enabled { 3 } else { 1 }
            {
                return Err("periodic PIT recovery requires an older Windows x86 snapshot without PIT state; older non-enlightened nested state is not recoverable".into());
            }
            #[derive(serde::Deserialize)]
            struct Legacy {
                vm: Vec<u8>,
                ioapic: Vec<u8>,
            }
            let (legacy, used): (Legacy, usize) = bincode::serde::decode_from_slice(
                &self.vm_state,
                bincode::config::standard().with_limit::<{ 1024 * 1024 }>(),
            )
            .map_err(|e| e.to_string())?;
            if used != self.vm_state.len() {
                return Err("trailing legacy Windows VM state".into());
            }
            let state = crate::WindowsX86VmExecutionState {
                vm: legacy.vm,
                ioapic: legacy.ioapic,
                pit: crate::windows::pit::PitState::recover_periodic(divisor)?,
            };
            Ok(Self {
                backend_state_abi: if self.nested_enabled { 4 } else { 2 },
                vm_state: bincode::serde::encode_to_vec(state, bincode::config::standard())
                    .map_err(|e| e.to_string())?,
                ..self
            })
        }
        #[cfg(not(all(target_os = "windows", target_arch = "x86_64")))]
        {
            let _ = divisor;
            Err("legacy Windows PIT recovery requires a Windows x86 host".into())
        }
    }

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
            nested_enabled: false,
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

    /// Whether this artifact requires nested virtualization in the destination VM.
    pub fn nested_enabled(&self) -> bool {
        self.nested_enabled
    }

    pub(crate) fn with_nested_enabled(mut self, enabled: bool) -> Self {
        self.nested_enabled = enabled;
        self
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
        writer.write_u16(if self.nested_enabled {
            NESTED_EXECUTION_STATE_SCHEMA
        } else {
            EXECUTION_STATE_SCHEMA
        });
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
        let nested_enabled = match reader.read_u16()? {
            EXECUTION_STATE_SCHEMA => false,
            NESTED_EXECUTION_STATE_SCHEMA => true,
            _ => return Err(Error::UnsupportedFormat),
        };
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
        .map(|state| state.with_nested_enabled(nested_enabled))
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

    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    #[test]
    fn legacy_pit_recovery_preserves_cpu_contract_and_requires_a_known_old_format() {
        // ABI 1/3 encoded only two byte vectors: VM [42] and IOAPIC [17].
        // Keep these legacy bytes independent of the current serializer.
        let old = ExecutionState::new(
            ExecutionArchitecture::X86_64,
            ExecutionBackend::Whp,
            1,
            91,
            vec![1, 42, 1, 17],
            vec![VcpuExecutionState::new(0, vec![7, 8]).unwrap()],
        )
        .unwrap();
        for nested in [false, true] {
            let mut source = old.clone().with_nested_enabled(nested);
            source.backend_state_abi = if nested { 3 } else { 1 };
            let recovered = source.clone().recover_windows_periodic_pit(4773).unwrap();
            assert_eq!(recovered.backend_state_abi, if nested { 4 } else { 2 });
            assert_eq!(recovered.vcpus, source.vcpus);
            assert_eq!(recovered.pause_generation, 91);
            assert_eq!(recovered.nested_enabled, nested);
            let (vm, used): (crate::WindowsX86VmExecutionState, _) =
                bincode::serde::decode_from_slice(&recovered.vm_state, bincode::config::standard())
                    .unwrap();
            assert_eq!(used, recovered.vm_state.len());
            assert_eq!(vm.vm, [42]);
            assert_eq!(vm.ioapic, [17]);
            assert!(recovered.recover_windows_periodic_pit(4773).is_err());
        }
        for divisor in [0, 65537, u32::MAX] {
            assert!(old.clone().recover_windows_periodic_pit(divisor).is_err());
        }
        let mut ordinary_vmcs = old.clone().with_nested_enabled(true);
        ordinary_vmcs.backend_state_abi = 2;
        assert!(ordinary_vmcs.recover_windows_periodic_pit(4773).is_err());
        let mut trailing = old.clone();
        trailing.vm_state.push(0);
        assert!(trailing.recover_windows_periodic_pit(4773).is_err());
        let mut other_backend = old;
        other_backend.backend = ExecutionBackend::Kvm;
        assert!(other_backend.recover_windows_periodic_pit(4773).is_err());
    }

    #[test]
    fn envelope_preserves_nested_policy_without_changing_ordinary_artifacts() {
        let plain = ExecutionState::new(
            ExecutionArchitecture::X86_64,
            ExecutionBackend::Kvm,
            1,
            1,
            Vec::new(),
            Vec::new(),
        )
        .unwrap();
        let bytes = plain.encode().unwrap();
        assert_eq!(&bytes[9..11], &1_u16.to_le_bytes());
        assert!(!ExecutionState::decode(&bytes).unwrap().nested_enabled());

        let nested = plain.with_nested_enabled(true);
        let bytes = nested.encode().unwrap();
        assert_eq!(&bytes[9..11], &2_u16.to_le_bytes());
        assert_eq!(ExecutionState::decode(&bytes).unwrap(), nested);
    }

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
