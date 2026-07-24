// SPDX-License-Identifier: Apache-2.0
//
// A minimal, hand-rolled VFIO (type1v2) wrapper for libkrun PCIe passthrough
// (aarch64). Reimplements just the subset of rust-vmm's `vfio-ioctls` needed to
// open a physical PCI device, read/write its config space, size its BARs and
// mmap its regions — using `vfio-bindings` (raw FFI structs/constants) +
// `vmm-sys-util` ioctl helpers. The vfio-user / iommufd / migration paths are
// intentionally absent. Interrupt (SET_IRQS) and KVM-VFIO binding land with the
// MSI-X / DMA steps.

use std::fs::{File, OpenOptions};
use std::mem;
use std::os::unix::fs::FileExt;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::path::Path;

use vfio_bindings::bindings::vfio::*;
use vmm_sys_util::ioctl::{
    ioctl, ioctl_with_mut_ref, ioctl_with_ptr, ioctl_with_ref, ioctl_with_val,
};
use vmm_sys_util::{ioctl_io_nr, ioctl_ioc_nr};

// VFIO uses the pure `_IO(type, nr)` form (no size) for all its request codes.
ioctl_io_nr!(VFIO_GET_API_VERSION, VFIO_TYPE.into(), VFIO_BASE);
ioctl_io_nr!(VFIO_CHECK_EXTENSION, VFIO_TYPE.into(), VFIO_BASE + 1);
ioctl_io_nr!(VFIO_SET_IOMMU, VFIO_TYPE.into(), VFIO_BASE + 2);
ioctl_io_nr!(VFIO_GROUP_GET_STATUS, VFIO_TYPE.into(), VFIO_BASE + 3);
ioctl_io_nr!(VFIO_GROUP_SET_CONTAINER, VFIO_TYPE.into(), VFIO_BASE + 4);
ioctl_io_nr!(VFIO_GROUP_UNSET_CONTAINER, VFIO_TYPE.into(), VFIO_BASE + 5);
ioctl_io_nr!(VFIO_GROUP_GET_DEVICE_FD, VFIO_TYPE.into(), VFIO_BASE + 6);
ioctl_io_nr!(VFIO_DEVICE_GET_INFO, VFIO_TYPE.into(), VFIO_BASE + 7);
ioctl_io_nr!(VFIO_DEVICE_GET_REGION_INFO, VFIO_TYPE.into(), VFIO_BASE + 8);
ioctl_io_nr!(VFIO_DEVICE_SET_IRQS, VFIO_TYPE.into(), VFIO_BASE + 10);
ioctl_io_nr!(VFIO_IOMMU_MAP_DMA, VFIO_TYPE.into(), VFIO_BASE + 13);
ioctl_io_nr!(VFIO_IOMMU_UNMAP_DMA, VFIO_TYPE.into(), VFIO_BASE + 14);

#[derive(Debug, thiserror::Error)]
pub enum VfioError {
    #[error("failed to open {0}: {1}")]
    OpenFile(String, std::io::Error),
    #[error("unexpected VFIO API version {0} (expected {1})")]
    ApiVersion(i32, u32),
    #[error("VFIO type1v2 IOMMU extension not supported")]
    ExtensionNotSupported,
    #[error("IOMMU group {0} is not viable (not all devices bound to vfio-pci)")]
    GroupNotViable(u32),
    #[error("device is not a PCI device (flags {0:#x})")]
    NotPciDevice(u32),
    #[error("ioctl {0} failed: {1}")]
    Ioctl(&'static str, std::io::Error),
    #[error("failed to read the iommu_group of {0}")]
    IommuGroup(String),
    #[error("region {0} access out of bounds")]
    RegionBounds(u32),
    #[error("region {0} is not mmappable")]
    RegionNotMmappable(u32),
    #[error("mmap of region {0} failed: {1}")]
    Mmap(u32, std::io::Error),
}

fn last_os_error(what: &'static str) -> VfioError {
    VfioError::Ioctl(what, std::io::Error::last_os_error())
}

/// A VFIO container — the IOMMU context that groups attach to.
pub struct VfioContainer {
    container: File,
}

impl VfioContainer {
    pub fn new() -> Result<Self, VfioError> {
        let container = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/vfio/vfio")
            .map_err(|e| VfioError::OpenFile("/dev/vfio/vfio".into(), e))?;

        // SAFETY: VFIO_GET_API_VERSION takes no argument and returns the version.
        let version = unsafe { ioctl(&container, VFIO_GET_API_VERSION()) };
        if version < 0 || version as u32 != VFIO_API_VERSION {
            return Err(VfioError::ApiVersion(version, VFIO_API_VERSION));
        }

        // SAFETY: CHECK_EXTENSION takes the extension id by value; returns 1 if supported.
        let supported =
            unsafe { ioctl_with_val(&container, VFIO_CHECK_EXTENSION(), VFIO_TYPE1v2_IOMMU.into()) };
        if supported != 1 {
            return Err(VfioError::ExtensionNotSupported);
        }

        Ok(VfioContainer { container })
    }

    /// Select the type1v2 IOMMU backend. Legal only after the first group is
    /// attached to this container.
    fn set_iommu(&self) -> Result<(), VfioError> {
        // SAFETY: SET_IOMMU takes the iommu type by value.
        let ret =
            unsafe { ioctl_with_val(&self.container, VFIO_SET_IOMMU(), VFIO_TYPE1v2_IOMMU.into()) };
        if ret < 0 {
            return Err(last_os_error("VFIO_SET_IOMMU"));
        }
        Ok(())
    }

    /// Map a host memory range into the device's IOMMU (identity: iova == gpa).
    /// Used by the DMA step; unused for pure enumeration.
    #[allow(dead_code)]
    pub fn map_dma(&self, vaddr: u64, iova: u64, size: u64) -> Result<(), VfioError> {
        let dma_map = vfio_iommu_type1_dma_map {
            argsz: mem::size_of::<vfio_iommu_type1_dma_map>() as u32,
            flags: VFIO_DMA_MAP_FLAG_READ | VFIO_DMA_MAP_FLAG_WRITE,
            vaddr,
            iova,
            size,
        };
        // SAFETY: MAP_DMA reads the struct by pointer.
        let ret = unsafe { ioctl_with_ref(&self.container, VFIO_IOMMU_MAP_DMA(), &dma_map) };
        if ret != 0 {
            return Err(last_os_error("VFIO_IOMMU_MAP_DMA"));
        }
        Ok(())
    }

    #[allow(dead_code)]
    pub fn unmap_dma(&self, iova: u64, size: u64) -> Result<(), VfioError> {
        let mut dma_unmap = vfio_iommu_type1_dma_unmap {
            argsz: mem::size_of::<vfio_iommu_type1_dma_unmap>() as u32,
            flags: 0,
            iova,
            size,
            ..Default::default()
        };
        // SAFETY: UNMAP_DMA reads/writes the struct by pointer.
        let ret =
            unsafe { ioctl_with_mut_ref(&self.container, VFIO_IOMMU_UNMAP_DMA(), &mut dma_unmap) };
        if ret != 0 {
            return Err(last_os_error("VFIO_IOMMU_UNMAP_DMA"));
        }
        Ok(())
    }
}

impl AsRawFd for VfioContainer {
    fn as_raw_fd(&self) -> RawFd {
        self.container.as_raw_fd()
    }
}

/// A VFIO group — the IOMMU-group granularity of assignment.
struct VfioGroup {
    group: File,
}

impl VfioGroup {
    fn new(id: u32) -> Result<Self, VfioError> {
        let path = format!("/dev/vfio/{id}");
        let group = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|e| VfioError::OpenFile(path, e))?;

        let mut status = vfio_group_status {
            argsz: mem::size_of::<vfio_group_status>() as u32,
            flags: 0,
        };
        // SAFETY: GET_STATUS writes the struct by pointer.
        let ret = unsafe { ioctl_with_mut_ref(&group, VFIO_GROUP_GET_STATUS(), &mut status) };
        if ret < 0 {
            return Err(last_os_error("VFIO_GROUP_GET_STATUS"));
        }
        if status.flags & VFIO_GROUP_FLAGS_VIABLE == 0 {
            return Err(VfioError::GroupNotViable(id));
        }

        Ok(VfioGroup { group })
    }

    /// Bind this group to a container (pass the container fd by pointer).
    fn set_container(&self, container: &VfioContainer) -> Result<(), VfioError> {
        let container_fd: RawFd = container.as_raw_fd();
        // SAFETY: SET_CONTAINER reads a pointer to the i32 container fd.
        let ret =
            unsafe { ioctl_with_ref(&self.group, VFIO_GROUP_SET_CONTAINER(), &container_fd) };
        if ret < 0 {
            return Err(last_os_error("VFIO_GROUP_SET_CONTAINER"));
        }
        Ok(())
    }

    /// Get the device fd for a device name (BDF, e.g. "0002:01:00.0").
    fn get_device_fd(&self, name: &str) -> Result<File, VfioError> {
        let cname = std::ffi::CString::new(name).unwrap();
        // SAFETY: GET_DEVICE_FD reads the NUL-terminated name pointer, returns a new fd.
        let fd = unsafe { ioctl_with_ptr(&self.group, VFIO_GROUP_GET_DEVICE_FD(), cname.as_ptr()) };
        if fd < 0 {
            return Err(last_os_error("VFIO_GROUP_GET_DEVICE_FD"));
        }
        // SAFETY: fd is a fresh, owned file descriptor returned by the kernel.
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

/// A single MMIO/config region of a VFIO device.
#[derive(Clone, Copy, Debug)]
pub struct VfioRegion {
    pub index: u32,
    pub flags: u32,
    pub size: u64,
    pub offset: u64,
}

impl VfioRegion {
    pub fn is_mmappable(&self) -> bool {
        self.flags & VFIO_REGION_INFO_FLAG_MMAP != 0
    }
}

/// A passed-through VFIO PCI device: config + BAR regions, ready for
/// pread/pwrite config access and BAR mmap.
pub struct VfioDevice {
    device: File,
    // Container and group are kept alive for the lifetime of the device.
    container: VfioContainer,
    group: VfioGroup,
    regions: Vec<VfioRegion>,
    pub num_irqs: u32,
}

impl VfioDevice {
    /// Open the PCI device at sysfs BDF `name` (e.g. "0002:01:00.0").
    pub fn new(name: &str) -> Result<Self, VfioError> {
        let group_id = Self::iommu_group_id(name)?;

        let container = VfioContainer::new()?;
        let group = VfioGroup::new(group_id)?;
        // Order is load-bearing: attach group to container, THEN set the IOMMU.
        group.set_container(&container)?;
        container.set_iommu()?;

        let device = group.get_device_fd(name)?;

        let mut dev_info = vfio_device_info {
            argsz: mem::size_of::<vfio_device_info>() as u32,
            flags: 0,
            num_regions: 0,
            num_irqs: 0,
            cap_offset: 0,
            pad: 0,
        };
        // SAFETY: GET_INFO writes the struct by pointer.
        let ret = unsafe { ioctl_with_mut_ref(&device, VFIO_DEVICE_GET_INFO(), &mut dev_info) };
        if ret < 0 {
            return Err(last_os_error("VFIO_DEVICE_GET_INFO"));
        }
        if dev_info.flags & VFIO_DEVICE_FLAGS_PCI == 0 {
            return Err(VfioError::NotPciDevice(dev_info.flags));
        }

        // Enumerate the standard PCI regions (config + BAR0..BAR5).
        let mut regions = Vec::new();
        for index in 0..dev_info.num_regions.min(VFIO_PCI_CONFIG_REGION_INDEX + 1) {
            let mut reg = vfio_region_info {
                argsz: mem::size_of::<vfio_region_info>() as u32,
                flags: 0,
                index,
                cap_offset: 0,
                size: 0,
                offset: 0,
            };
            // SAFETY: GET_REGION_INFO writes the struct by pointer.
            let ret =
                unsafe { ioctl_with_mut_ref(&device, VFIO_DEVICE_GET_REGION_INFO(), &mut reg) };
            if ret < 0 {
                // Some region indices legitimately EINVAL (e.g. VGA on non-VGA); record empty.
                regions.push(VfioRegion {
                    index,
                    flags: 0,
                    size: 0,
                    offset: 0,
                });
                continue;
            }
            regions.push(VfioRegion {
                index,
                flags: reg.flags,
                size: reg.size,
                offset: reg.offset,
            });
        }

        Ok(VfioDevice {
            device,
            container,
            group,
            regions,
            num_irqs: dev_info.num_irqs,
        })
    }

    /// The VFIO container (for IOMMU DMA mapping of guest RAM).
    pub fn container(&self) -> &VfioContainer {
        &self.container
    }

    /// The raw fd of the VFIO group (for binding to KVM via KVM_DEV_VFIO).
    pub fn group_as_raw_fd(&self) -> RawFd {
        self.group.group.as_raw_fd()
    }

    fn iommu_group_id(name: &str) -> Result<u32, VfioError> {
        let link = format!("/sys/bus/pci/devices/{name}/iommu_group");
        let target = std::fs::read_link(Path::new(&link))
            .map_err(|_| VfioError::IommuGroup(name.into()))?;
        target
            .file_name()
            .and_then(|s| s.to_str())
            .and_then(|s| s.parse::<u32>().ok())
            .ok_or_else(|| VfioError::IommuGroup(name.into()))
    }

    pub fn region(&self, index: u32) -> Option<&VfioRegion> {
        self.regions.get(index as usize)
    }

    /// Read from a device region at `offset` (positioned read on the device fd).
    pub fn region_read(&self, index: u32, offset: u64, buf: &mut [u8]) -> Result<(), VfioError> {
        let region = self.region(index).ok_or(VfioError::RegionBounds(index))?;
        if offset + buf.len() as u64 > region.size {
            return Err(VfioError::RegionBounds(index));
        }
        self.device
            .read_exact_at(buf, region.offset + offset)
            .map_err(|e| VfioError::Ioctl("pread", e))
    }

    /// Write to a device region at `offset` (positioned write on the device fd).
    pub fn region_write(&self, index: u32, offset: u64, buf: &[u8]) -> Result<(), VfioError> {
        let region = self.region(index).ok_or(VfioError::RegionBounds(index))?;
        if offset + buf.len() as u64 > region.size {
            return Err(VfioError::RegionBounds(index));
        }
        self.device
            .write_all_at(buf, region.offset + offset)
            .map_err(|e| VfioError::Ioctl("pwrite", e))
    }

    /// Read a 32-bit config-space register at byte `offset`.
    pub fn read_config_dword(&self, offset: u64) -> u32 {
        let mut buf = [0u8; 4];
        if self
            .region_read(VFIO_PCI_CONFIG_REGION_INDEX, offset, &mut buf)
            .is_err()
        {
            return 0xffff_ffff;
        }
        u32::from_le_bytes(buf)
    }

    /// Write a 32-bit config-space register at byte `offset`.
    pub fn write_config_dword(&self, offset: u64, value: u32) {
        let _ = self.region_write(VFIO_PCI_CONFIG_REGION_INDEX, offset, &value.to_le_bytes());
    }

    /// Write raw bytes to config space at byte `offset` (guest sub-dword writes).
    pub fn write_config(&self, offset: u64, data: &[u8]) {
        let _ = self.region_write(VFIO_PCI_CONFIG_REGION_INDEX, offset, data);
    }

    /// Read raw bytes from config space at byte `offset`.
    pub fn read_config(&self, offset: u64, data: &mut [u8]) {
        let _ = self.region_read(VFIO_PCI_CONFIG_REGION_INDEX, offset, data);
    }

    /// mmap a region over the device fd. Returns (host_addr, len). Used for BAR
    /// pass-through (step 6); the caller registers a KVM memslot at the guest addr.
    #[allow(dead_code)]
    pub fn mmap_region(&self, index: u32) -> Result<(*mut libc::c_void, usize), VfioError> {
        let region = self.region(index).ok_or(VfioError::RegionBounds(index))?;
        if !region.is_mmappable() {
            return Err(VfioError::RegionNotMmappable(index));
        }
        let mut prot = 0;
        if region.flags & VFIO_REGION_INFO_FLAG_READ != 0 {
            prot |= libc::PROT_READ;
        }
        if region.flags & VFIO_REGION_INFO_FLAG_WRITE != 0 {
            prot |= libc::PROT_WRITE;
        }
        // SAFETY: mmap over the device fd at the region's opaque offset.
        let addr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                region.size as usize,
                prot,
                libc::MAP_SHARED,
                self.device.as_raw_fd(),
                region.offset as libc::off_t,
            )
        };
        if addr == libc::MAP_FAILED {
            return Err(VfioError::Mmap(index, std::io::Error::last_os_error()));
        }
        Ok((addr, region.size as usize))
    }

    /// mmap a page-aligned sub-range `[region_offset, region_offset+len)` of a
    /// region over the device fd. Used to map a BAR around a trapped MSI-X table
    /// page (step 9): the table page is left unmapped so guest accesses fall
    /// through to emulation. `region_offset` and `len` must be page-aligned.
    pub fn mmap_region_range(
        &self,
        index: u32,
        region_offset: u64,
        len: u64,
    ) -> Result<(*mut libc::c_void, usize), VfioError> {
        let region = self.region(index).ok_or(VfioError::RegionBounds(index))?;
        if !region.is_mmappable() {
            return Err(VfioError::RegionNotMmappable(index));
        }
        if region_offset + len > region.size {
            return Err(VfioError::RegionBounds(index));
        }
        let mut prot = 0;
        if region.flags & VFIO_REGION_INFO_FLAG_READ != 0 {
            prot |= libc::PROT_READ;
        }
        if region.flags & VFIO_REGION_INFO_FLAG_WRITE != 0 {
            prot |= libc::PROT_WRITE;
        }
        // SAFETY: mmap over the device fd at the region's opaque offset plus the
        // requested sub-range offset.
        let addr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len as usize,
                prot,
                libc::MAP_SHARED,
                self.device.as_raw_fd(),
                (region.offset + region_offset) as libc::off_t,
            )
        };
        if addr == libc::MAP_FAILED {
            return Err(VfioError::Mmap(index, std::io::Error::last_os_error()));
        }
        Ok((addr, len as usize))
    }

    /// Arm a set of interrupt vectors by handing the kernel an eventfd per
    /// vector (`VFIO_DEVICE_SET_IRQS`, DATA_EVENTFD | ACTION_TRIGGER). The
    /// device signals `eventfds[i]` when it raises vector `i`. Used for MSI-X
    /// (index = `VFIO_PCI_MSIX_IRQ_INDEX`).
    pub fn set_irqs_eventfds(&self, index: u32, eventfds: &[RawFd]) -> Result<(), VfioError> {
        let header = mem::size_of::<vfio_irq_set>();
        let mut buf = vec![0u8; header + eventfds.len() * mem::size_of::<i32>()];

        // SAFETY: `buf` is at least `header` bytes; we only write the fixed
        // header fields through this pointer.
        unsafe {
            let irq_set = buf.as_mut_ptr() as *mut vfio_irq_set;
            (*irq_set).argsz = buf.len() as u32;
            (*irq_set).flags = VFIO_IRQ_SET_DATA_EVENTFD | VFIO_IRQ_SET_ACTION_TRIGGER;
            (*irq_set).index = index;
            (*irq_set).start = 0;
            (*irq_set).count = eventfds.len() as u32;
        }
        for (i, fd) in eventfds.iter().enumerate() {
            let off = header + i * mem::size_of::<i32>();
            buf[off..off + 4].copy_from_slice(&(*fd as i32).to_ne_bytes());
        }

        // SAFETY: `buf` holds a valid `vfio_irq_set` header followed by `count`
        // little-endian i32 eventfds, as the ioctl expects.
        let ret = unsafe { ioctl_with_ptr(&self.device, VFIO_DEVICE_SET_IRQS(), buf.as_ptr()) };
        if ret < 0 {
            return Err(last_os_error("VFIO_DEVICE_SET_IRQS"));
        }
        Ok(())
    }

    /// Disarm all vectors of an interrupt index (DATA_NONE | ACTION_TRIGGER,
    /// count 0), releasing the eventfd bindings established by `set_irqs_eventfds`.
    pub fn disable_irqs(&self, index: u32) -> Result<(), VfioError> {
        let irq_set = vfio_irq_set {
            argsz: mem::size_of::<vfio_irq_set>() as u32,
            flags: VFIO_IRQ_SET_DATA_NONE | VFIO_IRQ_SET_ACTION_TRIGGER,
            index,
            start: 0,
            count: 0,
            data: Default::default(),
        };
        // SAFETY: DATA_NONE takes no trailing data; the header is read by pointer.
        let ret = unsafe { ioctl_with_ref(&self.device, VFIO_DEVICE_SET_IRQS(), &irq_set) };
        if ret < 0 {
            return Err(last_os_error("VFIO_DEVICE_SET_IRQS"));
        }
        Ok(())
    }
}

impl AsRawFd for VfioDevice {
    fn as_raw_fd(&self) -> RawFd {
        self.device.as_raw_fd()
    }
}
