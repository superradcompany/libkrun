//! Host support and admission for nested virtualization.

use std::io;

/// Query whether this backend can expose hardware virtualization to a guest.
/// A positive result does not imply that the guest kernel includes KVM.
pub fn supported() -> io::Result<bool> {
    if cfg!(any(feature = "tee", feature = "aws-nitro")) {
        return Ok(false);
    }
    host_supported()
}

/// Refuse an enabled policy when the backend cannot honor it.
pub(crate) fn validate(enabled: bool) -> io::Result<()> {
    if !enabled {
        return Ok(());
    }
    if supported()? {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "nested virtualization is unavailable on this host or backend",
        ))
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn host_supported() -> io::Result<bool> {
    let kvm = kvm_ioctls::Kvm::new().map_err(io::Error::from)?;
    let cpuid = kvm
        .get_supported_cpuid(kvm_bindings::KVM_MAX_CPUID_ENTRIES)
        .map_err(io::Error::from)?;
    Ok(cpuid.as_slice().iter().any(|entry| {
        entry.index == 0
            && match entry.function {
                0x1 => entry.ecx & (1 << 5) != 0,
                0x8000_0001 => entry.ecx & (1 << 2) != 0,
                _ => false,
            }
    }))
}

#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
fn host_supported() -> io::Result<bool> {
    // Linux UAPI KVM_CAP_ARM_EL2; older kvm-bindings releases omit the constant.
    const KVM_CAP_ARM_EL2: libc::c_ulong = 240;
    let kvm = kvm_ioctls::Kvm::new().map_err(io::Error::from)?;
    Ok(kvm.check_extension_raw(KVM_CAP_ARM_EL2) > 0)
}

#[cfg(target_os = "macos")]
fn host_supported() -> io::Result<bool> {
    hvf::check_nested_virt().map_err(|error| io::Error::other(format!("{error:?}")))
}

#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
fn host_supported() -> io::Result<bool> {
    crate::windows::vstate::nested_virt_supported()
}

#[cfg(not(any(
    all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ),
    target_os = "macos",
    all(target_os = "windows", target_arch = "x86_64")
)))]
fn host_supported() -> io::Result<bool> {
    Ok(false)
}
