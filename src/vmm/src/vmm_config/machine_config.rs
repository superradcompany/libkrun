// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::fmt;

/// Ceiling for possible vCPUs (`max_vcpu_count`). Matches `CONFIG_NR_CPUS=64` in the
/// non-confidential libkrunfw kernels; a wider topology than the guest kernel can
/// address would silently strand the extra CPUs.
pub const MAX_SUPPORTED_VCPUS: u8 = 64;

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

/// Identifies one logical processor in the host's processor topology.
///
/// Linux currently supports processor group zero. The group is explicit so callers do not need a
/// different public representation when grouped processor topologies are supported on Windows.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HostCpuId {
    /// Host processor group.
    pub group: u16,
    /// Logical processor index within the group.
    pub index: u16,
}

//--------------------------------------------------------------------------------------------------
// Methods
//--------------------------------------------------------------------------------------------------

impl HostCpuId {
    /// Creates a host CPU identifier in processor group zero.
    pub const fn new(index: u16) -> Self {
        Self { group: 0, index }
    }

    /// Creates a host CPU identifier in the specified processor group.
    pub const fn in_group(group: u16, index: u16) -> Self {
        Self { group, index }
    }
}

/// Errors associated with configuring the microVM.
#[derive(Debug, Eq, PartialEq)]
pub enum VmConfigError {
    /// The vcpu count is invalid. When hyperthreading is enabled, the `cpu_count` must be either
    /// 1 or an even number.
    InvalidVcpuCount,
    /// The memory size is invalid. The memory can only be an unsigned integer.
    InvalidMemorySize,
    /// The maximum vcpu count is invalid: it must be non-zero, at least the effective vcpu
    /// count, no larger than the supported vcpu limit, and even when hyperthreading is enabled.
    InvalidMaxVcpuCount,
    /// The maximum memory size is invalid: it must be non-zero and at least the boot memory size.
    InvalidMaxMemorySize,
    /// Reserving capacity above the initial resources is not supported on this platform yet.
    MaxCapacityUnsupported,
}

impl fmt::Display for VmConfigError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use self::VmConfigError::*;
        match *self {
            InvalidVcpuCount => write!(
                f,
                "The vCPU number is invalid! The vCPU number can only \
                 be 1 or an even number when hyperthreading is enabled.",
            ),
            InvalidMemorySize => write!(f, "The memory size (MiB) is invalid.",),
            InvalidMaxVcpuCount => write!(
                f,
                "The maximum vCPU number is invalid! It must be at least the \
                 vCPU count and no larger than the supported vCPU limit.",
            ),
            InvalidMaxMemorySize => write!(
                f,
                "The maximum memory size (MiB) is invalid! It must be at least \
                 the boot memory size.",
            ),
            MaxCapacityUnsupported => write!(
                f,
                "Reserving CPU or memory capacity above the initial resources \
                 is not supported on this platform yet.",
            ),
        }
    }
}

/// Strongly typed structure that represents the configuration of the
/// microvm.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VmConfig {
    /// The number of vCPUs online at boot.
    pub vcpu_count: Option<u8>,
    /// The boot memory size in MiB.
    pub mem_size_mib: Option<usize>,
    /// Maximum possible vCPUs. The VM boots with this topology created but only `vcpu_count`
    /// online, so the guest can online the rest later. `None` means equal to `vcpu_count`.
    pub max_vcpu_count: Option<u8>,
    /// Maximum guest memory in MiB reserved for future hotplug (virtio-mem). `None` means
    /// equal to `mem_size_mib`. Currently config plumbing only; no device consumes it yet.
    pub max_mem_size_mib: Option<usize>,
    /// Enables or disabled hyperthreading.
    pub ht_enabled: Option<bool>,
    /// A CPU template that it is used to filter the CPU features exposed to the guest.
    pub cpu_template: Option<CpuFeaturesTemplate>,
}

impl Default for VmConfig {
    fn default() -> Self {
        VmConfig {
            vcpu_count: Some(1),
            mem_size_mib: Some(128),
            max_vcpu_count: None,
            max_mem_size_mib: None,
            ht_enabled: Some(false),
            cpu_template: None,
        }
    }
}

impl fmt::Display for VmConfig {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let vcpu_count = self.vcpu_count.unwrap_or(1);
        let mem_size = self.mem_size_mib.unwrap_or(128);
        let max_vcpu_count = self.max_vcpu_count.unwrap_or(vcpu_count);
        let max_mem_size = self.max_mem_size_mib.unwrap_or(mem_size);
        let ht_enabled = self.ht_enabled.unwrap_or(false);
        let cpu_template = self
            .cpu_template
            .map_or("Uninitialized".to_string(), |c| c.to_string());

        write!(f, "{{ \"vcpu_count\": {vcpu_count:?}, \"mem_size_mib\": {mem_size:?},  \"max_vcpu_count\": {max_vcpu_count:?},  \"max_mem_size_mib\": {max_mem_size:?},  \"ht_enabled\": {ht_enabled:?},  \"cpu_template\": {cpu_template:?} }}")
    }
}

/// Template types available for configuring the CPU features that map
/// to EC2 instances.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CpuFeaturesTemplate {
    /// C3 Template.
    C3,
    /// T2 Template.
    T2,
}

impl fmt::Display for CpuFeaturesTemplate {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            CpuFeaturesTemplate::C3 => write!(f, "C3"),
            CpuFeaturesTemplate::T2 => write!(f, "T2"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_display_cpu_features_template() {
        assert_eq!(CpuFeaturesTemplate::C3.to_string(), "C3".to_string());
        assert_eq!(CpuFeaturesTemplate::T2.to_string(), "T2".to_string());
    }

    #[test]
    fn test_display_vm_config_error() {
        let expected_str = "The vCPU number is invalid! The vCPU number can only \
                            be 1 or an even number when hyperthreading is enabled.";
        assert_eq!(VmConfigError::InvalidVcpuCount.to_string(), expected_str);

        let expected_str = "The memory size (MiB) is invalid.";
        assert_eq!(VmConfigError::InvalidMemorySize.to_string(), expected_str);
    }
}
