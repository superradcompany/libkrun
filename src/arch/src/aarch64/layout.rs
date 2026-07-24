// Copyright 2019 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//      ==== Address map in use in ARM development systems today ====
//
//              - 32-bit -              - 36-bit -          - 40-bit -
//1024GB    +                   +                      +-------------------+     <- 40-bit
//          |                                           | DRAM              |
//          ~                   ~                       ~                   ~
//          |                                           |                   |
//          |                                           |                   |
//          |                                           |                   |
//          |                                           |                   |
//544GB     +                   +                       +-------------------+
//          |                                           | Hole or DRAM      |
//          |                                           |                   |
//512GB     +                   +                       +-------------------+
//          |                                           |       Mapped      |
//          |                                           |       I/O         |
//          ~                   ~                       ~                   ~
//          |                                           |                   |
//256GB     +                   +                       +-------------------+
//          |                                           |       Reserved    |
//          ~                   ~                       ~                   ~
//          |                                           |                   |
//64GB      +                   +-----------------------+-------------------+   <- 36-bit
//          |                   |                   DRAM                    |
//          ~                   ~                   ~                       ~
//          |                   |                                           |
//          |                   |                                           |
//34GB      +                   +-----------------------+-------------------+
//          |                   |                  Hole or DRAM             |
//32GB      +                   +-----------------------+-------------------+
//          |                   |                   Mapped I/O              |
//          ~                   ~                       ~                   ~
//          |                   |                                           |
//16GB      +                   +-----------------------+-------------------+
//          |                   |                   Reserved                |
//          ~                   ~                       ~                   ~
//4GB       +-------------------+-----------------------+-------------------+   <- 32-bit
//          |           2GB of DRAM                                         |
//          |                                                               |
//2GB       +-------------------+-----------------------+-------------------+
//          |                           Mapped I/O                          |
//1GB       +-------------------+-----------------------+-------------------+
//          |                          ROM & RAM & I/O                      |
//0GB       +-------------------+-----------------------+-------------------+   0
//              - 32-bit -              - 36-bit -              - 40-bit -
//
// Taken from (http://infocenter.arm.com/help/topic/com.arm.doc.den0001c/DEN0001C_principles_of_arm_memory_maps.pdf).

/// Start of RAM on 64 bit ARM when loading an EFI firmware.
pub const DRAM_MEM_START_EFI: u64 = 0x4000_0000; // 1 GB.
/// Start of RAM on 64 bit ARM when loading a kernel.
pub const DRAM_MEM_START_KERNEL: u64 = 0x8000_0000; // 2 GB.
/// The maximum addressable RAM address.
pub const DRAM_MEM_END: u64 = 0x00FF_8000_0000; // 1024 - 2 = 1022 GB.
/// The maximum RAM size.
pub const DRAM_MEM_MAX_SIZE: u64 = DRAM_MEM_END - DRAM_MEM_START_KERNEL;

/// Kernel command line maximum size.
/// Matches msb-krunfw's `arch/arm64/include/uapi/asm/setup.h`.
pub const CMDLINE_MAX_SIZE: usize = 16 * 1024;

/// Maximum size of the device tree blob as specified in https://www.kernel.org/doc/Documentation/arm64/booting.txt.
pub const FDT_MAX_SIZE: usize = 0x20_0000;

// As per virt/kvm/arm/vgic/vgic-kvm-device.c we need
// the number of interrupts our GIC will support to be:
// * bigger than 32
// * less than 1023 and
// * a multiple of 32.
// We are setting up our interrupt controller to support a maximum of 128 interrupts.
/// First usable interrupt on aarch64.
pub const IRQ_BASE: u32 = 32;

/// Last usable interrupt on aarch64.
pub const IRQ_MAX: u32 = 223;

/// Timer interrupts
pub const GTIMER_SEC: u32 = 13;
pub const GTIMER_HYP: u32 = 14;
pub const GTIMER_VIRT: u32 = 11;
pub const GTIMER_PHYS: u32 = 12;

pub const VTIMER_IRQ: u32 = GTIMER_VIRT + 16;

/// Below this address will reside the GIC, above this address will reside the MMIO devices.
pub const MAPPED_IO_START: u64 = 0x0a00_0000;

// ==== PCIe host bridge (feature = "pci"/"vfio") ====
// QEMU-virt / Cloud-Hypervisor-compatible layout. All windows sit below EFI RAM
// (0x4000_0000) or high above guest RAM, so they are valid for both the EFI and
// direct-kernel boot modes and clear of the GIC (<0x0a00_0000) and the
// virtio-MMIO band (grows up from MAPPED_IO_START).
/// 32-bit PCI MMIO / BAR aperture (holds small 32-bit BARs + the MSI-X table page).
pub const PCIE_MMIO32_BASE: u64 = 0x1000_0000;
pub const PCIE_MMIO32_SIZE: u64 = 0x2000_0000; // 512 MiB, ends at PCIE_ECAM_BASE
/// PCIe ECAM window — buses 0-1 (root bus + root port), 256 devices x 4 KiB x 2 = 2 MiB.
pub const PCIE_ECAM_BASE: u64 = 0x3000_0000;
pub const PCIE_ECAM_SIZE: u64 = 0x0020_0000;
/// Highest bus number the ECAM window can address (1 MiB of config space per
/// bus). The FDT `bus-range` and every assigned bus number (e.g. the root
/// port's secondary bus) must stay <= this, or that bus's config space would
/// fall outside the ECAM MMIO region and be invisible. Derived from
/// `PCIE_ECAM_SIZE` so the two never drift.
pub const PCIE_MAX_BUS: u8 = (PCIE_ECAM_SIZE / 0x0010_0000 - 1) as u8;
/// 64-bit high PCI MMIO / BAR aperture — above any realistic guest RAM+shm, holds
/// large 64-bit prefetchable BARs (incl. resizable-BAR datacenter GPUs).
pub const PCIE_MMIO64_BASE: u64 = 0x40_0000_0000; // 256 GiB
pub const PCIE_MMIO64_SIZE: u64 = 0x40_0000_0000; // 256 GiB

/// The address to put the SMBIOS contents, if present.
pub const SMBIOS_START: u64 = 0x4000_F000;

/// Where the PC register will point after a reset.
pub const RESET_VECTOR: u64 = 0x0;

/// The address to load the firmware, if present.
pub const FIRMWARE_START: u64 = 0;
