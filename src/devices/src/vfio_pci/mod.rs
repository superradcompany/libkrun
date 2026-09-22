// SPDX-License-Identifier: Apache-2.0
//
// VFIO PCI passthrough for libkrun (aarch64/linux). A physical PCI device is
// opened through VFIO and presented to the guest on the emulated PCIe bus:
// config space passes through to the device, BARs are emulated + (later) mmap'd
// into guest physical address space. See CUDA_PASSTHROUGH_IMPL.md steps 4-10.

pub mod device;
pub mod vfio;

pub use device::{MmioRegion, VfioPciDevice};
pub use vfio::{VfioDevice, VfioError};
