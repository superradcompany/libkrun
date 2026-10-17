//! Bounded Linux x86 nested-state transport. The VMM admission gate remains closed
//! until fresh-VM restore is qualified on both Intel and AMD hosts.

use std::io;
use std::os::fd::AsRawFd;

use kvm_bindings::{MsrList, Msrs, KVM_CAP_NESTED_STATE, KVM_MAX_MSR_ENTRIES};
use kvm_ioctls::{Kvm, VcpuFd};
use serde::{Deserialize, Serialize};

const HEADER_SIZE: usize = 128;
const MAX_STATE_SIZE: usize = 1024 * 1024;
// Linux x86 UAPI: _IOWR/_IOW(KVMIO, 0xbe/0xbf, struct kvm_nested_state).
const GET_NESTED_STATE: libc::c_ulong = 0xc080_aebe;
const SET_NESTED_STATE: libc::c_ulong = 0x4080_aebf;

/// Host feature MSRs are immutable capabilities, distinct from vCPU runtime MSRs.
/// Exact equality intentionally restricts this draft to the same CPU contract.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Snapshot {
    pub(super) state: Vec<u8>,
    pub(super) feature_msrs: Vec<(u32, u64)>,
}

pub(super) fn runtime_msrs() -> io::Result<MsrList> {
    // The ordinary guest whitelist omits hypervisor state such as AMD's
    // VM_HSAVE_PA and TSC ratio. KVM's index list is its save/restore inventory;
    // nested guests need the complete list, separately from feature MSRs.
    let indices = Kvm::new()
        .map_err(io::Error::from)?
        .get_msr_index_list()
        .map_err(io::Error::from)?;
    if indices.as_slice().len() > KVM_MAX_MSR_ENTRIES {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "KVM runtime MSRs exceed the supported nested-state bound",
        ));
    }
    Ok(indices)
}

fn feature_msrs() -> io::Result<Vec<(u32, u64)>> {
    let kvm = Kvm::new().map_err(io::Error::from)?;
    let indices = kvm.get_msr_feature_index_list().map_err(io::Error::from)?;
    let mut entries = Msrs::new(indices.as_slice().len()).map_err(io::Error::other)?;
    for (entry, index) in entries.as_mut_slice().iter_mut().zip(indices.as_slice()) {
        entry.index = *index;
    }
    let count = kvm.get_msrs(&mut entries).map_err(io::Error::from)?;
    if count != entries.as_slice().len() {
        return Err(io::Error::other("KVM returned incomplete feature MSRs"));
    }
    let mut features = entries
        .as_slice()
        .iter()
        .map(|entry| (entry.index, entry.data))
        .collect::<Vec<_>>();
    features.sort_unstable_by_key(|(index, _)| *index);
    Ok(features)
}

fn capacity() -> io::Result<usize> {
    let capacity = Kvm::new()
        .map_err(io::Error::from)?
        .check_extension_raw(KVM_CAP_NESTED_STATE.into());
    if capacity < HEADER_SIZE as i32 || capacity as usize > MAX_STATE_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "KVM nested-state export is unavailable or exceeds the supported bound",
        ));
    }
    Ok(capacity as usize)
}

fn validate(bytes: &[u8]) -> io::Result<()> {
    if !(HEADER_SIZE..=MAX_STATE_SIZE).contains(&bytes.len()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid KVM nested-state length",
        ));
    }
    let size = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
    let format = u16::from_le_bytes(bytes[2..4].try_into().unwrap());
    if size != bytes.len() || format > 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid KVM nested-state header",
        ));
    }
    Ok(())
}

pub(super) fn capture(vcpu: &VcpuFd) -> io::Result<Snapshot> {
    let capacity = capacity()?;
    // u64 storage provides alignment for the UAPI header; zero initialization also
    // prevents uninitialized padding from entering the snapshot.
    let mut words = vec![0_u64; capacity.div_ceil(8)];
    let bytes =
        unsafe { std::slice::from_raw_parts_mut(words.as_mut_ptr().cast::<u8>(), capacity) };
    bytes[4..8].copy_from_slice(&(capacity as u32).to_le_bytes());
    // SAFETY: storage is aligned, writable, and covers the advertised capacity.
    if unsafe { libc::ioctl(vcpu.as_raw_fd(), GET_NESTED_STATE, bytes.as_mut_ptr()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let used = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
    if used > capacity {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "KVM returned oversized nested state",
        ));
    }
    let state = &bytes[..used];
    validate(state)?;
    Ok(Snapshot {
        state: state.to_vec(),
        feature_msrs: feature_msrs()?,
    })
}

pub(super) fn validate_destination(snapshot: &Snapshot) -> io::Result<()> {
    let bytes = &snapshot.state;
    validate(bytes)?;
    if snapshot.feature_msrs != feature_msrs()? {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "destination KVM feature MSRs differ from nested snapshot",
        ));
    }
    if bytes.len() > capacity()? {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "destination KVM cannot accept this nested-state size",
        ));
    }
    Ok(())
}

pub(super) fn restore(vcpu: &VcpuFd, snapshot: &Snapshot) -> io::Result<()> {
    validate_destination(snapshot)?;
    let bytes = &snapshot.state;
    let mut words = vec![0_u64; bytes.len().div_ceil(8)];
    // SAFETY: the allocation covers bytes.len() bytes and is suitably aligned.
    let aligned =
        unsafe { std::slice::from_raw_parts_mut(words.as_mut_ptr().cast::<u8>(), bytes.len()) };
    aligned.copy_from_slice(bytes);
    // KVM validates VMX/SVM format, flags and guest physical addresses against
    // the configured destination vCPU. Never substitute an empty state on error.
    if unsafe { libc::ioctl(vcpu.as_raw_fd(), SET_NESTED_STATE, aligned.as_ptr()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires Linux x86 KVM with nested-state support"]
    fn nested_state_transfers_between_fresh_vcpus() {
        let kvm = Kvm::new().unwrap();
        let cpuid = kvm
            .get_supported_cpuid(kvm_bindings::KVM_MAX_CPUID_ENTRIES)
            .unwrap();
        let source_vm = kvm.create_vm().unwrap();
        let source = source_vm.create_vcpu(0).unwrap();
        source.set_cpuid2(&cpuid).unwrap();
        let saved = capture(&source).unwrap();
        let destination_vm = kvm.create_vm().unwrap();
        let destination = destination_vm.create_vcpu(0).unwrap();
        destination.set_cpuid2(&cpuid).unwrap();
        restore(&destination, &saved).unwrap();
        assert_eq!(capture(&destination).unwrap(), saved);

        let before = capture(&destination).unwrap();
        let mut incompatible = saved;
        incompatible.feature_msrs.push((u32::MAX, 0));
        assert_eq!(
            restore(&destination, &incompatible).unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
        assert_eq!(capture(&destination).unwrap(), before);
    }

    #[test]
    fn nested_state_rejects_invalid_wire_headers() {
        let mut state = vec![0; HEADER_SIZE];
        state[4..8].copy_from_slice(&(HEADER_SIZE as u32).to_le_bytes());
        for format in [0_u16, 1] {
            state[2..4].copy_from_slice(&format.to_le_bytes());
            validate(&state).unwrap();
        }
        assert!(validate(&state[..HEADER_SIZE - 1]).is_err());
        state[2..4].copy_from_slice(&2_u16.to_le_bytes());
        assert!(validate(&state).is_err());
        state[2..4].copy_from_slice(&0_u16.to_le_bytes());
        state[4..8].copy_from_slice(&((HEADER_SIZE + 1) as u32).to_le_bytes());
        assert!(validate(&state).is_err());
    }
}
