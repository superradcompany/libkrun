// Copyright 2019 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::io;

use crate::bus::BusDevice;
use crate::legacy::gic::GICDevice;
use crate::legacy::irqchip::IrqChipT;
use crate::Error as DeviceError;

use kvm_ioctls::{DeviceFd, Error, VmFd};
use utils::eventfd::EventFd;

const KVM_VGIC_V3_BASE_SIZE: u64 = 0x0001_0000;

// GICv3 ITS (MSI controller) MMIO window size: two 64 KiB frames.
const KVM_VGIC_V3_ITS_SIZE: u64 = 0x0002_0000;

// Device trees specific constants
const ARCH_GIC_V3_MAINT_IRQ: u32 = 9;

pub struct KvmGicV3 {
    _device_fd: DeviceFd,

    /// GIC device properties, to be used for setting up the fdt entry
    properties: [u64; 4],

    /// The GICv3 ITS device fd (kept alive for the VM lifetime) and its
    /// `[base, size]` MMIO window, when the ITS was successfully created.
    _its_device_fd: Option<DeviceFd>,
    its_properties: Option<[u64; 2]>,

    /// Number of CPUs handled by the device
    vcpu_count: u64,
}

impl KvmGicV3 {
    pub fn new(vm: &VmFd, vcpu_count: u64) -> Result<Self, Error> {
        let dist_size = KVM_VGIC_V3_BASE_SIZE;
        let dist_addr = arch::MMIO_MEM_START - dist_size;
        let redist_size = 2 * dist_size;
        let redists_size = redist_size * vcpu_count;
        let redists_addr = dist_addr - redists_size;
        // Place the ITS MMIO window immediately below the redistributors.
        let its_addr = redists_addr - KVM_VGIC_V3_ITS_SIZE;

        let mut gic_device = kvm_bindings::kvm_create_device {
            type_: kvm_bindings::kvm_device_type_KVM_DEV_TYPE_ARM_VGIC_V3,
            fd: 0,
            flags: 0,
        };
        let device_fd = vm.create_device(&mut gic_device)?;

        let attr = kvm_bindings::kvm_device_attr {
            group: kvm_bindings::KVM_DEV_ARM_VGIC_GRP_ADDR,
            attr: u64::from(kvm_bindings::KVM_VGIC_V3_ADDR_TYPE_DIST),
            addr: &dist_addr as *const u64 as u64,
            flags: 0,
        };
        device_fd.set_device_attr(&attr)?;

        let attr = kvm_bindings::kvm_device_attr {
            group: kvm_bindings::KVM_DEV_ARM_VGIC_GRP_ADDR,
            attr: u64::from(kvm_bindings::KVM_VGIC_V3_ADDR_TYPE_REDIST),
            addr: &redists_addr as *const u64 as u64,
            flags: 0,
        };
        device_fd.set_device_attr(&attr)?;

        // Create the GICv3 ITS (MSI controller) so passed-through PCI devices can
        // deliver MSI-X. This is best-effort: a failure here leaves a working
        // GICv3 without MSI support (legacy INTx / no passthrough interrupts)
        // rather than discarding the GIC and falling back to GICv2. It MUST be
        // created and initialized BEFORE the distributor's CTRL_INIT below —
        // the kernel's `vgic_its_create` returns -EBUSY once the vgic is
        // initialized (this matches Cloud Hypervisor's ordering).
        let (its_device_fd, its_properties) = match Self::create_its(vm, its_addr) {
            Ok(fd) => {
                info!("KVM GICv3 ITS created at {its_addr:#x}");
                (Some(fd), Some([its_addr, KVM_VGIC_V3_ITS_SIZE]))
            }
            Err(e) => {
                warn!("KVM GICv3 ITS creation failed ({e:?}); MSI-X passthrough unavailable");
                (None, None)
            }
        };

        // GIC nr_irqs includes 32 private interrupts (SGIs + PPIs), so we need
        // the SPI count plus 32, rounded up to a multiple of 32.
        let nr_irqs: u32 = (arch::aarch64::layout::IRQ_MAX + 1).div_ceil(32) * 32;
        let nr_irqs_ptr = &nr_irqs as *const u32;
        let attr = kvm_bindings::kvm_device_attr {
            group: kvm_bindings::KVM_DEV_ARM_VGIC_GRP_NR_IRQS,
            attr: 0,
            addr: nr_irqs_ptr as u64,
            flags: 0,
        };
        device_fd.set_device_attr(&attr)?;

        let attr = kvm_bindings::kvm_device_attr {
            group: kvm_bindings::KVM_DEV_ARM_VGIC_GRP_CTRL,
            attr: u64::from(kvm_bindings::KVM_DEV_ARM_VGIC_CTRL_INIT),
            addr: 0,
            flags: 0,
        };
        device_fd.set_device_attr(&attr)?;

        Ok(Self {
            _device_fd: device_fd,
            properties: [dist_addr, dist_size, redists_addr, redists_size],
            _its_device_fd: its_device_fd,
            its_properties,
            vcpu_count,
        })
    }

    /// Create and initialize a GICv3 ITS at MMIO base `its_addr`.
    fn create_its(vm: &VmFd, its_addr: u64) -> Result<DeviceFd, Error> {
        let mut its_device = kvm_bindings::kvm_create_device {
            type_: kvm_bindings::kvm_device_type_KVM_DEV_TYPE_ARM_VGIC_ITS,
            fd: 0,
            flags: 0,
        };
        let its_fd = vm.create_device(&mut its_device)?;

        let attr = kvm_bindings::kvm_device_attr {
            group: kvm_bindings::KVM_DEV_ARM_VGIC_GRP_ADDR,
            attr: u64::from(kvm_bindings::KVM_VGIC_ITS_ADDR_TYPE),
            addr: &its_addr as *const u64 as u64,
            flags: 0,
        };
        its_fd.set_device_attr(&attr)?;

        let attr = kvm_bindings::kvm_device_attr {
            group: kvm_bindings::KVM_DEV_ARM_VGIC_GRP_CTRL,
            attr: u64::from(kvm_bindings::KVM_DEV_ARM_VGIC_CTRL_INIT),
            addr: 0,
            flags: 0,
        };
        its_fd.set_device_attr(&attr)?;

        Ok(its_fd)
    }
}

impl IrqChipT for KvmGicV3 {
    fn get_mmio_addr(&self) -> u64 {
        0
    }

    fn get_mmio_size(&self) -> u64 {
        0
    }

    fn set_irq(
        &self,
        _irq_line: Option<u32>,
        interrupt_evt: Option<&EventFd>,
    ) -> Result<(), DeviceError> {
        if let Some(interrupt_evt) = interrupt_evt {
            if let Err(e) = interrupt_evt.write(1) {
                error!("Failed to signal used queue: {e:?}");
                return Err(DeviceError::FailedSignalingUsedQueue(e));
            }
        } else {
            error!("EventFd not set up for irq line");
            return Err(DeviceError::FailedSignalingUsedQueue(io::Error::new(
                io::ErrorKind::NotFound,
                "EventFd not set up for irq line".to_string(),
            )));
        }
        Ok(())
    }
}

impl BusDevice for KvmGicV3 {
    fn read(&mut self, _vcpuid: u64, _offset: u64, _data: &mut [u8]) {
        unreachable!("MMIO operations are managed in-kernel");
    }

    fn write(&mut self, _vcpuid: u64, _offset: u64, _data: &[u8]) {
        unreachable!("MMIO operations are managed in-kernel");
    }
}

impl GICDevice for KvmGicV3 {
    fn device_properties(&self) -> Vec<u64> {
        self.properties.to_vec()
    }

    fn vcpu_count(&self) -> u64 {
        self.vcpu_count
    }

    fn fdt_compatibility(&self) -> String {
        "arm,gic-v3".to_string()
    }

    fn fdt_maint_irq(&self) -> u32 {
        ARCH_GIC_V3_MAINT_IRQ
    }

    fn version(&self) -> u32 {
        kvm_bindings::kvm_device_type_KVM_DEV_TYPE_ARM_VGIC_V3
    }

    fn its_properties(&self) -> Option<Vec<u64>> {
        self.its_properties.map(|p| p.to_vec())
    }
}
