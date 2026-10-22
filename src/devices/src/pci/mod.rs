// SPDX-License-Identifier: Apache-2.0 AND BSD-3-Clause
//
// PCIe host-bridge transport for libkrun (aarch64, ECAM). Ported from Cloud
// Hypervisor's `pci` crate as the foundation for VFIO GPU passthrough — see
// CUDA_PASSTHROUGH_IMPL.md. Phase 1 (this module) provides ECAM config access
// and an enumerable, initially empty, PCI bus with a host-bridge at 00:00.0.

pub mod bus;
pub mod configuration;
pub mod device;

pub use bus::{PciBus, PciConfigMmio, PciRoot, PciRootError, NUM_DEVICE_IDS, PCI_ROOT_DEVICE_ID};
pub use configuration::{
    PciBarConfiguration, PciBarPrefetchable, PciBarRegionType, PciCapability, PciCapabilityId,
    PciClassCode, PciConfiguration, PciHeaderType, PciProgrammingInterface, PciSubclass,
};
pub use device::{BarReprogrammingParams, DeviceRelocation, PciDevice};
