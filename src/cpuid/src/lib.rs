// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the THIRD-PARTY file.

#![deny(missing_docs)]
//! Utility for configuring the CPUID (CPU identification) for the guest microVM.

#![cfg(target_arch = "x86_64")]

use kvm_bindings::CpuId;

mod common;
use crate::common::*;

/// Contains helper methods for bit operations.
pub mod bit_helper;

mod template;
pub use crate::template::c3;
pub use crate::template::t2;

mod cpu_leaf;

mod transformer;
use crate::transformer::*;
pub use crate::transformer::{Error, VmSpec};

mod brand_string;

/// Sets up the CPUID entries for the given vcpu.
///
/// # Arguments
///
/// * `kvm_cpuid` - KVM related structure holding the relevant CPUID info.
/// * `vm_spec` - The specifications of the VM.
///
/// # Example
/// ```
/// use msb_krun_cpuid::{filter_cpuid, VmSpec};
/// use kvm_bindings::{CpuId, KVM_MAX_CPUID_ENTRIES};
/// use kvm_ioctls::Kvm;
///
/// let kvm = Kvm::new().unwrap();
/// let mut kvm_cpuid: CpuId = kvm.get_supported_cpuid(KVM_MAX_CPUID_ENTRIES).unwrap();
///
/// let vm_spec = VmSpec::new(0, 1, true).unwrap();
///
/// filter_cpuid(&mut kvm_cpuid, &vm_spec).unwrap();
///
/// // Get expected `kvm_cpuid` entries.
/// let entries = kvm_cpuid.as_mut_slice();
/// ```
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
pub fn filter_cpuid(kvm_cpuid: &mut CpuId, vm_spec: &VmSpec) -> Result<(), Error> {
    let maybe_cpuid_transformer: Option<&dyn CpuidTransformer> = match vm_spec.cpu_vendor_id() {
        VENDOR_ID_INTEL => Some(&intel::IntelCpuidTransformer {}),
        VENDOR_ID_AMD => Some(&amd::AmdCpuidTransformer {}),
        _ => None,
    };

    if let Some(cpuid_transformer) = maybe_cpuid_transformer {
        cpuid_transformer.process_cpuid(kvm_cpuid, vm_spec)?;
    }

    // With nested virtualization disabled, clear the hardware virtualization
    // feature bits so the guest cannot run its own VMs.
    if !vm_spec.nested_enabled() {
        mask_nested_virt(kvm_cpuid);
    }

    Ok(())
}

/// Clear the hardware virtualization feature bits from the guest CPUID: VMX
/// (leaf 0x1, ECX bit 5) and SVM (leaf 0x8000_0001, ECX bit 2).
fn mask_nested_virt(kvm_cpuid: &mut CpuId) {
    const VMX_BIT: u32 = 1 << 5;
    const SVM_BIT: u32 = 1 << 2;
    for entry in kvm_cpuid.as_mut_slice() {
        if entry.index != 0 {
            continue;
        }
        match entry.function {
            0x1 => entry.ecx &= !VMX_BIT,
            0x8000_0001 => entry.ecx &= !SVM_BIT,
            _ => {}
        }
    }
}

#[cfg(all(test, any(target_arch = "x86", target_arch = "x86_64")))]
mod tests {
    use super::*;
    use kvm_bindings::kvm_cpuid_entry2;

    #[test]
    fn test_vm_spec_nested_enabled() {
        let mut vm_spec = VmSpec::new(0, 1, false).unwrap();
        assert!(!vm_spec.nested_enabled());

        vm_spec.set_nested_enabled(true);
        assert!(vm_spec.nested_enabled());

        vm_spec.set_nested_enabled(false);
        assert!(!vm_spec.nested_enabled());
    }

    #[test]
    fn test_mask_nested_virt() {
        let mut cpuid = CpuId::new(2).unwrap();
        cpuid.as_mut_slice()[0] = kvm_cpuid_entry2 {
            function: 0x1,
            index: 0,
            ecx: u32::MAX,
            ..Default::default()
        };
        cpuid.as_mut_slice()[1] = kvm_cpuid_entry2 {
            function: 0x8000_0001,
            index: 0,
            ecx: u32::MAX,
            ..Default::default()
        };

        mask_nested_virt(&mut cpuid);

        let entries = cpuid.as_slice();
        assert_eq!(entries[0].ecx & (1 << 5), 0, "VMX must be cleared");
        assert_eq!(entries[1].ecx & (1 << 2), 0, "SVM must be cleared");
        // Unrelated bits are preserved.
        assert_eq!(entries[0].ecx, !(1u32 << 5));
    }

    /// A CPUID with the Intel VMX and AMD SVM feature bits set.
    fn virt_cpuid() -> CpuId {
        let mut cpuid = CpuId::new(2).unwrap();
        cpuid.as_mut_slice()[0] = kvm_cpuid_entry2 {
            function: 0x1,
            index: 0,
            ecx: 1 << 5,
            ..Default::default()
        };
        cpuid.as_mut_slice()[1] = kvm_cpuid_entry2 {
            function: 0x8000_0001,
            index: 0,
            ecx: 1 << 2,
            ..Default::default()
        };
        cpuid
    }

    /// The VMX/SVM bits the guest would see in a filtered CPUID.
    fn virt_bits(cpuid: &CpuId) -> u32 {
        let mut bits = 0;
        for entry in cpuid.as_slice() {
            if entry.index == 0 {
                match entry.function {
                    0x1 => bits |= entry.ecx & (1 << 5),
                    0x8000_0001 => bits |= entry.ecx & (1 << 2),
                    _ => {}
                }
            }
        }
        bits
    }

    #[test]
    fn test_filter_cpuid_masks_nested_virt_when_disabled() {
        let vm_spec = VmSpec::new(0, 1, false).unwrap();
        let mut cpuid = virt_cpuid();
        filter_cpuid(&mut cpuid, &vm_spec).unwrap();
        assert_eq!(virt_bits(&cpuid), 0);
    }

    #[test]
    fn test_filter_cpuid_keeps_nested_virt_when_enabled() {
        let mut vm_spec = VmSpec::new(0, 1, false).unwrap();
        vm_spec.set_nested_enabled(true);
        assert!(vm_spec.nested_enabled());
        let mut cpuid = virt_cpuid();
        filter_cpuid(&mut cpuid, &vm_spec).unwrap();
        assert_eq!(virt_bits(&cpuid), (1 << 5) | (1 << 2));
    }
}
