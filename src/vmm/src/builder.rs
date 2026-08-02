// Copyright 2019 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Enables pre-boot setup, instantiation and booting of a Firecracker VMM.

#[cfg(target_os = "macos")]
use crossbeam_channel::unbounded;
use crossbeam_channel::Sender;
use kernel::cmdline::Cmdline;
#[cfg(target_os = "macos")]
use std::collections::HashMap;
use std::fmt::{Display, Formatter};
#[cfg(any(
    not(target_os = "windows"),
    all(
        any(target_arch = "aarch64", target_arch = "x86_64"),
        target_os = "windows"
    )
))]
use std::fs::File;
use std::io;
#[cfg(not(target_os = "windows"))]
use std::io::IsTerminal;
#[cfg(not(all(target_arch = "x86_64", target_os = "windows")))]
use std::io::Read;
#[cfg(not(target_os = "windows"))]
use std::os::fd::AsRawFd;
#[cfg(not(target_os = "windows"))]
use std::os::fd::{BorrowedFd, FromRawFd};
#[cfg(not(target_os = "windows"))]
use std::path::PathBuf;
use std::sync::atomic::AtomicI32;
#[cfg(all(target_arch = "x86_64", target_os = "windows"))]
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use super::{Error, Vmm};

#[cfg(all(target_arch = "x86_64", not(target_os = "windows")))]
use crate::device_manager::legacy::PortIODeviceManager;
use crate::device_manager::mmio::MMIODeviceManager;
#[cfg(not(target_os = "windows"))]
use crate::resources::{
    DefaultVirtioConsoleConfig, PortConfig, TsiFlags, VirtioConsoleConfigMode, VmResources,
};
#[cfg(target_os = "windows")]
use crate::resources::{PortConfig, VirtioConsoleConfigMode, VmResources};
use crate::vmm_config::external_kernel::ExternalKernel;
#[cfg(not(all(target_arch = "x86_64", target_os = "windows")))]
use crate::vmm_config::external_kernel::KernelFormat;
#[cfg(feature = "net")]
use crate::vmm_config::net::NetBuilder;
#[cfg(target_arch = "x86_64")]
use devices::legacy::Cmos;
#[cfg(target_os = "windows")]
use devices::legacy::IrqChipT;
#[cfg(all(target_os = "linux", target_arch = "riscv64"))]
use devices::legacy::KvmAia;
#[cfg(any(
    not(target_os = "windows"),
    all(
        any(target_arch = "aarch64", target_arch = "x86_64"),
        target_os = "windows"
    )
))]
use devices::legacy::Serial;
#[cfg(target_os = "macos")]
use devices::legacy::VcpuList;
#[cfg(target_os = "macos")]
use devices::legacy::{GicV3, HvfGicV3};
#[cfg(all(target_arch = "x86_64", not(target_os = "windows")))]
use devices::legacy::{IoApic, IrqChipT, KvmIoapic};
use devices::legacy::{IrqChip, IrqChipDevice};
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
use devices::legacy::{KvmGicV2, KvmGicV3};
#[cfg(target_os = "windows")]
use devices::virtio::{port_io, PortDescription};
#[cfg(not(target_os = "windows"))]
use devices::virtio::{port_io, PortDescription, Vsock};
use devices::virtio::{MmioTransport, VirtioDevice};

#[cfg(feature = "tee")]
use kbs_types::Tee;

use crate::device_manager;
#[cfg(unix)]
use crate::exit_signal::register_exit_signal_handlers;
#[cfg(target_os = "linux")]
use crate::signal_handler::register_sigint_handler;
#[cfg(target_os = "linux")]
use crate::signal_handler::register_sigwinch_handler;
#[cfg(unix)]
use crate::terminal::{term_restore_mode, term_set_raw_mode};
#[cfg(feature = "blk")]
use crate::vmm_config::block::BlockBuilder;
#[cfg(not(any(feature = "tee", feature = "aws-nitro")))]
use crate::vmm_config::fs::CustomFsDeviceConfig;
#[cfg(not(feature = "tee"))]
use crate::vmm_config::fs::FsDeviceConfig;
use crate::vmm_config::kernel_cmdline::DEFAULT_KERNEL_CMDLINE;
#[cfg(target_os = "linux")]
use crate::vstate::KvmContext;
#[cfg(all(target_os = "linux", feature = "tee"))]
use crate::vstate::MeasuredRegion;
use crate::vstate::{Error as VstateError, Vcpu, VcpuConfig, Vm};
#[cfg(all(target_arch = "aarch64", target_os = "windows"))]
use crate::windows::vstate::{WHP_GICD_BASE, WHP_GICD_SIZE, WHP_GICR_BASE, WHP_GICR_SIZE};
use arch::{ArchMemoryInfo, InitrdConfig};
use device_manager::shm::ShmManager;
#[cfg(feature = "gpu")]
use devices::virtio::display::DisplayInfo;
#[cfg(feature = "gpu")]
use devices::virtio::display::NoopDisplayBackend;
#[cfg(not(feature = "tee"))]
use devices::virtio::{fs::ExportTable, VirtioShmRegion};
#[cfg(not(all(target_arch = "x86_64", target_os = "windows")))]
use flate2::read::GzDecoder;
#[cfg(feature = "gpu")]
use krun_display::DisplayBackend;
#[cfg(feature = "gpu")]
use krun_display::IntoDisplayBackend;
#[cfg(feature = "amd-sev")]
use kvm_bindings::KVM_MAX_CPUID_ENTRIES;
#[cfg(not(target_os = "windows"))]
use libc::{STDERR_FILENO, STDIN_FILENO, STDOUT_FILENO};
#[cfg(not(target_os = "windows"))]
use nix::unistd::isatty;
use polly::event_manager::{Error as EventManagerError, EventManager};
use utils::eventfd::EventFd;
use utils::worker_message::WorkerMessage;
#[cfg(all(
    target_arch = "x86_64",
    not(feature = "efi"),
    not(feature = "tee"),
    not(target_os = "windows")
))]
use vm_memory::mmap::MmapRegion;
#[cfg(not(feature = "tee"))]
use vm_memory::Address;
use vm_memory::Bytes;
use vm_memory::GuestMemoryBackend;
#[cfg(all(
    target_arch = "x86_64",
    not(feature = "tee"),
    not(target_os = "windows")
))]
use vm_memory::GuestRegionMmap;
use vm_memory::{GuestAddress, GuestMemoryMmap};
#[cfg(target_os = "windows")]
use windows_sys::Win32::System::Hypervisor::{WHvRequestInterrupt, WHV_PARTITION_HANDLE};

#[cfg(target_arch = "aarch64")]
#[allow(dead_code)]
static EDK2_BINARY: &[u8] = include_bytes!("../KRUN_EFI.silent.fd");

/// Errors associated with starting the instance.
#[derive(Debug)]
pub enum StartMicrovmError {
    /// Unable to attach block device to Vmm.
    AttachBlockDevice(io::Error),
    #[cfg(target_os = "macos")]
    /// Failed to create HVF in-kernel IrqChip.
    CreateHvfIrqChip(hvf::Error),
    #[cfg(target_os = "linux")]
    /// Failed to create KVM in-kernel IrqChip.
    CreateKvmIrqChip(kvm_ioctls::Error),
    /// Failed to create a `RateLimiter` object.
    CreateRateLimiter(io::Error),
    /// Cannot open the file containing the kernel code.
    #[cfg(not(target_os = "windows"))]
    ElfOpenKernel(io::Error),
    /// Cannot load the kernel into the VM.
    #[cfg(not(target_os = "windows"))]
    ElfLoadKernel(ElfLoadError),
    /// The firmware can't be loaded into the provided memory address.
    FirmwareInvalidAddress(vm_memory::GuestMemoryError),
    /// Cannot read firmware contents from file.
    FirmwareRead(io::Error),
    /// Memory regions are overlapping or mmap fails.
    GuestMemoryMmapFromRanges(vm_memory::mmap::FromRangesError),
    /// Guest memory collection operation failed.
    GuestMemoryMmap(vm_memory::GuestMemoryError),
    /// Guest memory region construction failed.
    GuestMemoryRegion,
    /// Guest memory region collection operation failed.
    GuestMemoryRegionCollection(vm_memory::GuestRegionCollectionError),
    /// The BZIP2 decoder couldn't decompress the kernel.
    ImageBz2Decoder(io::Error),
    /// Cannot find compressed kernel in file.
    ImageBz2Invalid,
    /// Cannot load the kernel from the uncompressed ELF data.
    #[cfg(not(target_os = "windows"))]
    ImageBz2LoadKernel(ElfLoadError),
    /// Cannot open the file containing the kernel code.
    ImageBz2OpenKernel(io::Error),
    /// The GZIP decoder couldn't decompress the kernel.
    ImageGzDecoder(io::Error),
    /// Cannot find compressed kernel in file.
    ImageGzInvalid,
    /// Cannot load the kernel from the uncompressed ELF data.
    #[cfg(not(target_os = "windows"))]
    ImageGzLoadKernel(ElfLoadError),
    /// Cannot open the file containing the kernel code.
    ImageGzOpenKernel(io::Error),
    /// The ZSTD decoder couldn't decompress the kernel.
    ImageZstdDecoder(io::Error),
    /// Cannot find compressed kernel in file.
    ImageZstdInvalid,
    /// Cannot load the kernel from the uncompressed ELF data.
    #[cfg(not(target_os = "windows"))]
    ImageZstdLoadKernel(ElfLoadError),
    /// Cannot open the file containing the kernel code.
    ImageZstdOpenKernel(io::Error),
    /// Cannot load initrd due to an invalid memory configuration.
    InitrdLoad,
    /// Cannot load initrd due to an invalid image.
    InitrdRead(io::Error),
    /// Internal error encountered while starting a microVM.
    Internal(Error),
    /// Cannot inject the kernel into the guest memory due to a problem with the bundle.
    InvalidKernelBundle(vm_memory::mmap::MmapRegionError),
    /// The kernel command line is invalid.
    KernelCmdline(String),
    /// The kernel doesn't fit into the microVM memory.
    KernelDoesNotFit(u64, usize),
    /// The supplied kernel format is not supported.
    KernelFormatUnsupported,
    /// Cannot load command line string.
    LoadCommandline(kernel::cmdline::Error),
    /// The start command was issued more than once.
    MicroVMAlreadyRunning,
    /// Cannot start the VM because the kernel was not configured.
    MissingKernelConfig,
    /// Cannot start the VM because the size of the guest memory  was not specified.
    MissingMemSizeConfig,
    /// The net device configuration is missing the tap device.
    NetDeviceNotConfigured,
    /// Cannot open the block device backing file.
    OpenBlockDevice(io::Error),
    /// Cannot open console output file.
    OpenConsoleFile(io::Error),
    /// Cannot open console named pipe.
    OpenConsolePipe(io::Error),
    /// The GZIP decoder couldn't decompress the kernel.
    PeGzDecoder(io::Error),
    /// Cannot open the file containing the kernel code.
    PeGzOpenKernel(io::Error),
    /// Cannot find compressed kernel in file.
    PeGzInvalid,
    /// Cannot open the file containing the kernel code.
    RawOpenKernel(io::Error),
    /// Cannot initialize a MMIO Balloon device or add a device to the MMIO Bus.
    RegisterBalloonDevice(device_manager::mmio::Error),
    /// Cannot initialize a MMIO Block Device or add a device to the MMIO Bus.
    RegisterBlockDevice(device_manager::mmio::Error),
    /// Cannot register an EventHandler.
    RegisterEvent(EventManagerError),
    /// Cannot initialize a MMIO Fs Device or add ad device to the MMIO Bus.
    RegisterFsDevice(device_manager::mmio::Error),
    // Cannot initialize a MMIO Fs Device or add ad device to the MMIO Bus.
    RegisterConsoleDevice(device_manager::mmio::Error),
    /// Cannot register SIGWINCH event file descriptor.
    #[cfg(target_os = "linux")]
    RegisterFsSigwinch(kvm_ioctls::Error),
    /// Cannot initialize a MMIO Gpu device or add a device to the MMIO Bus.
    RegisterGpuDevice(device_manager::mmio::Error),
    /// Cannot initialize a MMIO Input device or add a device to the MMIO Bus.
    RegisterInputDevice(device_manager::mmio::Error),
    /// Cannot initialize a MMIO Network Device or add a device to the MMIO Bus.
    RegisterNetDevice(device_manager::mmio::Error),
    /// Cannot initialize a MMIO microsandbox metrics device or add it to the MMIO Bus.
    RegisterMsbMetricsDevice(device_manager::mmio::Error),
    /// Cannot initialize a MMIO Rng device or add a device to the MMIO Bus.
    RegisterRngDevice(device_manager::mmio::Error),
    /// Cannot initialize a MMIO Snd device or add a device to the MMIO Bus.
    RegisterSndDevice(device_manager::mmio::Error),
    /// Cannot initialize a MMIO Vsock Device or add a device to the MMIO Bus.
    RegisterVsockDevice(device_manager::mmio::Error),
    /// Cannot attest the VM in the Secure Virtualization context.
    SecureVirtAttest(VstateError),
    /// Cannot initialize the Secure Virtualization backend.
    SecureVirtPrepare(VstateError),
    /// Error configuring an SHM region.
    ShmConfig(device_manager::shm::Error),
    /// Error creating an SHM region.
    ShmCreate(device_manager::shm::Error),
    /// Error obtaining the host address of an SHM region.
    ShmHostAddr(vm_memory::GuestMemoryError),
    /// The TEE specified is not supported.
    InvalidTee,
}

#[cfg(not(target_os = "windows"))]
#[derive(Debug)]
pub enum ElfLoadError {
    TruncatedHeader,
    InvalidMagic,
    UnsupportedClass(u8),
    UnsupportedEndian(u8),
    InvalidProgramHeaderSize(u16),
    InvalidProgramHeaderOffset,
    ProgramHeaderOutOfBounds,
    SegmentOutOfBounds,
    SegmentAddressOverflow,
    GuestWrite(vm_memory::GuestMemoryError),
}

#[cfg(not(target_os = "windows"))]
impl Display for ElfLoadError {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        use self::ElfLoadError::*;

        match *self {
            TruncatedHeader => write!(f, "ELF header is truncated"),
            InvalidMagic => write!(f, "kernel image is not an ELF image"),
            UnsupportedClass(class) => write!(f, "unsupported ELF class: {class}"),
            UnsupportedEndian(endian) => write!(f, "unsupported ELF endianness: {endian}"),
            InvalidProgramHeaderSize(size) => {
                write!(f, "invalid ELF program-header size: {size}")
            }
            InvalidProgramHeaderOffset => write!(f, "invalid ELF program-header offset"),
            ProgramHeaderOutOfBounds => write!(f, "ELF program-header table is out of bounds"),
            SegmentOutOfBounds => write!(f, "ELF load segment is out of bounds"),
            SegmentAddressOverflow => write!(f, "ELF load segment guest address overflows"),
            GuestWrite(ref err) => write!(f, "failed to write ELF segment to guest memory: {err}"),
        }
    }
}

#[cfg(not(target_os = "windows"))]
impl std::error::Error for ElfLoadError {}

/// It's convenient to automatically convert `kernel::cmdline::Error`s
/// to `StartMicrovmError`s.
impl std::convert::From<kernel::cmdline::Error> for StartMicrovmError {
    fn from(e: kernel::cmdline::Error) -> StartMicrovmError {
        StartMicrovmError::KernelCmdline(e.to_string())
    }
}

impl Display for StartMicrovmError {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        use self::StartMicrovmError::*;
        match *self {
            AttachBlockDevice(ref err) => {
                write!(f, "Unable to attach block device to Vmm. Error: {err}")
            }
            #[cfg(target_os = "macos")]
            CreateHvfIrqChip(ref err) => {
                write!(f, "Cannot create HVF in-kernel IrqChip: {err}")
            }
            #[cfg(target_os = "linux")]
            CreateKvmIrqChip(ref err) => {
                write!(f, "Cannot create KVM in-kernel IrqChip: {err}")
            }
            CreateRateLimiter(ref err) => write!(f, "Cannot create RateLimiter: {err}"),
            #[cfg(not(target_os = "windows"))]
            ElfOpenKernel(ref err) => {
                write!(f, "Cannot open the file containing the kernel code: {err}")
            }
            #[cfg(not(target_os = "windows"))]
            ElfLoadKernel(ref err) => {
                write!(f, "Cannot load the kernel into the VM: {err}")
            }
            FirmwareInvalidAddress(ref err) => {
                write!(
                    f,
                    "The firmware can't be loaded into the guest memory: {err}"
                )
            }
            FirmwareRead(ref err) => {
                write!(f, "Cannot read firmware contents from file: {err}")
            }
            GuestMemoryMmap(ref err) => {
                // Remove imbricated quotes from error message.
                let mut err_msg = format!("{err:?}");
                err_msg = err_msg.replace('\"', "");
                write!(f, "Invalid Memory Configuration: {err_msg}")
            }
            GuestMemoryRegion => {
                write!(
                    f,
                    "Invalid Memory Configuration: invalid guest memory region"
                )
            }
            GuestMemoryRegionCollection(ref err) => {
                let mut err_msg = format!("{err:?}");
                err_msg = err_msg.replace('\"', "");
                write!(f, "Invalid Memory Configuration: {err_msg}")
            }
            GuestMemoryMmapFromRanges(ref err) => {
                // Remove imbricated quotes from error message.
                let mut err_msg = format!("{err:?}");
                err_msg = err_msg.replace('\"', "");
                write!(f, "Invalid Memory Configuration: {err_msg}")
            }
            ImageBz2Decoder(ref err) => {
                write!(f, "The BZIP2 decoder couldn't decompress the kernel. {err}")
            }
            ImageBz2Invalid => {
                write!(f, "Cannot find compressed kernel in file.")
            }
            #[cfg(not(target_os = "windows"))]
            ImageBz2LoadKernel(ref err) => {
                write!(
                    f,
                    "Cannot load the kernel from the uncompressed ELF data. {err}"
                )
            }
            ImageBz2OpenKernel(ref err) => {
                write!(f, "Cannot open the file containing the kernel code. {err}")
            }
            ImageGzDecoder(ref err) => {
                write!(f, "The GZIP decoder couldn't decompress the kernel. {err}")
            }
            ImageGzInvalid => {
                write!(f, "Cannot find compressed kernel in file.")
            }
            #[cfg(not(target_os = "windows"))]
            ImageGzLoadKernel(ref err) => {
                write!(
                    f,
                    "Cannot load the kernel from the uncompressed ELF data. {err}"
                )
            }
            ImageGzOpenKernel(ref err) => {
                write!(f, "Cannot open the file containing the kernel code. {err}")
            }
            ImageZstdDecoder(ref err) => {
                write!(f, "The ZSTD decoder couldn't decompress the kernel. {err}")
            }
            ImageZstdInvalid => {
                write!(f, "Cannot find compressed kernel in file.")
            }
            #[cfg(not(target_os = "windows"))]
            ImageZstdLoadKernel(ref err) => {
                write!(
                    f,
                    "Cannot load the kernel from the uncompressed ELF data. {err}"
                )
            }
            ImageZstdOpenKernel(ref err) => {
                write!(f, "Cannot open the file containing the kernel code. {err}")
            }
            InitrdLoad => write!(
                f,
                "Cannot load initrd due to an invalid memory configuration."
            ),
            InitrdRead(ref err) => write!(f, "Cannot load initrd due to an invalid image: {err}"),
            Internal(ref err) => write!(f, "Internal error while starting microVM: {err:?}"),
            InvalidKernelBundle(ref err) => {
                let mut err_msg = format!("{err}");
                err_msg = err_msg.replace('\"', "");
                write!(
                    f,
                    "Cannot inject the kernel into the guest memory due to a problem with the \
                     bundle. {err_msg}"
                )
            }
            KernelCmdline(ref err) => write!(f, "Invalid kernel command line: {err}"),
            KernelDoesNotFit(load_addr, size) => write!(
                f,
                "The kernel doesn't fit in the microVM memory (load_addr={load_addr}, size={size})"
            ),
            KernelFormatUnsupported => {
                write!(f, "The supplied kernel format is not supported.")
            }
            LoadCommandline(ref err) => {
                let mut err_msg = format!("{err}");
                err_msg = err_msg.replace('\"', "");
                write!(f, "Cannot load command line string. {err_msg}")
            }
            MicroVMAlreadyRunning => write!(f, "Microvm already running."),
            MissingKernelConfig => write!(f, "Cannot start microvm without kernel configuration."),
            MissingMemSizeConfig => {
                write!(f, "Cannot start microvm without guest mem_size config.")
            }
            NetDeviceNotConfigured => {
                write!(f, "The net device configuration is missing the tap device.")
            }
            OpenBlockDevice(ref err) => {
                let mut err_msg = format!("{err:?}");
                err_msg = err_msg.replace('\"', "");

                write!(f, "Cannot open the block device backing file. {err_msg}")
            }
            OpenConsoleFile(ref err) => {
                let mut err_msg = format!("{err:?}");
                err_msg = err_msg.replace('\"', "");

                write!(f, "Cannot open the console output file. {err_msg}")
            }
            OpenConsolePipe(ref err) => {
                let mut err_msg = format!("{err:?}");
                err_msg = err_msg.replace('\"', "");

                write!(f, "Cannot open the console named pipe. {err_msg}")
            }
            PeGzDecoder(ref err) => {
                write!(f, "The GZIP decoder couldn't decompress the kernel. {err}")
            }
            PeGzOpenKernel(ref err) => {
                write!(f, "Cannot open the file containing the kernel code. {err}")
            }
            PeGzInvalid => {
                write!(f, "Cannot find compressed kernel in file.")
            }
            RawOpenKernel(ref err) => {
                write!(f, "Cannot open the file containing the kernel code: {err}")
            }
            RegisterBalloonDevice(ref err) => {
                let mut err_msg = format!("{err}");
                err_msg = err_msg.replace('\"', "");
                write!(
                    f,
                    "Cannot initialize a MMIO Balloon Device or add a device to the MMIO Bus. {err_msg}"
                )
            }
            RegisterBlockDevice(ref err) => {
                let mut err_msg = format!("{err}");
                err_msg = err_msg.replace('\"', "");
                write!(
                    f,
                    "Cannot initialize a MMIO Block Device or add a device to the MMIO Bus. {err_msg}"
                )
            }
            RegisterEvent(ref err) => write!(f, "Cannot register EventHandler. {err:?}"),
            RegisterFsDevice(ref err) => {
                let mut err_msg = format!("{err}");
                err_msg = err_msg.replace('\"', "");

                write!(
                    f,
                    "Cannot initialize a MMIO Fs Device or add a device to the MMIO Bus. {err_msg}"
                )
            }
            RegisterConsoleDevice(ref err) => {
                let mut err_msg = format!("{err}");
                err_msg = err_msg.replace('\"', "");

                write!(
                    f,
                    "Cannot initialize a MMIO Console Device or add a device to the MMIO Bus. {err_msg}"
                )
            }
            #[cfg(target_os = "linux")]
            RegisterFsSigwinch(ref err) => {
                let mut err_msg = format!("{err}");
                err_msg = err_msg.replace('\"', "");

                write!(
                    f,
                    "Cannot register SIGWINCH file descriptor for Fs Device. {err_msg}"
                )
            }
            RegisterGpuDevice(ref err) => {
                let mut err_msg = format!("{err}");
                err_msg = err_msg.replace('\"', "");
                write!(
                    f,
                    "Cannot initialize a MMIO Gpu Device or add a device to the MMIO Bus. {err_msg}"
                )
            }
            RegisterInputDevice(ref err) => {
                let mut err_msg = format!("{err}");
                err_msg = err_msg.replace('\"', "");
                write!(
                    f,
                    "Cannot initialize a MMIO Input Device or add a device to the MMIO Bus. {err_msg}"
                )
            }
            RegisterNetDevice(ref err) => {
                let mut err_msg = format!("{err}");
                err_msg = err_msg.replace('\"', "");

                write!(
                    f,
                    "Cannot initialize a MMIO Network Device or add a device to the MMIO Bus. {err_msg}"
                )
            }
            RegisterMsbMetricsDevice(ref err) => {
                let mut err_msg = format!("{err}");
                err_msg = err_msg.replace('\"', "");
                write!(
                    f,
                    "Cannot initialize a MMIO microsandbox metrics Device or add a device to the MMIO Bus. {err_msg}"
                )
            }
            RegisterRngDevice(ref err) => {
                let mut err_msg = format!("{err}");
                err_msg = err_msg.replace('\"', "");
                write!(
                    f,
                    "Cannot initialize a MMIO Rng Device or add a device to the MMIO Bus. {err_msg}"
                )
            }
            RegisterSndDevice(ref err) => {
                let mut err_msg = format!("{err}");
                err_msg = err_msg.replace('\"', "");
                write!(
                    f,
                    "Cannot initialize a MMIO Snd Device or add a device to the MMIO Bus. {err_msg}"
                )
            }
            RegisterVsockDevice(ref err) => {
                let mut err_msg = format!("{err}");
                err_msg = err_msg.replace('\"', "");

                write!(
                    f,
                    "Cannot initialize a MMIO Vsock Device or add a device to the MMIO Bus. {err_msg}"
                )
            }
            SecureVirtAttest(ref err) => {
                let mut err_msg = format!("{err}");
                err_msg = err_msg.replace('\"', "");

                write!(
                    f,
                    "Cannot attest the VM in the Secure Virtualization context. {err_msg}"
                )
            }
            SecureVirtPrepare(ref err) => {
                let mut err_msg = format!("{err}");
                err_msg = err_msg.replace('\"', "");

                write!(
                    f,
                    "Cannot initialize the Secure Virtualization backend. {err_msg}"
                )
            }
            ShmHostAddr(ref err) => {
                let mut err_msg = format!("{err:?}");
                err_msg = err_msg.replace('\"', "");

                write!(
                    f,
                    "Error obtaining the host address of an SHM region. {err_msg}"
                )
            }
            ShmConfig(ref err) => {
                let mut err_msg = format!("{err:?}");
                err_msg = err_msg.replace('\"', "");

                write!(f, "Error while configuring an SHM region. {err_msg}")
            }
            ShmCreate(ref err) => {
                let mut err_msg = format!("{err:?}");
                err_msg = err_msg.replace('\"', "");

                write!(f, "Error while creating an SHM region. {err_msg}")
            }
            InvalidTee => {
                write!(f, "TEE selected is not currently supported")
            }
        }
    }
}

pub enum Payload {
    #[cfg(all(
        target_arch = "x86_64",
        not(feature = "tee"),
        not(target_os = "windows")
    ))]
    KernelMmap,
    #[cfg(any(
        all(target_arch = "x86_64", target_os = "windows", not(feature = "tee")),
        target_arch = "aarch64",
        target_arch = "riscv64"
    ))]
    KernelCopy,
    ExternalKernel(ExternalKernel),
    #[cfg(test)]
    Empty,
    Firmware,
    #[cfg(feature = "tee")]
    Tee,
}

#[cfg(target_os = "windows")]
struct WhpIrqChip {
    partition_handle: WHV_PARTITION_HANDLE,
    #[cfg(target_arch = "x86_64")]
    ioapic: Mutex<WhpIoApicState>,
    #[cfg(target_arch = "aarch64")]
    vcpu_count: u64,
}

#[cfg(all(target_arch = "x86_64", target_os = "windows"))]
struct WhpIoApicState {
    id: u8,
    ioregsel: u8,
    ioredtbl: [u64; X86_IOAPIC_NUM_PINS],
    version: u8,
}

#[cfg(target_os = "windows")]
impl WhpIrqChip {
    fn new(
        partition_handle: WHV_PARTITION_HANDLE,
        #[cfg_attr(not(target_arch = "aarch64"), allow(unused_variables))] vcpu_count: u64,
    ) -> Self {
        Self {
            partition_handle,
            #[cfg(target_arch = "x86_64")]
            ioapic: Mutex::new(WhpIoApicState::new()),
            #[cfg(target_arch = "aarch64")]
            vcpu_count,
        }
    }
}

#[cfg(all(target_arch = "x86_64", target_os = "windows"))]
impl WhpIoApicState {
    fn new() -> Self {
        Self {
            id: 0,
            ioregsel: 0,
            ioredtbl: [X86_IOAPIC_REDTBL_MASKED; X86_IOAPIC_NUM_PINS],
            version: X86_IOAPIC_VERSION,
        }
    }

    fn read(&mut self, offset: u64, data: &mut [u8]) {
        let value = match offset {
            X86_IOAPIC_REG_SEL => self.ioregsel as u32,
            X86_IOAPIC_WIN => self.read_selected_register(),
            _ => {
                debug!("WHP IOAPIC read from unsupported offset {offset:#x}");
                0
            }
        };

        let value = value.to_le_bytes();
        let len = data.len().min(value.len());
        data[..len].copy_from_slice(&value[..len]);
        if data.len() > len {
            data[len..].fill(0);
        }
    }

    fn write(&mut self, offset: u64, data: &[u8]) {
        if data.len() != 4 {
            debug!(
                "WHP IOAPIC ignoring write with unsupported size {}",
                data.len()
            );
            return;
        }

        let value = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
        match offset {
            X86_IOAPIC_REG_SEL => self.ioregsel = value as u8,
            X86_IOAPIC_WIN => self.write_selected_register(value),
            _ => debug!("WHP IOAPIC write to unsupported offset {offset:#x}"),
        }
    }

    fn read_selected_register(&self) -> u32 {
        match self.ioregsel {
            X86_IOAPIC_ID => (u32::from(self.id)) << X86_IOAPIC_ID_SHIFT,
            X86_IOAPIC_VER => {
                u32::from(self.version)
                    | (((X86_IOAPIC_NUM_PINS as u32) - 1) << X86_IOAPIC_VER_ENTRIES_SHIFT)
            }
            X86_IOAPIC_ARB => (u32::from(self.id)) << X86_IOAPIC_ID_SHIFT,
            _ => {
                let Some((index, high)) = self.selected_redirection_entry() else {
                    debug!("WHP IOAPIC read from invalid selector {:#x}", self.ioregsel);
                    return 0;
                };

                if high {
                    (self.ioredtbl[index] >> 32) as u32
                } else {
                    (self.ioredtbl[index] & 0xffff_ffff) as u32
                }
            }
        }
    }

    fn write_selected_register(&mut self, value: u32) {
        match self.ioregsel {
            X86_IOAPIC_ID => self.id = ((value >> X86_IOAPIC_ID_SHIFT) & X86_IOAPIC_ID_MASK) as u8,
            X86_IOAPIC_VER | X86_IOAPIC_ARB => {}
            _ => {
                let Some((index, high)) = self.selected_redirection_entry() else {
                    debug!("WHP IOAPIC write to invalid selector {:#x}", self.ioregsel);
                    return;
                };

                let ro_bits = self.ioredtbl[index] & X86_IOAPIC_REDTBL_RO_BITS;
                if high {
                    self.ioredtbl[index] &= 0xffff_ffff;
                    self.ioredtbl[index] |= (u64::from(value)) << 32;
                } else {
                    self.ioredtbl[index] &= !0xffff_ffff;
                    self.ioredtbl[index] |= u64::from(value);
                }
                self.ioredtbl[index] &= X86_IOAPIC_REDTBL_RW_BITS;
                self.ioredtbl[index] |= ro_bits;
            }
        }
    }

    fn selected_redirection_entry(&self) -> Option<(usize, bool)> {
        let selector = u64::from(self.ioregsel);
        let reg_index = selector.checked_sub(X86_IOAPIC_REDTBL_BASE)?;
        let index = (reg_index >> 1) as usize;
        if index >= X86_IOAPIC_NUM_PINS {
            return None;
        }

        Some((index, reg_index & 1 != 0))
    }

    fn route_for_irq(&self, irq_line: u32) -> Option<IoApicRoute> {
        let entry = *self.ioredtbl.get(irq_line as usize)?;
        let vector = (entry & X86_IOAPIC_VECTOR_MASK) as u32;
        if vector == 0 {
            return None;
        }
        Some(IoApicRoute {
            vector,
            destination: ((entry >> 56) & 0xff) as u32,
            logical: (entry >> 11) & 0x1 == 1,
            level_triggered: (entry >> 15) & 0x1 == 1,
            masked: entry & X86_IOAPIC_REDTBL_MASKED != 0,
        })
    }
}

/// Interrupt routing the guest programmed into an IOAPIC redirection entry.
#[cfg(all(target_arch = "x86_64", target_os = "windows"))]
struct IoApicRoute {
    vector: u32,
    destination: u32,
    logical: bool,
    level_triggered: bool,
    masked: bool,
}

#[cfg(all(target_arch = "aarch64", target_os = "windows"))]
#[repr(C)]
struct WhvArm64InterruptControl {
    target_partition: u64,
    interrupt_control: u64,
    destination_address: u64,
    requested_vector: u32,
    target_vtl: u8,
    reserved_z0: u8,
    reserved_z1: u16,
}

#[cfg(all(target_arch = "aarch64", target_os = "windows"))]
const WHV_ARM64_INTERRUPT_CONTROL_ASSERTED: u64 = 1 << 34;

#[cfg(all(target_arch = "x86_64", target_os = "windows"))]
#[repr(C, align(8))]
struct WhvX64InterruptControl {
    interrupt_type: u8,
    modes: u8,
    reserved: [u8; 6],
    destination: u32,
    vector: u32,
}

#[cfg(all(target_arch = "x86_64", target_os = "windows"))]
const WHV_X64_INTERRUPT_TYPE_FIXED: u8 = 0;
#[cfg(all(target_arch = "x86_64", target_os = "windows"))]
const WHV_X64_INTERRUPT_DESTINATION_PHYSICAL: u8 = 0;
#[cfg(all(target_arch = "x86_64", target_os = "windows"))]
const WHV_X64_INTERRUPT_DESTINATION_LOGICAL: u8 = 1;
#[cfg(all(target_arch = "x86_64", target_os = "windows"))]
const WHV_X64_INTERRUPT_TRIGGER_EDGE: u8 = 0;
#[cfg(all(target_arch = "x86_64", target_os = "windows"))]
const WHV_X64_INTERRUPT_TRIGGER_LEVEL: u8 = 1;
#[cfg(all(target_arch = "x86_64", target_os = "windows"))]
const X86_LEGACY_IRQ_VECTOR_BASE: u32 = 0x20;
#[cfg(all(target_arch = "x86_64", target_os = "windows"))]
const X86_IOAPIC_BASE: u64 = 0xfec0_0000;
#[cfg(all(target_arch = "x86_64", target_os = "windows"))]
const X86_IOAPIC_SIZE: u64 = 0x1000;
#[cfg(all(target_arch = "x86_64", target_os = "windows"))]
const X86_IOAPIC_NUM_PINS: usize = 64;
#[cfg(all(target_arch = "x86_64", target_os = "windows"))]
const X86_IOAPIC_REG_SEL: u64 = 0x00;
#[cfg(all(target_arch = "x86_64", target_os = "windows"))]
const X86_IOAPIC_WIN: u64 = 0x10;
#[cfg(all(target_arch = "x86_64", target_os = "windows"))]
const X86_IOAPIC_ID: u8 = 0x00;
#[cfg(all(target_arch = "x86_64", target_os = "windows"))]
const X86_IOAPIC_VER: u8 = 0x01;
#[cfg(all(target_arch = "x86_64", target_os = "windows"))]
const X86_IOAPIC_ARB: u8 = 0x02;
#[cfg(all(target_arch = "x86_64", target_os = "windows"))]
const X86_IOAPIC_REDTBL_BASE: u64 = 0x10;
#[cfg(all(target_arch = "x86_64", target_os = "windows"))]
const X86_IOAPIC_VERSION: u8 = 0x20;
#[cfg(all(target_arch = "x86_64", target_os = "windows"))]
const X86_IOAPIC_ID_SHIFT: u32 = 24;
#[cfg(all(target_arch = "x86_64", target_os = "windows"))]
const X86_IOAPIC_ID_MASK: u32 = 0x0f;
#[cfg(all(target_arch = "x86_64", target_os = "windows"))]
const X86_IOAPIC_VER_ENTRIES_SHIFT: u32 = 16;
#[cfg(all(target_arch = "x86_64", target_os = "windows"))]
const X86_IOAPIC_REDTBL_MASKED: u64 = 1 << 16;
#[cfg(all(target_arch = "x86_64", target_os = "windows"))]
const X86_IOAPIC_REDTBL_RO_BITS: u64 = (1 << 12) | (1 << 14);
#[cfg(all(target_arch = "x86_64", target_os = "windows"))]
const X86_IOAPIC_REDTBL_RW_BITS: u64 = !X86_IOAPIC_REDTBL_RO_BITS;
#[cfg(all(target_arch = "x86_64", target_os = "windows"))]
const X86_IOAPIC_VECTOR_MASK: u64 = 0xff;

#[cfg(target_os = "windows")]
impl devices::BusDevice for WhpIrqChip {
    fn read(&mut self, _vcpuid: u64, offset: u64, data: &mut [u8]) {
        #[cfg(target_arch = "x86_64")]
        self.ioapic.lock().unwrap().read(offset, data);

        #[cfg(target_arch = "aarch64")]
        let _ = (offset, data);
    }

    fn write(&mut self, _vcpuid: u64, offset: u64, data: &[u8]) {
        #[cfg(target_arch = "x86_64")]
        self.ioapic.lock().unwrap().write(offset, data);

        #[cfg(target_arch = "aarch64")]
        let _ = (offset, data);
    }
}

#[cfg(all(target_arch = "aarch64", target_os = "windows"))]
impl devices::legacy::gic::GICDevice for WhpIrqChip {
    fn device_properties(&self) -> Vec<u64> {
        vec![
            WHP_GICD_BASE,
            WHP_GICD_SIZE,
            WHP_GICR_BASE,
            WHP_GICR_SIZE * self.vcpu_count,
        ]
    }

    fn vcpu_count(&self) -> u64 {
        self.vcpu_count
    }

    fn fdt_compatibility(&self) -> String {
        "arm,gic-v3".to_string()
    }

    fn fdt_maint_irq(&self) -> u32 {
        9
    }

    fn version(&self) -> u32 {
        3
    }
}

#[cfg(target_os = "windows")]
impl IrqChipT for WhpIrqChip {
    fn get_mmio_addr(&self) -> u64 {
        #[cfg(target_arch = "x86_64")]
        {
            X86_IOAPIC_BASE
        }

        #[cfg(target_arch = "aarch64")]
        0
    }

    fn get_mmio_size(&self) -> u64 {
        #[cfg(target_arch = "x86_64")]
        {
            X86_IOAPIC_SIZE
        }

        #[cfg(target_arch = "aarch64")]
        0
    }

    fn set_irq(
        &self,
        irq_line: Option<u32>,
        _interrupt_evt: Option<&EventFd>,
    ) -> std::result::Result<(), devices::Error> {
        #[cfg(target_arch = "aarch64")]
        {
            let Some(irq_line) = irq_line else {
                return Err(devices::Error::IoError(io::Error::new(
                    io::ErrorKind::NotFound,
                    "WHP interrupt line was not set",
                )));
            };

            let interrupt = WhvArm64InterruptControl {
                target_partition: 0,
                interrupt_control: WHV_ARM64_INTERRUPT_CONTROL_ASSERTED,
                destination_address: 0,
                requested_vector: irq_line,
                target_vtl: 0,
                reserved_z0: 0,
                reserved_z1: 0,
            };
            let hresult = unsafe {
                WHvRequestInterrupt(
                    self.partition_handle,
                    &interrupt as *const WhvArm64InterruptControl as *const _,
                    std::mem::size_of::<WhvArm64InterruptControl>() as u32,
                )
            };
            if hresult < 0 {
                return Err(devices::Error::IoError(io::Error::other(format!(
                    "WHvRequestInterrupt failed with HRESULT {hresult:#x}"
                ))));
            }
        }

        #[cfg(target_arch = "x86_64")]
        {
            let Some(irq_line) = irq_line else {
                return Err(devices::Error::IoError(io::Error::new(
                    io::ErrorKind::NotFound,
                    "WHP interrupt line was not set",
                )));
            };

            let route = self.ioapic.lock().unwrap().route_for_irq(irq_line);
            let (vector, destination, logical, level_triggered) = match route {
                // The guest masked the pin: architecturally the interrupt
                // is simply not delivered.
                Some(route) if route.masked => return Ok(()),
                Some(route) => (
                    route.vector,
                    route.destination,
                    route.logical,
                    route.level_triggered,
                ),
                // Pin not programmed yet: legacy identity route to the BSP.
                None => (X86_LEGACY_IRQ_VECTOR_BASE + irq_line, 0, false, false),
            };
            let destination_mode = if logical {
                WHV_X64_INTERRUPT_DESTINATION_LOGICAL
            } else {
                WHV_X64_INTERRUPT_DESTINATION_PHYSICAL
            };
            let trigger_mode = if level_triggered {
                WHV_X64_INTERRUPT_TRIGGER_LEVEL
            } else {
                WHV_X64_INTERRUPT_TRIGGER_EDGE
            };
            let interrupt = WhvX64InterruptControl {
                interrupt_type: WHV_X64_INTERRUPT_TYPE_FIXED,
                modes: destination_mode | (trigger_mode << 4),
                reserved: [0; 6],
                destination,
                vector,
            };
            let hresult = unsafe {
                WHvRequestInterrupt(
                    self.partition_handle,
                    &interrupt as *const WhvX64InterruptControl as *const _,
                    std::mem::size_of::<WhvX64InterruptControl>() as u32,
                )
            };
            if hresult < 0 {
                return Err(devices::Error::IoError(io::Error::new(
                    io::ErrorKind::Other,
                    format!("WHvRequestInterrupt failed with HRESULT {hresult:#x}"),
                )));
            }
        }

        Ok(())
    }
}

fn choose_payload(vm_resources: &VmResources) -> Result<Payload, StartMicrovmError> {
    if let Some(_kernel_bundle) = &vm_resources.kernel_bundle {
        #[cfg(feature = "tee")]
        if vm_resources.qboot_bundle.is_none() || vm_resources.initrd_bundle.is_none() {
            return Err(StartMicrovmError::MissingKernelConfig);
        }

        #[cfg(feature = "tee")]
        return Ok(Payload::Tee);

        #[cfg(all(target_os = "linux", target_arch = "x86_64", not(feature = "tee")))]
        return Ok(Payload::KernelMmap);

        #[cfg(all(target_os = "windows", target_arch = "x86_64", not(feature = "tee")))]
        return Ok(Payload::KernelCopy);

        #[cfg(any(target_arch = "aarch64", target_arch = "riscv64"))]
        return Ok(Payload::KernelCopy);
    } else if let Some(external_kernel) = vm_resources.external_kernel() {
        Ok(Payload::ExternalKernel(external_kernel.clone()))
    } else if cfg!(feature = "efi") || vm_resources.firmware_config.is_some() {
        Ok(Payload::Firmware)
    } else {
        Err(StartMicrovmError::MissingKernelConfig)
    }
}

/// Builds and starts a microVM based on the current Firecracker VmResources configuration.
///
/// This is the default build recipe, one could build other microVM flavors by using the
/// independent functions in this module instead of calling this recipe.
///
/// An `Arc` reference of the built `Vmm` is also plugged in the `EventManager`, while another
/// is returned.
pub fn build_microvm(
    vm_resources: &mut super::resources::VmResources,
    event_manager: &mut EventManager,
    _shutdown_efd: Option<EventFd>,
    _sender: Sender<WorkerMessage>,
    exit_evt: EventFd,
    exit_code: Arc<AtomicI32>,
) -> std::result::Result<Arc<Mutex<Vmm>>, StartMicrovmError> {
    let mut trace = BootTrace::new("vmm");
    trace.mark("build_microvm.start");

    let payload = choose_payload(vm_resources)?;

    let (guest_memory, arch_memory_info, mut _shm_manager, payload_config) = create_guest_memory(
        vm_resources
            .vm_config()
            .mem_size_mib
            .ok_or(StartMicrovmError::MissingMemSizeConfig)?,
        vm_resources,
        &payload,
    )?;
    trace.mark("guest_memory.ready");

    #[allow(unused_mut)]
    let mut vcpu_config = vm_resources.vcpu_config();

    // Clone the command-line so that a failed boot doesn't pollute the original.
    #[allow(unused_mut)]
    let mut kernel_cmdline = Cmdline::new(arch::CMDLINE_MAX_SIZE);
    if let Some(cmdline) = payload_config.kernel_cmdline {
        kernel_cmdline.insert_str(cmdline.as_str())?;
    } else if let Some(cmdline) = &vm_resources.kernel_cmdline.prolog {
        kernel_cmdline.insert_str(cmdline)?;
    } else {
        kernel_cmdline.insert_str(DEFAULT_KERNEL_CMDLINE)?;
    }

    // The krun_env segment carries caller-controlled guest env; a non-cmdline-safe byte in it
    // (e.g. a tab in an OCI image env value) must surface as a StartMicrovmError, not a panic —
    // the API layer validates per variable, this is the backstop.
    if let Some(cmdline) = &vm_resources.kernel_cmdline.krun_env {
        kernel_cmdline.insert_str(cmdline.as_str())?;
    }

    // When extra CPU capacity is reserved, every possible vCPU is described to the guest
    // (MPTABLE/FDT), so cap the number brought online at boot to the requested count. The
    // remaining CPUs stay offline until the guest's msb-cpu driver onlines them.
    if vcpu_config.max_vcpu_count > vcpu_config.vcpu_count {
        kernel_cmdline.insert_str(format!(" maxcpus={}", vcpu_config.vcpu_count))?;
    }

    // Hotplugged virtio-mem blocks must come online automatically: without this,
    // guests whose kernel lacks MEMORY_HOTPLUG_DEFAULT_ONLINE (and which run no
    // udev onlining rule) accept the blocks but leave them offline, so a live
    // grow silently adds no usable memory. The movable zone keeps later unplug
    // reliable.
    #[cfg(not(feature = "tee"))]
    if vm_resources.mem_device.is_some() {
        kernel_cmdline.insert_str(" memhp_default_state=online_movable")?;
    }
    trace.mark("kernel_cmdline.ready");

    if let Some(kernel_console) = &vm_resources.kernel_console {
        let cmdline = kernel_cmdline.as_str();
        let console_start_idx = cmdline.find("console=").unwrap();
        let console_end_idx = cmdline
            .get(console_start_idx..)
            .and_then(|s| s.find(" ").map(|i| i + console_start_idx));

        let cmdline = cmdline.replace(
            &cmdline[console_start_idx..console_end_idx.unwrap()],
            format!("console={kernel_console}").as_str(),
        );
        kernel_cmdline = Cmdline::new(arch::CMDLINE_MAX_SIZE);
        kernel_cmdline.insert_str(cmdline)?;
    }

    // The WHP partition is sized to the full possible topology: reserved
    // CPU capacity creates every vCPU up front (parked in the startup
    // router), so the partition's processor count must cover them all.
    #[cfg(target_os = "windows")]
    let setup_vcpu_count = vcpu_config.max_vcpu_count.max(vcpu_config.vcpu_count);
    #[cfg(all(not(target_os = "windows"), not(feature = "tee")))]
    let setup_vcpu_count = vcpu_config.vcpu_count;
    #[cfg(not(feature = "tee"))]
    #[allow(unused_mut)]
    let mut vm = setup_vm(&guest_memory, vm_resources.nested_enabled, setup_vcpu_count)?;
    trace.mark("vm.ready");

    #[cfg(feature = "tee")]
    let (_kvm, vm) = {
        let kvm = KvmContext::new()
            .map_err(Error::KvmContext)
            .map_err(StartMicrovmError::Internal)?;
        let vm = setup_vm(
            &kvm,
            &guest_memory,
            vm_resources,
            #[cfg(feature = "tdx")]
            _sender.clone(),
        )?;
        (kvm, vm)
    };

    #[cfg(feature = "tee")]
    let tee = vm_resources.tee_config().tee;

    #[cfg(feature = "amd-sev")]
    let snp_launcher = match tee {
        Tee::Snp => Some(
            vm.snp_secure_virt_prepare(&guest_memory)
                .map_err(StartMicrovmError::SecureVirtPrepare)?,
        ),
        _ => None,
    };

    #[cfg(feature = "tdx")]
    let mut tdx_launcher = match tee {
        Tee::Tdx => vm
            .tdx_secure_virt_prepare()
            .map_err(StartMicrovmError::SecureVirtPrepare)?,
        _ => panic!(),
    };

    #[cfg(all(feature = "tee", not(feature = "tdx")))]
    let measured_regions = {
        println!("Injecting and measuring memory regions. This may take a while.");

        let qboot_size = if let Some(qboot_bundle) = &vm_resources.qboot_bundle {
            qboot_bundle.size
        } else {
            return Err(StartMicrovmError::MissingKernelConfig);
        };
        let (kernel_guest_addr, kernel_size) =
            if let Some(kernel_bundle) = &vm_resources.kernel_bundle {
                (kernel_bundle.guest_addr, kernel_bundle.size)
            } else {
                return Err(StartMicrovmError::MissingKernelConfig);
            };
        let (initrd_addr, initrd_size) = if let Some(initrd_config) = &payload_config.initrd_config
        {
            (initrd_config.address, initrd_config.size)
        } else {
            return Err(StartMicrovmError::MissingKernelConfig);
        };

        vec![
            MeasuredRegion {
                guest_addr: arch::FIRMWARE_START,
                host_addr: guest_memory
                    .get_host_address(GuestAddress(arch::FIRMWARE_START))
                    .unwrap() as u64,
                size: qboot_size,
            },
            MeasuredRegion {
                guest_addr: kernel_guest_addr,
                host_addr: guest_memory
                    .get_host_address(GuestAddress(kernel_guest_addr))
                    .unwrap() as u64,
                size: kernel_size,
            },
            MeasuredRegion {
                guest_addr: initrd_addr.0,
                host_addr: guest_memory.get_host_address(initrd_addr).unwrap() as u64,
                size: initrd_size,
            },
            MeasuredRegion {
                guest_addr: arch::x86_64::layout::ZERO_PAGE_START,
                host_addr: guest_memory
                    .get_host_address(GuestAddress(arch::x86_64::layout::ZERO_PAGE_START))
                    .unwrap() as u64,
                size: 4096,
            },
        ]
    };

    #[cfg(feature = "tdx")]
    let measured_regions = {
        println!("Injecting and measuring memory regions. This may take a while.");
        let qboot_size = if let Some(qboot_bundle) = &vm_resources.qboot_bundle {
            qboot_bundle.size
        } else {
            return Err(StartMicrovmError::MissingKernelConfig);
        };
        let m = vec![
            MeasuredRegion {
                guest_addr: 0,
                host_addr: guest_memory.get_host_address(GuestAddress(0)).unwrap() as u64,
                size: 0x8000_0000,
            },
            MeasuredRegion {
                guest_addr: arch::FIRMWARE_START,
                host_addr: guest_memory
                    .get_host_address(GuestAddress(arch::FIRMWARE_START))
                    .unwrap() as u64,
                size: qboot_size,
            },
        ];

        m
    };

    #[cfg(any(
        not(target_os = "windows"),
        all(
            any(target_arch = "aarch64", target_arch = "x86_64"),
            target_os = "windows"
        )
    ))]
    let mut serial_devices = Vec::new();

    // Create the legacy serial device if we're booting from a firmware
    #[cfg(not(target_os = "windows"))]
    if (cfg!(feature = "efi") || vm_resources.firmware_config.is_some())
        && !vm_resources.disable_implicit_console
    {
        serial_devices.push(setup_serial_device(
            event_manager,
            None,
            None,
            // Uncomment this to get EFI output when debugging EDK2.
            //Some(Box::new(io::stdout())),
        )?);
    };

    // We can't call to `setup_terminal_raw_mode` until `Vmm` is created,
    // so let's keep track of FDs connected to legacy serial devices here
    // and set raw mode on them later.
    #[cfg(not(target_os = "windows"))]
    let mut serial_ttys = Vec::new();

    #[cfg(not(target_os = "windows"))]
    for s in &vm_resources.serial_consoles {
        let input = unsafe { BorrowedFd::borrow_raw(s.input_fd) };
        if input.is_terminal() {
            serial_ttys.push(input);
        }
        let input: Option<Box<dyn devices::legacy::ReadableFd + Send>> = if s.input_fd >= 0 {
            Some(Box::new(unsafe { File::from_raw_fd(s.input_fd) }))
        } else {
            None
        };

        let output: Option<Box<dyn io::Write + Send>> = if s.output_fd >= 0 {
            Some(Box::new(unsafe { File::from_raw_fd(s.output_fd) }))
        } else {
            None
        };

        serial_devices.push(setup_serial_device(event_manager, input, output)?);
    }

    #[cfg(all(
        any(target_arch = "aarch64", target_arch = "x86_64"),
        target_os = "windows"
    ))]
    if !vm_resources.disable_implicit_console {
        let output: Box<dyn io::Write + Send> = if let Some(console_output_path) =
            &vm_resources.console_output
        {
            Box::new(File::create(console_output_path).map_err(StartMicrovmError::OpenConsoleFile)?)
        } else {
            Box::new(io::stdout())
        };
        serial_devices.push(setup_serial_device(event_manager, None, Some(output))?);

        if vm_resources.kernel_console.is_none() {
            if kernel_cmdline.as_str().contains("console=") {
                #[cfg(target_arch = "aarch64")]
                let cmdline = kernel_cmdline
                    .as_str()
                    .replace("console=hvc0", "console=ttyAMA0");
                #[cfg(target_arch = "x86_64")]
                let cmdline = kernel_cmdline
                    .as_str()
                    .replace("console=hvc0", "console=ttyS0,115200n8")
                    .replace("console=ttyAMA0", "console=ttyS0,115200n8");
                kernel_cmdline = Cmdline::new(arch::CMDLINE_MAX_SIZE);
                kernel_cmdline.insert_str(cmdline.as_str())?;
            } else {
                #[cfg(target_arch = "aarch64")]
                kernel_cmdline.insert("console", "ttyAMA0")?;
                #[cfg(target_arch = "x86_64")]
                kernel_cmdline.insert("console", "ttyS0,115200n8").unwrap();
            }
        }
    }

    // Register signal handlers with the pre-created exit EventFd.
    // On Linux eventfd is a single fd (read/write on the same fd).
    // On macOS EventFd is a pipe pair — we need the write end.
    #[cfg(target_os = "linux")]
    let exit_write_fd = exit_evt.as_raw_fd();
    #[cfg(target_os = "macos")]
    let exit_write_fd = exit_evt.get_write_fd();

    #[cfg(unix)]
    register_exit_signal_handlers(exit_write_fd)
        .map_err(Error::EventFd)
        .map_err(StartMicrovmError::Internal)?;

    #[cfg(all(target_arch = "x86_64", not(target_os = "windows")))]
    // Safe to unwrap 'serial_device' as it's always 'Some' on x86_64.
    // x86_64 uses the i8042 reset event as the Vmm exit event.
    let mut pio_device_manager = PortIODeviceManager::new(
        Arc::new(Mutex::new(Cmos::new(
            arch_memory_info.ram_below_gap,
            arch_memory_info.ram_above_gap,
        ))),
        serial_devices,
        exit_evt
            .try_clone()
            .map_err(Error::EventFd)
            .map_err(StartMicrovmError::Internal)?,
    )
    .map_err(Error::CreateLegacyDevice)
    .map_err(StartMicrovmError::Internal)?;

    // Instantiate the MMIO device manager.
    // 'mmio_base' address has to be an address which is protected by the kernel
    // and is architectural specific.
    //
    // x86_64 exposes only IRQ_BASE..=IRQ_MAX (the legacy ISA range) of MMIO interrupt lines with
    // the in-kernel IOAPIC, which a guest with many virtio-mmio devices (each device takes one
    // IRQ) can exhaust. The userspace IOAPIC exposes IRQ_BASE..=IRQ_MAX_SPLIT instead. On KVM that
    // is the split irqchip: enable it automatically when the configured device count would overflow
    // the in-kernel range, leaving light guests on the faster in-kernel chip. On Windows the IOAPIC
    // is always userspace, so its full pin range is used unconditionally.
    #[cfg(all(target_arch = "x86_64", not(target_os = "windows")))]
    if !vm_resources.split_irqchip {
        // Each virtio-fs share and block device takes one IOAPIC pin; the other configured platform
        // devices (balloon, virtio-mem, cpu, rng, metrics, console, net, vsock) take a fixed handful
        // more. A conservative base plus the per-share count errs toward enabling the split irqchip
        // (which costs a worker thread) rather than exhausting IRQs at device-registration time.
        // Light guests stay under the range and keep the in-kernel irqchip.
        const PLATFORM_IRQ_DEVICES: usize = 9;
        let dynamic = vm_resources.fs.len();
        let pool = (arch::IRQ_MAX - arch::IRQ_BASE + 1) as usize;
        if PLATFORM_IRQ_DEVICES + dynamic > pool {
            vm_resources.split_irqchip = true;
        }
    }
    // Use the WhpIrqChip IOAPIC's full pin range. WHP has no in-kernel irqchip, so the guest IOAPIC
    // is emulated in userspace with X86_IOAPIC_NUM_PINS redirection entries; the usable ceiling is
    // pin NUM_PINS-1, not the legacy in-kernel IRQ_MAX. This lifts the virtio MMIO IRQ count from 11
    // (IRQ_BASE..=IRQ_MAX) to NUM_PINS-IRQ_BASE, enough for a guest with several virtiofs mounts.
    #[cfg(all(target_arch = "x86_64", target_os = "windows"))]
    let irq_max = X86_IOAPIC_NUM_PINS as u32 - 1;
    #[cfg(all(target_arch = "x86_64", not(target_os = "windows")))]
    let irq_max = if vm_resources.split_irqchip {
        arch::IRQ_MAX_SPLIT
    } else {
        arch::IRQ_MAX
    };
    #[cfg(not(target_arch = "x86_64"))]
    let irq_max = arch::IRQ_MAX;
    #[allow(unused_mut)]
    let mut mmio_device_manager = MMIODeviceManager::new(
        &mut (arch::MMIO_MEM_START.clone()),
        (arch::IRQ_BASE, irq_max),
    );

    #[cfg(target_os = "macos")]
    let vcpu_list = {
        // Size for the full possible topology so parked vCPUs can register too.
        Arc::new(VcpuList::new(vcpu_config.max_vcpu_count as u64))
    };

    #[allow(unused_mut)]
    let mut vcpus;
    let intc: IrqChip;
    // For x86_64 we need to create the interrupt controller before calling `KVM_CREATE_VCPUS`
    // while on aarch64 we need to do it the other way around.
    #[cfg(all(target_arch = "x86_64", not(target_os = "windows")))]
    {
        // Userspace split irqchip is required for >11 virtio IRQs, since KVM's
        // in-kernel IOAPIC is hardcoded at 24 pins (KVM_IOAPIC_NUM_PINS).
        let ioapic: Box<dyn IrqChipT> = if vm_resources.split_irqchip {
            Box::new(
                IoApic::new(vm.fd(), _sender.clone())
                    .map_err(StartMicrovmError::CreateKvmIrqChip)?,
            )
        } else {
            Box::new(KvmIoapic::new(vm.fd()).map_err(StartMicrovmError::CreateKvmIrqChip)?)
        };
        intc = Arc::new(Mutex::new(IrqChipDevice::new(ioapic)));

        attach_legacy_devices(
            &vm,
            vm_resources.split_irqchip,
            &mut pio_device_manager,
            &mut mmio_device_manager,
            Some(intc.clone()),
        )?;

        let kernel_boot = vm_resources.firmware_config.is_none() && !cfg!(feature = "tee");

        vcpus = create_vcpus_x86_64(
            &vm,
            &vcpu_config,
            &guest_memory,
            payload_config.entry_addr,
            &pio_device_manager.io_bus,
            &exit_evt,
            kernel_boot,
            vm_resources.metrics.clone(),
            #[cfg(feature = "tee")]
            _sender,
        )
        .map_err(StartMicrovmError::Internal)?;
    }

    #[cfg(target_os = "windows")]
    {
        // Size the guest-visible GIC redistributor range to the created topology:
        // with reserved CPU capacity the FDT lists every possible CPU, and the
        // GICv3 driver must find each one's GICR frame inside the advertised
        // range (the HVF GIC sizes by max for the same reason).
        intc = Arc::new(Mutex::new(IrqChipDevice::new(Box::new(WhpIrqChip::new(
            vm.partition_handle(),
            u64::from(setup_vcpu_count),
        )))));
        #[cfg(target_arch = "x86_64")]
        let pio_bus = create_windows_x86_64_pio_bus(
            intc.clone(),
            &serial_devices,
            &exit_evt,
            &arch_memory_info,
        )?;
        #[cfg(target_arch = "x86_64")]
        mmio_device_manager
            .register_mmio_ioapic(intc.clone())
            .map_err(Error::RegisterMMIODevice)
            .map_err(StartMicrovmError::Internal)?;
        vcpus = create_vcpus_windows(
            &vm,
            &vcpu_config,
            &guest_memory,
            &arch_memory_info,
            payload_config.entry_addr,
            &exit_evt,
            vm_resources.metrics.clone(),
            #[cfg(target_arch = "x86_64")]
            pio_bus.as_ref(),
        )
        .map_err(StartMicrovmError::Internal)?;
    }

    #[cfg(all(target_arch = "aarch64", target_os = "windows"))]
    {
        attach_legacy_devices(
            &vm,
            &mut mmio_device_manager,
            &mut kernel_cmdline,
            intc.clone(),
            serial_devices,
        )?;
    }

    #[cfg(feature = "tdx")]
    {
        for vcpu in &vcpus {
            vcpu.tdx_secure_virt_prepare(&mut tdx_launcher);
        }
        vm.tdx_secure_virt_init_vcpus(&mut tdx_launcher).unwrap();
    }

    // On aarch64, the vCPUs need to be created (i.e call KVM_CREATE_VCPU) and configured before
    // setting up the IRQ chip because the `KVM_CREATE_VCPU` ioctl will return error if the IRQCHIP
    // was already initialized.
    // Search for `kvm_arch_vcpu_create` in arch/arm/kvm/arm.c.
    #[cfg(all(target_arch = "aarch64", target_os = "linux"))]
    {
        vcpus = create_vcpus_aarch64(
            &vm,
            &vcpu_config,
            &arch_memory_info,
            payload_config.entry_addr,
            &exit_evt,
            vm_resources.metrics.clone(),
        )
        .map_err(StartMicrovmError::Internal)?;

        intc = {
            // The SoC in some popular boards (namely, the RPi family) doesn't support an
            // architected vGIC, which is required for requesting KVM the instantiation of a
            // GICv3. To relieve the users from having to configure the gic version manually,
            // try first to instantiate a GICv3, and fall back to a GICv2 if it fails.
            // Redistributors must cover the full possible topology, not just online CPUs.
            let vcpu_count = vcpu_config.max_vcpu_count as u64;
            let gic = match KvmGicV3::new(vm.fd(), vcpu_count) {
                Ok(gicv3) => IrqChipDevice::new(Box::new(gicv3)),
                Err(_) => {
                    warn!("KVM GICv3 creation failed, falling back to KVM GICv2");
                    IrqChipDevice::new(Box::new(KvmGicV2::new(vm.fd(), vcpu_count)))
                }
            };
            Arc::new(Mutex::new(gic))
        };

        attach_legacy_devices(
            &vm,
            &mut mmio_device_manager,
            &mut kernel_cmdline,
            intc.clone(),
            serial_devices,
        )?;
    }

    #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
    {
        intc = {
            // If the system supports the in-kernel GIC, use it. Otherwise, fall back to the
            // userspace implementation. Redistributors must cover the full possible
            // topology, not just online CPUs.
            let gic = match HvfGicV3::new(vcpu_config.max_vcpu_count as u64) {
                Ok(hvfgic) => IrqChipDevice::new(Box::new(hvfgic)),
                Err(_) => IrqChipDevice::new(Box::new(GicV3::new(vcpu_list.clone()))),
            };
            Arc::new(Mutex::new(gic))
        };

        vcpus = create_vcpus_aarch64(
            &vm,
            &vcpu_config,
            &arch_memory_info,
            payload_config.entry_addr,
            &exit_evt,
            vcpu_list.clone(),
            vm_resources.nested_enabled,
            vm_resources.metrics.clone(),
        )
        .map_err(StartMicrovmError::Internal)?;

        attach_legacy_devices(
            &vm,
            &mut mmio_device_manager,
            &mut kernel_cmdline,
            intc.clone(),
            serial_devices,
            event_manager,
            _shutdown_efd,
        )?;
    }

    #[cfg(all(not(feature = "tee"), any(target_os = "linux", target_os = "macos")))]
    if let Some(cpu_device) = &vm_resources.cpu_device {
        let enforcement = cpu_device.lock().unwrap().enforcement();
        for vcpu in &mut vcpus {
            vcpu.set_enforcement(enforcement.clone());
        }
    }

    #[cfg(all(target_arch = "riscv64", target_os = "linux"))]
    {
        vcpus = create_vcpus_riscv64(
            &vm,
            &vcpu_config,
            &guest_memory,
            payload_config.entry_addr,
            &exit_evt,
            vm_resources.metrics.clone(),
        )
        .map_err(StartMicrovmError::Internal)?;

        intc = Arc::new(Mutex::new(IrqChipDevice::new(Box::new(
            KvmAia::new(vm.fd(), vm_resources.vm_config().vcpu_count.unwrap() as u32).unwrap(),
        ))));

        attach_legacy_devices(
            &vm,
            &mut mmio_device_manager,
            &mut kernel_cmdline,
            serial_device,
        )?;
    }

    trace.mark("arch_devices.ready");

    // exit_code is pre-created and shared with callers via Vm::exit_code().

    let mut vmm = Vmm {
        guest_memory,
        arch_memory_info,
        kernel_cmdline,
        vcpus_handles: Vec::new(),
        exit_evt,
        exit_observers: Vec::new(),
        exit_code: exit_code.clone(),
        vm,
        mmio_device_manager,
        #[cfg(all(target_arch = "x86_64", not(target_os = "windows")))]
        pio_device_manager,
    };

    // Set raw mode for FDs that are connected to legacy serial devices.
    #[cfg(not(target_os = "windows"))]
    for serial_tty in serial_ttys {
        setup_terminal_raw_mode(&mut vmm, Some(serial_tty), false);
    }

    #[cfg(not(feature = "tee"))]
    if vm_resources.enable_balloon {
        attach_balloon_device(
            &mut vmm,
            event_manager,
            intc.clone(),
            vm_resources.metrics.clone(),
            vm_resources.balloon_stats_interval,
        )?;
        trace.mark("balloon.attached");
    } else {
        trace.mark("balloon.skipped");
    }
    #[cfg(not(feature = "tee"))]
    if let Some(mem_device) = vm_resources.mem_device.clone() {
        attach_mem_device(&mut vmm, event_manager, intc.clone(), mem_device)?;
        trace.mark("mem.attached");
    }
    #[cfg(not(feature = "tee"))]
    if let Some(cpu_device) = vm_resources.cpu_device.clone() {
        attach_cpu_device(&mut vmm, event_manager, intc.clone(), cpu_device)?;
        trace.mark("cpu.attached");
    }
    #[cfg(all(not(feature = "tee"), not(target_os = "windows")))]
    if vm_resources.enable_rng {
        attach_rng_device(&mut vmm, event_manager, intc.clone())?;
        trace.mark("rng.attached");
    } else {
        trace.mark("rng.skipped");
    }
    #[cfg(not(feature = "tee"))]
    if vm_resources.enable_msb_metrics {
        attach_msb_metrics_device(
            &mut vmm,
            event_manager,
            intc.clone(),
            vm_resources.metrics.clone(),
        )?;
        trace.mark("msb_metrics.attached");
    } else {
        trace.mark("msb_metrics.skipped");
    }

    #[cfg(not(target_os = "windows"))]
    let mut console_id = 0;
    #[cfg(not(target_os = "windows"))]
    if !vm_resources.disable_implicit_console {
        attach_console_devices(
            &mut vmm,
            event_manager,
            intc.clone(),
            vm_resources,
            None,
            console_id,
        )?;
        console_id += 1;
        trace.mark("implicit_console.attached");
    } else {
        trace.mark("implicit_console.skipped");
    }

    #[cfg(target_os = "windows")]
    trace.mark("implicit_console.skipped");

    #[cfg(not(target_os = "windows"))]
    for console_cfg in std::mem::take(&mut vm_resources.virtio_consoles) {
        attach_console_devices(
            &mut vmm,
            event_manager,
            intc.clone(),
            vm_resources,
            Some(console_cfg),
            console_id,
        )?;
        console_id += 1;
    }
    #[cfg(target_os = "windows")]
    for (console_id, console_cfg) in std::mem::take(&mut vm_resources.virtio_consoles)
        .into_iter()
        .enumerate()
    {
        attach_console_devices(
            &mut vmm,
            event_manager,
            intc.clone(),
            console_cfg,
            console_id as u32,
        )?;
    }
    trace.mark("virtio_consoles.ready");

    #[cfg(not(feature = "tee"))]
    let export_table: Option<ExportTable> = if cfg!(feature = "gpu") {
        Some(Default::default())
    } else {
        None
    };

    #[cfg(feature = "gpu")]
    if let Some(virgl_flags) = vm_resources.gpu_virgl_flags {
        let display_backend = vm_resources
            .display_backend
            .unwrap_or_else(|| NoopDisplayBackend::into_display_backend(None));

        attach_gpu_device(
            &mut vmm,
            &mut _shm_manager,
            #[cfg(not(feature = "tee"))]
            export_table.clone(),
            intc.clone(),
            virgl_flags,
            Box::from(&vm_resources.displays[..]),
            display_backend,
            #[cfg(target_os = "macos")]
            _sender.clone(),
        )?;
    }

    #[cfg(feature = "input")]
    if !vm_resources.input_backends.is_empty() {
        attach_input_devices(&mut vmm, &vm_resources.input_backends, intc.clone())?;
    }

    #[cfg(not(feature = "tee"))]
    attach_fs_devices(
        &mut vmm,
        &vm_resources.fs,
        &mut _shm_manager,
        #[cfg(not(feature = "tee"))]
        export_table,
        intc.clone(),
        exit_code.clone(),
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        _sender.clone(),
    )?;
    trace.mark("fs.ready");
    #[cfg(not(any(feature = "tee", feature = "aws-nitro")))]
    attach_custom_fs_devices(
        &mut vmm,
        &vm_resources.custom_fs,
        &mut _shm_manager,
        vm_resources.fs.len(),
        intc.clone(),
        exit_code,
        #[cfg(target_os = "macos")]
        _sender,
    )?;
    trace.mark("custom_fs.ready");
    #[cfg(feature = "blk")]
    attach_block_devices(&mut vmm, &vm_resources.block, intc.clone())?;
    #[cfg(feature = "blk")]
    trace.mark("block.ready");

    #[cfg(not(target_os = "windows"))]
    if let Some(vsock) = vm_resources.vsock.get() {
        attach_unixsock_vsock_device(&mut vmm, vsock, event_manager, intc.clone())?;
        let tsi_flags = vm_resources.vsock.tsi_flags();
        if tsi_flags.contains(TsiFlags::HIJACK_INET) {
            vmm.kernel_cmdline.insert_str("tsi_hijack")?;
        }
        if tsi_flags.contains(TsiFlags::HIJACK_UNIX) {
            vmm.kernel_cmdline.insert_str("tsi_hijack_unix")?;
        }
        trace.mark("vsock.ready");
    } else {
        trace.mark("vsock.skipped");
    }

    #[cfg(feature = "net")]
    attach_net_devices(&mut vmm, &vm_resources.net, intc.clone())?;
    #[cfg(feature = "net")]
    trace.mark("net.ready");
    #[cfg(feature = "snd")]
    if vm_resources.snd_device {
        attach_snd_device(&mut vmm, intc.clone())?;
    }

    if let Some(s) = &vm_resources.kernel_cmdline.epilog {
        vmm.kernel_cmdline.insert_str(s)?;
    };

    // Write the kernel command line to guest memory. This is x86_64 specific, since on
    // aarch64 the command line will be specified through the FDT.
    #[cfg(all(target_arch = "x86_64", not(feature = "tee")))]
    load_cmdline(&vmm)?;

    vmm.configure_system(
        vcpus.as_slice(),
        &intc,
        &payload_config.initrd_config,
        &vm_resources.smbios_oem_strings,
    )
    .map_err(StartMicrovmError::Internal)?;
    trace.mark("system.configured");

    #[cfg(feature = "tee")]
    {
        match tee {
            #[cfg(feature = "amd-sev")]
            Tee::Snp => {
                let cpuid = _kvm
                    .fd()
                    .get_supported_cpuid(KVM_MAX_CPUID_ENTRIES)
                    .map_err(VstateError::KvmCpuId)
                    .map_err(StartMicrovmError::SecureVirtAttest)?;
                vmm.kvm_vm()
                    .snp_secure_virt_measure(
                        cpuid,
                        vmm.guest_memory(),
                        measured_regions,
                        snp_launcher.unwrap(),
                    )
                    .map_err(StartMicrovmError::SecureVirtAttest)?;
            }
            #[cfg(feature = "tdx")]
            Tee::Tdx => {
                vmm.kvm_vm()
                    .tdx_secure_virt_prepare_memory(&mut tdx_launcher, &measured_regions)
                    .unwrap();
                vmm.kvm_vm()
                    .tdx_secure_virt_finalize_vm(tdx_launcher)
                    .map_err(StartMicrovmError::SecureVirtPrepare)?;
            }
            _ => return Err(StartMicrovmError::InvalidTee),
        }

        println!("Starting TEE/microVM.");
    }

    #[cfg(any(target_os = "linux", target_os = "windows"))]
    if let Some(affinity) = &vm_resources.vcpu_affinity {
        // The public builder validates an exact map for the full possible topology.
        for (vcpu, host_cpu) in vcpus.iter_mut().zip(affinity.iter().copied()) {
            vcpu.set_host_cpu(host_cpu);
        }
    }

    vmm.start_vcpus(vcpus)
        .map_err(StartMicrovmError::Internal)?;
    trace.mark("vcpus.started");

    // Clippy thinks we don't need Arc<Mutex<...
    // but we don't want to change the event_manager interface
    #[allow(clippy::arc_with_non_send_sync)]
    let vmm = Arc::new(Mutex::new(vmm));
    event_manager
        .add_subscriber(vmm.clone())
        .map_err(StartMicrovmError::RegisterEvent)?;
    trace.mark("build_microvm.done");

    Ok(vmm)
}

struct BootTrace {
    enabled: bool,
    scope: &'static str,
    start: Instant,
    last: Instant,
}

impl BootTrace {
    fn new(scope: &'static str) -> Self {
        let now = Instant::now();
        Self {
            enabled: std::env::var_os("MSB_KRUN_BOOT_TRACE").is_some(),
            scope,
            start: now,
            last: now,
        }
    }

    fn mark(&mut self, label: &'static str) {
        if !self.enabled {
            return;
        }

        let now = Instant::now();
        eprintln!(
            "krun.boot scope={} label={} elapsed_us={} delta_us={}",
            self.scope,
            label,
            now.duration_since(self.start).as_micros(),
            now.duration_since(self.last).as_micros(),
        );
        self.last = now;
    }
}

#[cfg(all(target_arch = "x86_64", not(target_os = "windows")))]
fn load_elf64_kernel(
    guest_mem: &GuestMemoryMmap,
    kernel_data: &[u8],
) -> std::result::Result<GuestAddress, ElfLoadError> {
    const ELF_HEADER_SIZE: usize = 64;
    const ELF_MAGIC: &[u8; 4] = b"\x7fELF";
    const ELFCLASS64: u8 = 2;
    const ELFDATA2LSB: u8 = 1;
    const PT_LOAD: u32 = 1;
    const ELF64_PHDR_SIZE: u16 = 56;

    if kernel_data.len() < ELF_HEADER_SIZE {
        return Err(ElfLoadError::TruncatedHeader);
    }
    if kernel_data.get(0..4) != Some(ELF_MAGIC.as_slice()) {
        return Err(ElfLoadError::InvalidMagic);
    }
    if kernel_data[4] != ELFCLASS64 {
        return Err(ElfLoadError::UnsupportedClass(kernel_data[4]));
    }
    if kernel_data[5] != ELFDATA2LSB {
        return Err(ElfLoadError::UnsupportedEndian(kernel_data[5]));
    }

    let entry = read_elf_u64(kernel_data, 24)?;
    let phoff = read_elf_u64(kernel_data, 32)? as usize;
    let phentsize = read_elf_u16(kernel_data, 54)?;
    let phnum = read_elf_u16(kernel_data, 56)? as usize;

    if phentsize != ELF64_PHDR_SIZE {
        return Err(ElfLoadError::InvalidProgramHeaderSize(phentsize));
    }
    if phoff < ELF_HEADER_SIZE {
        return Err(ElfLoadError::InvalidProgramHeaderOffset);
    }

    for index in 0..phnum {
        let phdr_delta = index
            .checked_mul(phentsize as usize)
            .ok_or(ElfLoadError::ProgramHeaderOutOfBounds)?;
        let phdr_offset = phoff
            .checked_add(phdr_delta)
            .ok_or(ElfLoadError::ProgramHeaderOutOfBounds)?;
        let phdr_end = phdr_offset
            .checked_add(ELF64_PHDR_SIZE as usize)
            .ok_or(ElfLoadError::ProgramHeaderOutOfBounds)?;
        if phdr_end > kernel_data.len() {
            return Err(ElfLoadError::ProgramHeaderOutOfBounds);
        }

        let p_type = read_elf_u32(kernel_data, phdr_offset)?;
        if p_type != PT_LOAD {
            continue;
        }

        let p_offset = read_elf_u64(kernel_data, phdr_offset + 8)? as usize;
        let p_paddr = read_elf_u64(kernel_data, phdr_offset + 24)?;
        let p_filesz = read_elf_u64(kernel_data, phdr_offset + 32)? as usize;
        let p_memsz = read_elf_u64(kernel_data, phdr_offset + 40)? as usize;
        if p_filesz > p_memsz {
            return Err(ElfLoadError::SegmentOutOfBounds);
        }

        // ELF kernels describe where loadable bytes live in both the file and guest physical
        // memory. Keep both sides checked before handing slices to vm-memory.
        let file_end = p_offset
            .checked_add(p_filesz)
            .ok_or(ElfLoadError::SegmentOutOfBounds)?;
        if file_end > kernel_data.len() {
            return Err(ElfLoadError::SegmentOutOfBounds);
        }

        p_paddr
            .checked_add(p_memsz as u64)
            .ok_or(ElfLoadError::SegmentAddressOverflow)?;

        let guest_addr = GuestAddress(p_paddr);
        if p_filesz > 0 {
            guest_mem
                .write_slice(&kernel_data[p_offset..file_end], guest_addr)
                .map_err(ElfLoadError::GuestWrite)?;
        }

        let zero_len = p_memsz - p_filesz;
        if zero_len > 0 {
            // Guest memory starts zeroed, but explicitly clearing the BSS tail keeps the loader
            // correct if this path is ever reused with pre-populated memory regions.
            let mut remaining = zero_len;
            let mut offset = p_filesz as u64;
            let zero_page = [0u8; 4096];
            while remaining > 0 {
                let chunk_len = remaining.min(zero_page.len());
                let addr = p_paddr
                    .checked_add(offset)
                    .ok_or(ElfLoadError::SegmentAddressOverflow)?;
                guest_mem
                    .write_slice(&zero_page[..chunk_len], GuestAddress(addr))
                    .map_err(ElfLoadError::GuestWrite)?;
                remaining -= chunk_len;
                offset += chunk_len as u64;
            }
        }
    }

    Ok(GuestAddress(entry))
}

#[cfg(all(target_arch = "x86_64", not(target_os = "windows")))]
fn decode_image_bz2_kernel(data: &[u8]) -> std::result::Result<Vec<u8>, StartMicrovmError> {
    const ELF_MAGIC: &[u8; 4] = b"\x7fELF";

    let mut decoder_error = None;
    let mut decoded_non_elf = false;

    // An Image prefix can contain bytes that resemble a BZIP2 header, so keep looking until a
    // candidate both decompresses successfully and contains the expected ELF payload.
    for (magic, window) in data.windows(3).enumerate() {
        if window != b"BZh" {
            continue;
        }

        let mut kernel_data = Vec::new();
        let mut bz2 = bzip2::read::BzDecoder::new(&data[magic..]);
        match bz2.read_to_end(&mut kernel_data) {
            Ok(_) if kernel_data.starts_with(ELF_MAGIC) => {
                debug!("Found BZIP2 kernel payload in Image file at: 0x{magic:x}");
                return Ok(kernel_data);
            }
            Ok(_) => decoded_non_elf = true,
            Err(err) => decoder_error = Some(err),
        }
    }

    if decoded_non_elf {
        Err(StartMicrovmError::ImageBz2LoadKernel(
            ElfLoadError::InvalidMagic,
        ))
    } else if let Some(err) = decoder_error {
        Err(StartMicrovmError::ImageBz2Decoder(err))
    } else {
        Err(StartMicrovmError::ImageBz2Invalid)
    }
}

#[cfg(all(target_arch = "x86_64", not(target_os = "windows")))]
fn read_elf_u16(data: &[u8], offset: usize) -> std::result::Result<u16, ElfLoadError> {
    let bytes = data
        .get(offset..offset + 2)
        .ok_or(ElfLoadError::TruncatedHeader)?;
    Ok(u16::from_le_bytes(bytes.try_into().unwrap()))
}

#[cfg(all(target_arch = "x86_64", not(target_os = "windows")))]
fn read_elf_u32(data: &[u8], offset: usize) -> std::result::Result<u32, ElfLoadError> {
    let bytes = data
        .get(offset..offset + 4)
        .ok_or(ElfLoadError::ProgramHeaderOutOfBounds)?;
    Ok(u32::from_le_bytes(bytes.try_into().unwrap()))
}

#[cfg(all(target_arch = "x86_64", not(target_os = "windows")))]
fn read_elf_u64(data: &[u8], offset: usize) -> std::result::Result<u64, ElfLoadError> {
    let bytes = data
        .get(offset..offset + 8)
        .ok_or(ElfLoadError::ProgramHeaderOutOfBounds)?;
    Ok(u64::from_le_bytes(bytes.try_into().unwrap()))
}

#[cfg(all(target_arch = "x86_64", target_os = "windows"))]
fn load_external_kernel(
    _guest_mem: &GuestMemoryMmap,
    _arch_mem_info: &ArchMemoryInfo,
    _external_kernel: &ExternalKernel,
) -> std::result::Result<(GuestAddress, Option<InitrdConfig>, Option<String>), StartMicrovmError> {
    Err(StartMicrovmError::KernelFormatUnsupported)
}

#[cfg(not(all(target_arch = "x86_64", target_os = "windows")))]
fn load_external_kernel(
    guest_mem: &GuestMemoryMmap,
    arch_mem_info: &ArchMemoryInfo,
    external_kernel: &ExternalKernel,
) -> std::result::Result<(GuestAddress, Option<InitrdConfig>, Option<String>), StartMicrovmError> {
    let entry_addr: GuestAddress = match external_kernel.format {
        // Raw images are treated as bundled kernels on x86_64
        #[cfg(all(target_arch = "x86_64", not(target_os = "windows")))]
        KernelFormat::Raw => unreachable!(),
        #[cfg(any(target_arch = "aarch64", target_arch = "riscv64"))]
        KernelFormat::Raw => {
            let data: Vec<u8> = std::fs::read(external_kernel.path.clone())
                .map_err(StartMicrovmError::RawOpenKernel)?;
            guest_mem.write(&data, GuestAddress(0x8000_0000)).unwrap();
            GuestAddress(0x8000_0000)
        }
        #[cfg(all(target_arch = "x86_64", not(target_os = "windows")))]
        KernelFormat::Elf => {
            let data = std::fs::read(external_kernel.path.clone())
                .map_err(StartMicrovmError::ElfOpenKernel)?;
            load_elf64_kernel(guest_mem, &data).map_err(StartMicrovmError::ElfLoadKernel)?
        }
        #[cfg(any(target_arch = "aarch64", target_arch = "riscv64"))]
        KernelFormat::PeGz => {
            let data: Vec<u8> = std::fs::read(external_kernel.path.clone())
                .map_err(StartMicrovmError::PeGzOpenKernel)?;
            if let Some(magic) = data
                .windows(3)
                .position(|window| window == [0x1f, 0x8b, 0x8])
            {
                debug!("Found GZIP header on PE file at: 0x{magic:x}");
                let (_, compressed) = data.split_at(magic);
                let mut gz = GzDecoder::new(compressed);
                let mut kernel_data: Vec<u8> = Vec::new();
                gz.read_to_end(&mut kernel_data)
                    .map_err(StartMicrovmError::PeGzDecoder)?;
                guest_mem
                    .write(&kernel_data, GuestAddress(0x8000_0000))
                    .unwrap();
                GuestAddress(0x8000_0000)
            } else {
                return Err(StartMicrovmError::PeGzInvalid);
            }
        }
        #[cfg(all(target_arch = "x86_64", not(target_os = "windows")))]
        KernelFormat::ImageBz2 => {
            let data: Vec<u8> = std::fs::read(external_kernel.path.clone())
                .map_err(StartMicrovmError::ImageBz2OpenKernel)?;
            let kernel_data = decode_image_bz2_kernel(&data)?;
            load_elf64_kernel(guest_mem, &kernel_data)
                .map_err(StartMicrovmError::ImageBz2LoadKernel)?
        }
        #[cfg(all(target_arch = "x86_64", not(target_os = "windows")))]
        KernelFormat::ImageGz => {
            let data: Vec<u8> = std::fs::read(external_kernel.path.clone())
                .map_err(StartMicrovmError::ImageGzOpenKernel)?;
            if let Some(magic) = data
                .windows(3)
                .position(|window| window == [0x1f, 0x8b, 0x8])
            {
                debug!("Found GZIP header on Image file at: 0x{magic:x}");
                let (_, compressed) = data.split_at(magic);
                let mut gz = GzDecoder::new(compressed);
                let mut kernel_data: Vec<u8> = Vec::new();
                gz.read_to_end(&mut kernel_data)
                    .map_err(StartMicrovmError::ImageGzDecoder)?;
                load_elf64_kernel(guest_mem, &kernel_data)
                    .map_err(StartMicrovmError::ImageGzLoadKernel)?
            } else {
                return Err(StartMicrovmError::ImageGzInvalid);
            }
        }
        #[cfg(all(target_arch = "x86_64", not(target_os = "windows")))]
        KernelFormat::ImageZstd => {
            let data: Vec<u8> = std::fs::read(external_kernel.path.clone())
                .map_err(StartMicrovmError::ImageZstdOpenKernel)?;
            if let Some(magic) = data
                .windows(4)
                .position(|window| window == [0x28, 0xb5, 0x2f, 0xfd])
            {
                debug!("Found ZSTD header on Image file at: 0x{magic:x}");
                let (_, zstd_data) = data.split_at(magic);
                let mut kernel_data: Vec<u8> = Vec::new();
                let _ = zstd::stream::copy_decode(zstd_data, &mut kernel_data);
                load_elf64_kernel(guest_mem, &kernel_data)
                    .map_err(StartMicrovmError::ImageZstdLoadKernel)?
            } else {
                return Err(StartMicrovmError::ImageZstdInvalid);
            }
        }
        _ => return Err(StartMicrovmError::KernelFormatUnsupported),
    };

    debug!("load_external_kernel: 0x{:x}", entry_addr.0);

    let initrd_config = if let Some(initramfs_path) = &external_kernel.initramfs_path {
        let data = std::fs::read(initramfs_path).map_err(StartMicrovmError::InitrdRead)?;
        guest_mem
            .write(&data, GuestAddress(arch_mem_info.initrd_addr))
            .unwrap();
        Some(InitrdConfig {
            address: GuestAddress(arch_mem_info.initrd_addr),
            size: data.len(),
        })
    } else {
        None
    };

    Ok((entry_addr, initrd_config, external_kernel.cmdline.clone()))
}

fn load_payload(
    _vm_resources: &VmResources,
    guest_mem: GuestMemoryMmap,
    _arch_mem_info: &ArchMemoryInfo,
    payload: &Payload,
) -> std::result::Result<
    (
        GuestMemoryMmap,
        GuestAddress,
        Option<InitrdConfig>,
        Option<String>,
    ),
    StartMicrovmError,
> {
    match payload {
        #[cfg(any(
            all(target_arch = "x86_64", target_os = "windows", not(feature = "tee")),
            target_arch = "aarch64",
            target_arch = "riscv64"
        ))]
        Payload::KernelCopy => {
            let (kernel_entry_addr, kernel_host_addr, kernel_guest_addr, kernel_size) =
                if let Some(kernel_bundle) = &_vm_resources.kernel_bundle {
                    (
                        kernel_bundle.entry_addr,
                        kernel_bundle.host_addr,
                        kernel_bundle.guest_addr,
                        kernel_bundle.size,
                    )
                } else {
                    return Err(StartMicrovmError::MissingKernelConfig);
                };

            let kernel_data =
                unsafe { std::slice::from_raw_parts(kernel_host_addr as *mut u8, kernel_size) };
            if kernel_guest_addr + kernel_size as u64 > _arch_mem_info.ram_last_addr {
                return Err(StartMicrovmError::KernelDoesNotFit(
                    kernel_guest_addr,
                    kernel_size,
                ));
            }
            guest_mem
                .write(kernel_data, GuestAddress(kernel_guest_addr))
                .unwrap();

            let initrd_config = if let Some(initrd_bundle) = &_vm_resources.initrd_bundle {
                let initrd_data = unsafe {
                    std::slice::from_raw_parts(
                        initrd_bundle.host_addr as *mut u8,
                        initrd_bundle.size,
                    )
                };

                guest_mem
                    .write(initrd_data, GuestAddress(_arch_mem_info.initrd_addr))
                    .map_err(|_| StartMicrovmError::InitrdLoad)?;

                Some(InitrdConfig {
                    address: GuestAddress(_arch_mem_info.initrd_addr),
                    size: initrd_data.len(),
                })
            } else {
                None
            };

            Ok((
                guest_mem,
                GuestAddress(kernel_entry_addr),
                initrd_config,
                None,
            ))
        }
        #[cfg(all(
            target_arch = "x86_64",
            not(feature = "tee"),
            not(target_os = "windows")
        ))]
        Payload::KernelMmap => {
            let (kernel_entry_addr, kernel_host_addr, kernel_guest_addr, kernel_size) =
                if let Some(kernel_bundle) = &_vm_resources.kernel_bundle {
                    (
                        kernel_bundle.entry_addr,
                        kernel_bundle.host_addr,
                        kernel_bundle.guest_addr,
                        kernel_bundle.size,
                    )
                } else {
                    return Err(StartMicrovmError::MissingKernelConfig);
                };

            let kernel_region = unsafe {
                MmapRegion::build_raw(kernel_host_addr as *mut u8, kernel_size, 0, 0)
                    .map_err(StartMicrovmError::InvalidKernelBundle)?
            };

            Ok((
                guest_mem
                    .insert_region(Arc::new(
                        GuestRegionMmap::new(kernel_region, GuestAddress(kernel_guest_addr))
                            .ok_or(StartMicrovmError::GuestMemoryRegion)?,
                    ))
                    .map_err(StartMicrovmError::GuestMemoryRegionCollection)?,
                GuestAddress(kernel_entry_addr),
                None,
                None,
            ))
        }
        Payload::ExternalKernel(external_kernel) => {
            let (entry_addr, initrd_config, cmdline) =
                load_external_kernel(&guest_mem, _arch_mem_info, external_kernel)?;
            Ok((guest_mem, entry_addr, initrd_config, cmdline))
        }
        #[cfg(test)]
        Payload::Empty => Ok((guest_mem, GuestAddress(0), None, None)),
        #[cfg(feature = "tee")]
        Payload::Tee => {
            let (kernel_host_addr, kernel_guest_addr, kernel_size) =
                if let Some(kernel_bundle) = &_vm_resources.kernel_bundle {
                    (
                        kernel_bundle.host_addr,
                        kernel_bundle.guest_addr,
                        kernel_bundle.size,
                    )
                } else {
                    return Err(StartMicrovmError::MissingKernelConfig);
                };
            let kernel_data =
                unsafe { std::slice::from_raw_parts(kernel_host_addr as *mut u8, kernel_size) };
            guest_mem
                .write(kernel_data, GuestAddress(kernel_guest_addr))
                .unwrap();

            let (qboot_host_addr, qboot_size) =
                if let Some(qboot_bundle) = &_vm_resources.qboot_bundle {
                    (qboot_bundle.host_addr, qboot_bundle.size)
                } else {
                    return Err(StartMicrovmError::MissingKernelConfig);
                };
            let qboot_data =
                unsafe { std::slice::from_raw_parts(qboot_host_addr as *mut u8, qboot_size) };
            guest_mem
                .write(qboot_data, GuestAddress(arch::FIRMWARE_START))
                .unwrap();

            let (initrd_host_addr, initrd_size) =
                if let Some(initrd_bundle) = &_vm_resources.initrd_bundle {
                    (initrd_bundle.host_addr, initrd_bundle.size)
                } else {
                    return Err(StartMicrovmError::MissingKernelConfig);
                };
            let initrd_data =
                unsafe { std::slice::from_raw_parts(initrd_host_addr as *mut u8, initrd_size) };
            guest_mem
                .write(initrd_data, GuestAddress(_arch_mem_info.initrd_addr))
                .unwrap();

            let initrd_config = InitrdConfig {
                address: GuestAddress(_arch_mem_info.initrd_addr),
                size: initrd_data.len(),
            };

            Ok((
                guest_mem,
                GuestAddress(arch::RESET_VECTOR),
                Some(initrd_config),
                None,
            ))
        }
        Payload::Firmware => Ok((guest_mem, GuestAddress(arch::RESET_VECTOR), None, None)),
    }
}

pub struct PayloadConfig {
    entry_addr: GuestAddress,
    initrd_config: Option<InitrdConfig>,
    kernel_cmdline: Option<String>,
}

pub fn create_guest_memory(
    mem_size: usize,
    vm_resources: &VmResources,
    payload: &Payload,
) -> std::result::Result<
    (GuestMemoryMmap, ArchMemoryInfo, ShmManager, PayloadConfig),
    StartMicrovmError,
> {
    let mem_size = mem_size << 20;

    #[cfg(not(feature = "efi"))]
    let (firmware_data, firmware_size) = if let Some(firmware) = &vm_resources.firmware_config {
        let data = std::fs::read(firmware.path.clone()).map_err(StartMicrovmError::FirmwareRead)?;
        let len = data.len();
        (Some(data), Some(len))
    } else {
        (None, None)
    };
    #[cfg(feature = "efi")]
    let (firmware_data, firmware_size) = (Some(EDK2_BINARY), Some(EDK2_BINARY.len()));

    #[cfg(target_arch = "x86_64")]
    let (arch_mem_info, mut arch_mem_regions) = match payload {
        #[cfg(all(not(feature = "tee"), not(target_os = "windows")))]
        Payload::KernelMmap => {
            let (kernel_guest_addr, kernel_size) =
                if let Some(kernel_bundle) = &vm_resources.kernel_bundle {
                    (kernel_bundle.guest_addr, kernel_bundle.size)
                } else {
                    return Err(StartMicrovmError::MissingKernelConfig);
                };
            arch::arch_memory_regions(mem_size, Some(kernel_guest_addr), kernel_size, 0, None)
        }
        Payload::ExternalKernel(external_kernel) => arch::arch_memory_regions(
            mem_size,
            None,
            0,
            external_kernel.initramfs_size,
            firmware_size,
        ),
        // WHP maps ordinary guest RAM up front, so copy the bundled kernel into
        // that RAM instead of inserting a raw libkrunfw-backed region like KVM.
        #[cfg(all(target_arch = "x86_64", target_os = "windows", not(feature = "tee")))]
        Payload::KernelCopy => {
            let initrd_size = vm_resources
                .initrd_bundle
                .as_ref()
                .map(|initrd| initrd.size as u64)
                .unwrap_or(0);
            arch::arch_memory_regions(mem_size, None, 0, initrd_size, firmware_size)
        }
        #[cfg(feature = "tee")]
        Payload::Tee => {
            let (kernel_guest_addr, kernel_size) =
                if let Some(kernel_bundle) = &vm_resources.kernel_bundle {
                    (kernel_bundle.guest_addr, kernel_bundle.size)
                } else {
                    return Err(StartMicrovmError::MissingKernelConfig);
                };
            arch::arch_memory_regions(mem_size, Some(kernel_guest_addr), kernel_size, 0, None)
        }
        #[cfg(test)]
        Payload::Empty => arch::arch_memory_regions(mem_size, None, 0, 0, None),
        Payload::Firmware => arch::arch_memory_regions(mem_size, None, 0, 0, firmware_size),
    };
    #[cfg(any(target_arch = "aarch64", target_arch = "riscv64"))]
    let (arch_mem_info, mut arch_mem_regions) = match payload {
        Payload::ExternalKernel(external_kernel) => {
            arch::arch_memory_regions(mem_size, external_kernel.initramfs_size, None)
        }
        _ => {
            let initrd_size = vm_resources
                .initrd_bundle
                .as_ref()
                .map(|initrd| initrd.size as u64)
                .unwrap_or(0);
            arch::arch_memory_regions(mem_size, initrd_size, firmware_size)
        }
    };

    let mut shm_manager = ShmManager::new(&arch_mem_info);

    #[cfg(not(feature = "tee"))]
    for (index, fs) in vm_resources.fs.iter().enumerate() {
        if let Some(shm_size) = fs.shm_size {
            shm_manager
                .create_fs_region(index, shm_size)
                .map_err(StartMicrovmError::ShmCreate)?;
        }
    }
    #[cfg(not(any(feature = "tee", feature = "aws-nitro")))]
    {
        let offset = vm_resources.fs.len();
        for (i, custom) in vm_resources.custom_fs.iter().enumerate() {
            if let Some(shm_size) = custom.shm_size {
                shm_manager
                    .create_fs_region(offset + i, shm_size)
                    .map_err(StartMicrovmError::ShmCreate)?;
            }
        }
    }
    if vm_resources.gpu_virgl_flags.is_some() {
        let size = vm_resources.gpu_shm_size.unwrap_or(1 << 33);
        shm_manager
            .create_gpu_region(size)
            .map_err(StartMicrovmError::ShmCreate)?;
    }

    arch_mem_regions.extend(shm_manager.regions());

    // Reserve the virtio-mem hotplug region above every other window. The
    // backing is anonymous and lazily faulted, so unplugged capacity costs no
    // host memory; the region is deliberately absent from the FDT/MPTABLE
    // memory nodes — the guest discovers it through the virtio-mem device.
    #[cfg(not(feature = "tee"))]
    if let Some(mem_device) = &vm_resources.mem_device {
        let boot_mib = vm_resources.vm_config().mem_size_mib.unwrap_or(128) as u64;
        let max_mib = vm_resources
            .vm_config()
            .max_mem_size_mib
            .map(|mib| mib as u64)
            .unwrap_or(boot_mib);
        let block = devices::virtio::VIRTIO_MEM_BLOCK_SIZE;
        let hotplug_bytes = (max_mib.saturating_sub(boot_mib) << 20) / block * block;
        if hotplug_bytes > 0 {
            const GIB: u64 = 1 << 30;
            let base = shm_manager.next_guest_addr().div_ceil(GIB) * GIB;
            mem_device
                .lock()
                .unwrap()
                .set_region(base, hotplug_bytes)
                .expect("hotplug region is block-aligned by construction");
            arch_mem_regions.push((GuestAddress(base), hotplug_bytes as usize));
        }
    }

    let guest_mem = GuestMemoryMmap::from_ranges(&arch_mem_regions)
        .map_err(StartMicrovmError::GuestMemoryMmapFromRanges)?;

    let (guest_mem, entry_addr, initrd_config, cmdline) =
        load_payload(vm_resources, guest_mem, &arch_mem_info, payload)?;

    // Only write firmware if data exists AND this isn't an ExternalKernel payload
    // (ExternalKernel does direct kernel boot and doesn't use EFI firmware)
    if !matches!(payload, Payload::ExternalKernel(_)) {
        if let Some(firmware_data) = firmware_data.as_ref() {
            guest_mem
                .write(firmware_data, GuestAddress(arch_mem_info.firmware_addr))
                .map_err(StartMicrovmError::FirmwareInvalidAddress)?;
        }
    }

    crate::metrics::install_host_resident_memory_sampler(
        &vm_resources.metrics,
        &guest_mem,
        &arch_mem_info,
    );

    let payload_config = PayloadConfig {
        entry_addr,
        initrd_config,
        kernel_cmdline: cmdline.clone(),
    };

    Ok((guest_mem, arch_mem_info, shm_manager, payload_config))
}

#[cfg(all(target_arch = "x86_64", not(feature = "tee")))]
fn load_cmdline(vmm: &Vmm) -> std::result::Result<(), StartMicrovmError> {
    kernel::loader::load_cmdline(
        vmm.guest_memory(),
        GuestAddress(arch::x86_64::layout::CMDLINE_START),
        &vmm.kernel_cmdline
            .as_cstring()
            .map_err(StartMicrovmError::LoadCommandline)?,
    )
    .map_err(StartMicrovmError::LoadCommandline)
}

#[cfg(all(target_os = "linux", not(feature = "tee")))]
pub(crate) fn setup_vm(
    guest_memory: &GuestMemoryMmap,
    _nested_enabled: bool,
    _vcpu_count: u8,
) -> std::result::Result<Vm, StartMicrovmError> {
    let kvm = KvmContext::new()
        .map_err(Error::KvmContext)
        .map_err(StartMicrovmError::Internal)?;
    let mut vm = Vm::new(kvm.fd())
        .map_err(Error::Vm)
        .map_err(StartMicrovmError::Internal)?;
    vm.memory_init(guest_memory, kvm.max_memslots())
        .map_err(Error::Vm)
        .map_err(StartMicrovmError::Internal)?;
    Ok(vm)
}
#[cfg(all(target_os = "linux", feature = "tee"))]
pub(crate) fn setup_vm(
    kvm: &KvmContext,
    guest_memory: &GuestMemoryMmap,
    resources: &super::resources::VmResources,
    #[cfg(feature = "tdx")] _sender: Sender<WorkerMessage>,
) -> std::result::Result<Vm, StartMicrovmError> {
    let mut vm = Vm::new(
        kvm.fd(),
        resources.tee_config(),
        #[cfg(feature = "tdx")]
        _sender,
    )
    .map_err(Error::Vm)
    .map_err(StartMicrovmError::Internal)?;
    vm.memory_init(guest_memory, kvm.max_memslots())
        .map_err(Error::Vm)
        .map_err(StartMicrovmError::Internal)?;
    Ok(vm)
}
#[cfg(target_os = "macos")]
pub(crate) fn setup_vm(
    guest_memory: &GuestMemoryMmap,
    nested_enabled: bool,
    _vcpu_count: u8,
) -> std::result::Result<Vm, StartMicrovmError> {
    let mut vm = Vm::new(nested_enabled)
        .map_err(Error::Vm)
        .map_err(StartMicrovmError::Internal)?;
    vm.memory_init(guest_memory)
        .map_err(Error::Vm)
        .map_err(StartMicrovmError::Internal)?;
    Ok(vm)
}
#[cfg(target_os = "windows")]
pub(crate) fn setup_vm(
    guest_memory: &GuestMemoryMmap,
    _nested_enabled: bool,
    vcpu_count: u8,
) -> std::result::Result<Vm, StartMicrovmError> {
    let mut vm = Vm::new(vcpu_count)
        .map_err(Error::Vm)
        .map_err(StartMicrovmError::Internal)?;
    vm.memory_init(guest_memory)
        .map_err(Error::Vm)
        .map_err(StartMicrovmError::Internal)?;
    Ok(vm)
}

/// Sets up the serial device.
#[cfg(any(
    not(target_os = "windows"),
    all(
        any(target_arch = "aarch64", target_arch = "x86_64"),
        target_os = "windows"
    )
))]
pub fn setup_serial_device(
    event_manager: &mut EventManager,
    input: Option<Box<dyn devices::legacy::ReadableFd + Send>>,
    out: Option<Box<dyn io::Write + Send>>,
) -> std::result::Result<Arc<Mutex<Serial>>, StartMicrovmError> {
    let interrupt_evt = EventFd::new(utils::eventfd::EFD_NONBLOCK)
        .map_err(Error::EventFd)
        .map_err(StartMicrovmError::Internal)?;
    let has_input = input.is_some();
    let serial = Arc::new(Mutex::new(Serial::new(interrupt_evt, out, input)));
    if has_input {
        if let Err(e) = event_manager.add_subscriber(serial.clone()) {
            // TODO: We just log this message, and immediately return Ok, instead of returning the
            // actual error because this operation always fails with EPERM when adding a fd which
            // has been redirected to /dev/null via dup2 (this may happen inside the jailer).
            // Find a better solution to this (and think about the state of the serial device
            // while we're at it).
            warn!("Could not add serial input event to epoll: {e:?}");
        }
    }
    Ok(serial)
}

#[cfg(all(target_arch = "x86_64", target_os = "windows"))]
struct WindowsX64PitStub {
    channels: [u8; 3],
    command: u8,
    channel0_access: u8,
    channel0_mode: u8,
    channel0_write_lsb: Option<u8>,
    channel0_period_ns: Arc<AtomicU64>,
    channel0_periodic: Arc<AtomicBool>,
    channel2_latch: u32,
    channel2_started_at: Option<Instant>,
    channel2_write_lsb: Option<u8>,
    channel2_read_msb_next: bool,
    running: Arc<AtomicBool>,
}

#[cfg(all(target_arch = "x86_64", target_os = "windows"))]
const PIT_TICK_RATE_HZ: u128 = 1_193_182;

#[cfg(all(target_arch = "x86_64", target_os = "windows"))]
#[derive(Default)]
struct WindowsX64PicStub {
    command: u8,
    data: u8,
}

#[cfg(all(target_arch = "x86_64", target_os = "windows"))]
#[derive(Default)]
struct WindowsX64PostCodeStub {
    value: u8,
}

#[cfg(all(target_arch = "x86_64", target_os = "windows"))]
impl WindowsX64PitStub {
    fn new(intc: IrqChip) -> Self {
        let channel0_period_ns = Arc::new(AtomicU64::new(0));
        let channel0_periodic = Arc::new(AtomicBool::new(false));
        let running = Arc::new(AtomicBool::new(true));
        let thread_period = channel0_period_ns.clone();
        let thread_periodic = channel0_periodic.clone();
        let thread_running = running.clone();

        // Channel 0 timer: ticks at the guest-programmed rate and injects
        // IRQ0 through the IOAPIC route, which already drops it while the
        // guest keeps the pin masked.
        std::thread::spawn(move || {
            while thread_running.load(Ordering::Relaxed) {
                let period_ns = thread_period.load(Ordering::Relaxed);
                if period_ns == 0 {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    continue;
                }
                // Clamp so a tiny divisor cannot busy-spin the host;
                // jiffies-grade timing is all a guest needs from a PIT.
                std::thread::sleep(std::time::Duration::from_nanos(period_ns.max(1_000_000)));
                if thread_period.load(Ordering::Relaxed) == 0 {
                    continue;
                }
                {
                    let intc = intc.lock().unwrap();
                    if let Err(e) = intc.set_irq(Some(0), None) {
                        trace!("PIT tick interrupt injection failed: {e:?}");
                    }
                }
                if !thread_periodic.load(Ordering::Relaxed) {
                    // One-shot modes fire once per reload.
                    thread_period.store(0, Ordering::Relaxed);
                }
            }
        });

        Self {
            channels: [0; 3],
            command: 0,
            channel0_access: 0,
            channel0_mode: 0,
            channel0_write_lsb: None,
            channel0_period_ns,
            channel0_periodic,
            channel2_latch: 0,
            channel2_started_at: None,
            channel2_write_lsb: None,
            channel2_read_msb_next: false,
            running,
        }
    }

    fn write_channel0(&mut self, value: u8) {
        self.channels[0] = value;

        let divisor = match self.channel0_access {
            0b01 => u32::from(value),
            0b10 => u32::from(value) << 8,
            _ => {
                if let Some(lsb) = self.channel0_write_lsb.take() {
                    u32::from(u16::from_le_bytes([lsb, value]))
                } else {
                    self.channel0_write_lsb = Some(value);
                    return;
                }
            }
        };
        let divisor = if divisor == 0 { 0x1_0000 } else { divisor };
        let period_ns = u64::from(divisor) * 1_000_000_000 / PIT_TICK_RATE_HZ as u64;

        // Modes 2 (rate generator) and 3 (square wave) reload themselves;
        // everything else behaves as a one-shot.
        self.channel0_periodic
            .store(matches!(self.channel0_mode, 2 | 3), Ordering::Relaxed);
        self.channel0_period_ns.store(period_ns, Ordering::Relaxed);
    }

    fn read_channel2(&mut self) -> u8 {
        let counter = self.channel2_counter();
        let value = if self.channel2_read_msb_next {
            (counter >> 8) as u8
        } else {
            (counter & 0xff) as u8
        };
        self.channel2_read_msb_next = !self.channel2_read_msb_next;
        value
    }

    fn write_channel2(&mut self, value: u8) {
        self.channels[2] = value;

        if self.command & 0xc0 == 0x80 && self.command & 0x30 == 0x30 {
            if let Some(lsb) = self.channel2_write_lsb.take() {
                let raw_latch = u16::from_le_bytes([lsb, value]);
                self.channel2_latch = if raw_latch == 0 {
                    0x1_0000
                } else {
                    u32::from(raw_latch)
                };
                self.channel2_started_at = Some(Instant::now());
                self.channel2_read_msb_next = false;
            } else {
                self.channel2_write_lsb = Some(value);
            }
        }
    }

    fn channel2_counter(&self) -> u16 {
        let Some(started_at) = self.channel2_started_at else {
            return self.channel2_latch.min(u32::from(u16::MAX)) as u16;
        };

        // Linux early TSC calibration polls PIT channel 2 until the MSB changes.
        // Returning a frozen byte makes that path burn the full 50k retry budget on WHP.
        let elapsed_ticks = started_at
            .elapsed()
            .as_nanos()
            .saturating_mul(PIT_TICK_RATE_HZ)
            / 1_000_000_000;

        if elapsed_ticks >= u128::from(self.channel2_latch) {
            0
        } else {
            (u128::from(self.channel2_latch) - elapsed_ticks).min(u128::from(u16::MAX)) as u16
        }
    }
}

#[cfg(all(target_arch = "x86_64", target_os = "windows"))]
impl Drop for WindowsX64PitStub {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
    }
}

#[cfg(all(target_arch = "x86_64", target_os = "windows"))]
impl devices::BusDevice for WindowsX64PitStub {
    fn read(&mut self, _vcpuid: u64, offset: u64, data: &mut [u8]) {
        if data.len() != 1 {
            return;
        }

        data[0] = match offset {
            0..=1 => self.channels[offset as usize],
            2 => self.read_channel2(),
            3 => self.command,
            _ => 0,
        };
    }

    fn write(&mut self, _vcpuid: u64, offset: u64, data: &[u8]) {
        if data.len() != 1 {
            return;
        }

        match offset {
            0 => self.write_channel0(data[0]),
            1 => self.channels[1] = data[0],
            2 => self.write_channel2(data[0]),
            3 => {
                self.command = data[0];
                if self.command & 0xc0 == 0 {
                    let access = (self.command >> 4) & 0x3;
                    // Access 0 is the counter-latch command; anything else
                    // reprograms the channel, so stop ticking until the
                    // guest writes the new reload value.
                    if access != 0 {
                        self.channel0_access = access;
                        self.channel0_mode = (self.command >> 1) & 0x7;
                        self.channel0_write_lsb = None;
                        self.channel0_period_ns.store(0, Ordering::Relaxed);
                    }
                }
                if self.command & 0xc0 == 0x80 {
                    self.channel2_write_lsb = None;
                    self.channel2_read_msb_next = false;
                }
            }
            _ => {}
        }
    }
}

#[cfg(all(target_arch = "x86_64", target_os = "windows"))]
impl devices::BusDevice for WindowsX64PicStub {
    fn read(&mut self, _vcpuid: u64, offset: u64, data: &mut [u8]) {
        if data.len() != 1 {
            return;
        }

        data[0] = match offset {
            0 => self.command,
            1 => self.data,
            _ => 0,
        };
    }

    fn write(&mut self, _vcpuid: u64, offset: u64, data: &[u8]) {
        if data.len() != 1 {
            return;
        }

        match offset {
            0 => self.command = data[0],
            1 => self.data = data[0],
            _ => {}
        }
    }
}

#[cfg(all(target_arch = "x86_64", target_os = "windows"))]
impl devices::BusDevice for WindowsX64PostCodeStub {
    fn read(&mut self, _vcpuid: u64, _offset: u64, data: &mut [u8]) {
        if data.len() == 1 {
            data[0] = self.value;
        }
    }

    fn write(&mut self, _vcpuid: u64, _offset: u64, data: &[u8]) {
        if data.len() == 1 {
            self.value = data[0];
        }
    }
}

#[cfg(all(target_arch = "x86_64", target_os = "windows"))]
fn create_windows_x86_64_pio_bus(
    intc: IrqChip,
    serial: &[Arc<Mutex<Serial>>],
    exit_evt: &EventFd,
    arch_memory_info: &ArchMemoryInfo,
) -> std::result::Result<Option<devices::Bus>, StartMicrovmError> {
    let mut pio_bus = devices::Bus::new();

    if let Some(serial) = serial.first() {
        {
            let mut serial = serial.lock().unwrap();
            serial.set_intc(intc.clone());
            serial.set_irq_line(4);
        }

        pio_bus
            .insert(serial.clone(), 0x3f8, 0x8)
            .map_err(Error::LegacyPioBus)
            .map_err(StartMicrovmError::Internal)?;
    }

    pio_bus
        .insert(
            Arc::new(Mutex::new(WindowsX64PitStub::new(intc.clone()))),
            0x40,
            0x4,
        )
        .map_err(Error::LegacyPioBus)
        .map_err(StartMicrovmError::Internal)?;

    // The x64 kernel still probes and masks the legacy 8259 PIC even when WHP handles
    // interrupt delivery through the local APIC and IOAPIC emulation paths.
    pio_bus
        .insert(
            Arc::new(Mutex::new(WindowsX64PicStub::default())),
            0x20,
            0x2,
        )
        .map_err(Error::LegacyPioBus)
        .map_err(StartMicrovmError::Internal)?;
    pio_bus
        .insert(
            Arc::new(Mutex::new(WindowsX64PicStub::default())),
            0xa0,
            0x2,
        )
        .map_err(Error::LegacyPioBus)
        .map_err(StartMicrovmError::Internal)?;

    let cmos = Arc::new(Mutex::new(Cmos::new(
        arch_memory_info.ram_below_gap,
        arch_memory_info.ram_above_gap,
    )));
    {
        let mut cmos = cmos.lock().unwrap();
        cmos.set_intc(intc.clone());
        cmos.set_irq_line(8);
    }
    pio_bus
        .insert(cmos, 0x70, 0x8)
        .map_err(Error::LegacyPioBus)
        .map_err(StartMicrovmError::Internal)?;

    // Linux uses port 0x80 as a legacy POST/checkpoint delay sink during early x86 boot.
    pio_bus
        .insert(
            Arc::new(Mutex::new(WindowsX64PostCodeStub::default())),
            0x80,
            0x1,
        )
        .map_err(Error::LegacyPioBus)
        .map_err(StartMicrovmError::Internal)?;

    let reset_evt = exit_evt
        .try_clone()
        .map_err(Error::EventFd)
        .map_err(StartMicrovmError::Internal)?;
    let kbd_evt = EventFd::new(utils::eventfd::EFD_NONBLOCK)
        .map_err(Error::EventFd)
        .map_err(StartMicrovmError::Internal)?;
    let i8042 = Arc::new(Mutex::new(devices::legacy::I8042Device::new(
        reset_evt, kbd_evt,
    )));
    pio_bus
        .insert(i8042, 0x60, 0x5)
        .map_err(Error::LegacyPioBus)
        .map_err(StartMicrovmError::Internal)?;

    Ok(Some(pio_bus))
}

#[cfg(all(target_arch = "aarch64", target_os = "windows"))]
fn attach_legacy_devices(
    vm: &Vm,
    mmio_device_manager: &mut MMIODeviceManager,
    kernel_cmdline: &mut kernel::cmdline::Cmdline,
    intc: IrqChip,
    serial: Vec<Arc<Mutex<Serial>>>,
) -> std::result::Result<(), StartMicrovmError> {
    for s in serial {
        mmio_device_manager
            .register_mmio_serial(vm, kernel_cmdline, intc.clone(), s)
            .map_err(Error::RegisterMMIODevice)
            .map_err(StartMicrovmError::Internal)?;
    }

    Ok(())
}

#[cfg(all(target_arch = "x86_64", not(target_os = "windows")))]
fn attach_legacy_devices(
    vm: &Vm,
    split_irqchip: bool,
    pio_device_manager: &mut PortIODeviceManager,
    mmio_device_manager: &mut MMIODeviceManager,
    intc: Option<Arc<Mutex<IrqChipDevice>>>,
) -> std::result::Result<(), StartMicrovmError> {
    pio_device_manager
        .register_devices()
        .map_err(Error::LegacyIOBus)
        .map_err(StartMicrovmError::Internal)?;

    if split_irqchip {
        mmio_device_manager
            .register_mmio_ioapic(intc)
            .map_err(Error::RegisterMMIODevice)
            .map_err(StartMicrovmError::Internal)?;
    }

    macro_rules! register_irqfd_evt {
        ($evt: ident, $index: expr) => {{
            vm.fd()
                .register_irqfd(&pio_device_manager.$evt, $index)
                .map_err(|e| {
                    Error::LegacyIOBus(device_manager::legacy::Error::EventFd(
                        io::Error::from_raw_os_error(e.errno()),
                    ))
                })
                .map_err(StartMicrovmError::Internal)?;
        }};
    }

    register_irqfd_evt!(com_evt_1, 4);
    register_irqfd_evt!(com_evt_2, 3);
    register_irqfd_evt!(com_evt_3, 4);
    register_irqfd_evt!(com_evt_4, 3);
    register_irqfd_evt!(kbd_evt, 1);
    Ok(())
}

#[cfg(all(
    any(target_arch = "aarch64", target_arch = "riscv64"),
    target_os = "linux"
))]
fn attach_legacy_devices(
    vm: &Vm,
    mmio_device_manager: &mut MMIODeviceManager,
    kernel_cmdline: &mut kernel::cmdline::Cmdline,
    intc: IrqChip,
    serial: Vec<Arc<Mutex<Serial>>>,
) -> std::result::Result<(), StartMicrovmError> {
    for s in serial {
        mmio_device_manager
            .register_mmio_serial(vm.fd(), kernel_cmdline, intc.clone(), s)
            .map_err(Error::RegisterMMIODevice)
            .map_err(StartMicrovmError::Internal)?;
    }

    #[cfg(all(target_arch = "aarch64", target_os = "linux"))]
    mmio_device_manager
        .register_mmio_rtc(vm.fd())
        .map_err(Error::RegisterMMIODevice)
        .map_err(StartMicrovmError::Internal)?;

    Ok(())
}

#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
fn attach_legacy_devices(
    vm: &Vm,
    mmio_device_manager: &mut MMIODeviceManager,
    kernel_cmdline: &mut kernel::cmdline::Cmdline,
    intc: IrqChip,
    serial: Vec<Arc<Mutex<Serial>>>,
    event_manager: &mut EventManager,
    shutdown_efd: Option<EventFd>,
) -> Result<(), StartMicrovmError> {
    for s in serial {
        mmio_device_manager
            .register_mmio_serial(vm, kernel_cmdline, intc.clone(), s)
            .map_err(Error::RegisterMMIODevice)
            .map_err(StartMicrovmError::Internal)?;
    }

    mmio_device_manager
        .register_mmio_rtc(vm, intc.clone())
        .map_err(Error::RegisterMMIODevice)
        .map_err(StartMicrovmError::Internal)?;

    mmio_device_manager
        .register_mmio_gic(vm, intc.clone())
        .map_err(Error::RegisterMMIODevice)
        .map_err(StartMicrovmError::Internal)?;

    if let Some(shutdown_efd) = shutdown_efd {
        mmio_device_manager
            .register_mmio_gpio(vm, intc.clone(), event_manager, shutdown_efd)
            .map_err(Error::RegisterMMIODevice)
            .map_err(StartMicrovmError::Internal)?;
    }

    Ok(())
}

#[cfg(all(target_arch = "x86_64", not(target_os = "windows")))]
#[allow(clippy::too_many_arguments)]
fn create_vcpus_x86_64(
    vm: &Vm,
    vcpu_config: &VcpuConfig,
    guest_mem: &GuestMemoryMmap,
    entry_addr: GuestAddress,
    io_bus: &devices::Bus,
    exit_evt: &EventFd,
    kernel_boot: bool,
    metrics: utils::metrics::MetricsWriter,
    #[cfg(feature = "tee")] pm_sender: Sender<WorkerMessage>,
) -> super::Result<Vec<Vcpu>> {
    // Create the full possible topology; CPUs beyond `vcpu_count` boot offline (maxcpus=)
    // and wait inside KVM for INIT/SIPI until the guest onlines them.
    let mut vcpus = Vec::with_capacity(vcpu_config.max_vcpu_count as usize);
    for cpu_index in 0..vcpu_config.max_vcpu_count {
        let mut vcpu = Vcpu::new_x86_64(
            cpu_index,
            vm.fd(),
            vm.supported_cpuid().clone(),
            vm.supported_msrs().clone(),
            io_bus.clone(),
            exit_evt.try_clone().map_err(Error::EventFd)?,
            metrics.clone(),
            #[cfg(feature = "tee")]
            pm_sender.clone(),
        )
        .map_err(Error::Vcpu)?;

        vcpu.configure_x86_64(guest_mem, entry_addr, vcpu_config, kernel_boot)
            .map_err(Error::Vcpu)?;

        vcpus.push(vcpu);
    }
    Ok(vcpus)
}

#[cfg(target_os = "windows")]
fn create_vcpus_windows(
    vm: &Vm,
    vcpu_config: &VcpuConfig,
    guest_mem: &GuestMemoryMmap,
    mem_info: &ArchMemoryInfo,
    entry_addr: GuestAddress,
    exit_evt: &EventFd,
    metrics: utils::metrics::MetricsWriter,
    #[cfg(target_arch = "x86_64")] pio_bus: Option<&devices::Bus>,
) -> super::Result<Vec<Vcpu>> {
    // Create the full possible topology so CPUs beyond the boot online count
    // (reserved capacity) can come online later through the msb-cpu driver.
    // On x86 every AP parks in the startup router until the guest sends it
    // INIT/SIPI; on aarch64 only vCPU 0 gets an entry point and WHP's
    // in-hypervisor PSCI keeps the rest powered off until the guest issues
    // CPU_ON for them.
    let create_count = vcpu_config.max_vcpu_count.max(vcpu_config.vcpu_count);

    let mut vcpus = Vec::with_capacity(create_count as usize);
    // One router shared by all vCPU threads: INIT/SIPI writes trap on the
    // sending vCPU and are applied to the parked target APs through it.
    #[cfg(target_arch = "x86_64")]
    let sipi_router =
        std::sync::Arc::new(crate::windows::vstate::ApStartupRouter::new(create_count));
    for cpu_index in 0..create_count {
        let mut vcpu = vm
            .create_vcpu(
                cpu_index,
                exit_evt.try_clone().map_err(Error::EventFd)?,
                metrics.clone(),
            )
            .map_err(Error::Vcpu)?;
        vcpu.configure_windows(guest_mem, mem_info, entry_addr)
            .map_err(Error::Vcpu)?;
        #[cfg(target_arch = "x86_64")]
        {
            if let Some(pio_bus) = pio_bus {
                vcpu.set_pio_bus(pio_bus.clone());
            }
            vcpu.set_sipi_router(sipi_router.clone());
        }
        vcpus.push(vcpu);
    }
    Ok(vcpus)
}

#[cfg(all(target_arch = "aarch64", target_os = "linux"))]
fn create_vcpus_aarch64(
    vm: &Vm,
    vcpu_config: &VcpuConfig,
    mem_info: &ArchMemoryInfo,
    entry_addr: GuestAddress,
    exit_evt: &EventFd,
    metrics: utils::metrics::MetricsWriter,
) -> super::Result<Vec<Vcpu>> {
    // Create the full possible topology; CPUs beyond `vcpu_count` boot powered off
    // (KVM_ARM_VCPU_POWER_OFF) and wait for PSCI CPU_ON when the guest onlines them.
    let mut vcpus = Vec::with_capacity(vcpu_config.max_vcpu_count as usize);
    for cpu_index in 0..vcpu_config.max_vcpu_count {
        let mut vcpu = Vcpu::new_aarch64(
            cpu_index,
            vm.fd(),
            exit_evt.try_clone().map_err(Error::EventFd)?,
            metrics.clone(),
        )
        .map_err(Error::Vcpu)?;

        vcpu.configure_aarch64(vm.fd(), mem_info, entry_addr)
            .map_err(Error::Vcpu)?;

        vcpus.push(vcpu);
    }
    Ok(vcpus)
}

#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
#[allow(clippy::too_many_arguments)]
fn create_vcpus_aarch64(
    _vm: &Vm,
    vcpu_config: &VcpuConfig,
    mem_info: &ArchMemoryInfo,
    entry_addr: GuestAddress,
    exit_evt: &EventFd,
    vcpu_list: Arc<VcpuList>,
    nested_enabled: bool,
    metrics: utils::metrics::MetricsWriter,
) -> super::Result<Vec<Vcpu>> {
    // Create the full possible topology; CPUs beyond `vcpu_count` park on their boot
    // channel until a PSCI CPU_ON supplies an entry point when the guest onlines them.
    let mut vcpus = Vec::with_capacity(vcpu_config.max_vcpu_count as usize);
    let mut boot_senders: HashMap<u64, Sender<u64>> = HashMap::new();
    let mut parked_cpus: HashMap<u64, std::sync::atomic::AtomicBool> = HashMap::new();

    for cpu_index in 0..vcpu_config.max_vcpu_count {
        // Every CPU gets a boot channel so it can re-park and restart across
        // guest offline/online cycles; only vCPU 0 skips waiting at first boot.
        let (boot_sender, boot_receiver) = unbounded();

        let mut vcpu = Vcpu::new_aarch64(
            cpu_index,
            entry_addr,
            Some(boot_receiver),
            exit_evt.try_clone().map_err(Error::EventFd)?,
            vcpu_list.clone(),
            nested_enabled,
            metrics.clone(),
        )
        .map_err(Error::Vcpu)?;

        vcpu.configure_aarch64(mem_info).map_err(Error::Vcpu)?;

        boot_senders.insert(vcpu.get_mpidr(), boot_sender);
        parked_cpus.insert(
            vcpu.get_mpidr(),
            std::sync::atomic::AtomicBool::new(cpu_index != 0),
        );

        vcpus.push(vcpu);
    }

    // CPU_ON can be issued from any online vCPU and AFFINITY_INFO answered from
    // any thread, so every vCPU shares the relay and parked-state maps.
    let boot_senders = Arc::new(boot_senders);
    let parked_cpus = Arc::new(parked_cpus);
    for vcpu in &mut vcpus {
        vcpu.set_boot_senders(boot_senders.clone());
        vcpu.set_parked_cpus(parked_cpus.clone());
    }

    Ok(vcpus)
}

#[cfg(all(target_arch = "riscv64", target_os = "linux"))]
fn create_vcpus_riscv64(
    vm: &Vm,
    vcpu_config: &VcpuConfig,
    guest_mem: &GuestMemoryMmap,
    entry_addr: GuestAddress,
    exit_evt: &EventFd,
    metrics: utils::metrics::MetricsWriter,
) -> super::Result<Vec<Vcpu>> {
    let mut vcpus = Vec::with_capacity(vcpu_config.vcpu_count as usize);
    for cpu_index in 0..vcpu_config.vcpu_count {
        let mut vcpu = Vcpu::new_riscv64(
            cpu_index,
            vm.fd(),
            exit_evt.try_clone().map_err(Error::EventFd)?,
            metrics.clone(),
        )
        .map_err(Error::Vcpu)?;

        vcpu.configure_riscv64(vm.fd(), guest_mem, entry_addr)
            .map_err(Error::Vcpu)?;

        vcpus.push(vcpu);
    }
    Ok(vcpus)
}

/// Attaches an virtio mmio device to the device manager.
#[cfg_attr(target_os = "windows", allow(dead_code))]
fn attach_mmio_device(
    vmm: &mut Vmm,
    id: String,
    intc: IrqChip,
    device: Arc<Mutex<dyn VirtioDevice>>,
) -> std::result::Result<(), device_manager::mmio::Error> {
    let mmio_device = MmioTransport::new(vmm.guest_memory().clone(), intc, device)?;

    let type_id = mmio_device.locked_device().device_type();
    let _cmdline = &mut vmm.kernel_cmdline;

    #[cfg(target_os = "linux")]
    let (_mmio_base, _irq) =
        vmm.mmio_device_manager
            .register_mmio_device(vmm.vm.fd(), mmio_device, type_id, id)?;
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    let (_mmio_base, _irq) =
        vmm.mmio_device_manager
            .register_mmio_device(mmio_device, type_id, id)?;

    #[cfg(target_arch = "x86_64")]
    vmm.mmio_device_manager
        .add_device_to_cmdline(_cmdline, _mmio_base, _irq)?;

    Ok(())
}

#[cfg(not(feature = "tee"))]
fn attach_fs_devices(
    vmm: &mut Vmm,
    fs_devs: &[FsDeviceConfig],
    shm_manager: &mut ShmManager,
    #[cfg(not(feature = "tee"))] export_table: Option<ExportTable>,
    intc: IrqChip,
    exit_code: Arc<AtomicI32>,
    #[cfg(any(target_os = "macos", target_os = "windows"))] map_sender: Sender<WorkerMessage>,
) -> std::result::Result<(), StartMicrovmError> {
    use self::StartMicrovmError::*;

    for (i, config) in fs_devs.iter().enumerate() {
        let fs = Arc::new(Mutex::new(
            devices::virtio::Fs::new(
                config.fs_id.clone(),
                config.shared_dir.clone(),
                exit_code.clone(),
                config.allow_root_dir_delete,
            )
            .unwrap(),
        ));

        let id = format!("{}{}", String::from(fs.lock().unwrap().id()), i);

        if let Some(shm_region) = shm_manager.fs_region(i) {
            fs.lock().unwrap().set_shm_region(VirtioShmRegion {
                host_addr: vmm
                    .guest_memory
                    .get_host_address(shm_region.guest_addr)
                    .map_err(StartMicrovmError::ShmHostAddr)? as u64,
                guest_addr: shm_region.guest_addr.raw_value(),
                size: shm_region.size,
            });
        }

        #[cfg(not(feature = "tee"))]
        if let Some(export_table) = export_table.as_ref() {
            fs.lock().unwrap().set_export_table(export_table.clone());
        }

        #[cfg(any(target_os = "macos", target_os = "windows"))]
        fs.lock().unwrap().set_map_sender(map_sender.clone());

        // The device mutex mustn't be locked here otherwise it will deadlock.
        attach_mmio_device(vmm, id, intc.clone(), fs).map_err(RegisterFsDevice)?;
    }

    Ok(())
}

#[cfg(not(any(feature = "tee", feature = "aws-nitro")))]
fn attach_custom_fs_devices(
    vmm: &mut Vmm,
    custom_fs_devs: &[CustomFsDeviceConfig],
    shm_manager: &mut ShmManager,
    index_offset: usize,
    intc: IrqChip,
    exit_code: Arc<AtomicI32>,
    #[cfg(target_os = "macos")] map_sender: Sender<WorkerMessage>,
) -> std::result::Result<(), StartMicrovmError> {
    use self::StartMicrovmError::*;

    for (i, config) in custom_fs_devs.iter().enumerate() {
        let fs = Arc::new(Mutex::new(
            devices::virtio::Fs::with_custom_backend(
                config.fs_id.clone(),
                Arc::clone(&config.backend),
                exit_code.clone(),
            )
            .unwrap(),
        ));

        let id = format!(
            "{}{}",
            String::from(fs.lock().unwrap().id()),
            index_offset + i
        );

        let shm_index = index_offset + i;
        if let Some(shm_region) = shm_manager.fs_region(shm_index) {
            fs.lock().unwrap().set_shm_region(VirtioShmRegion {
                host_addr: vmm
                    .guest_memory
                    .get_host_address(shm_region.guest_addr)
                    .map_err(StartMicrovmError::ShmHostAddr)? as u64,
                guest_addr: shm_region.guest_addr.raw_value(),
                size: shm_region.size,
            });
        }

        #[cfg(target_os = "macos")]
        fs.lock().unwrap().set_map_sender(map_sender.clone());

        attach_mmio_device(vmm, id, intc.clone(), fs).map_err(RegisterFsDevice)?;
    }

    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn autoconfigure_console_ports(
    vmm: &mut Vmm,
    vm_resources: &VmResources,
    cfg: Option<&DefaultVirtioConsoleConfig>,
    creating_implicit_console: bool,
) -> std::result::Result<Vec<PortDescription>, StartMicrovmError> {
    use self::StartMicrovmError::*;

    let mut console_output_path: Option<PathBuf> = None;
    if let Some(path) = vm_resources.console_output.clone() {
        if !vm_resources.disable_implicit_console && creating_implicit_console {
            console_output_path = Some(path)
        }
    }

    if let Some(console_output_path) = console_output_path {
        let file = File::create(console_output_path).map_err(OpenConsoleFile)?;
        // Manually emulate our Legacy behavior: In the case of output_path we have always used the
        // stdin to determine the console size
        let stdin_fd = unsafe { BorrowedFd::borrow_raw(STDIN_FILENO) };
        let term_fd = if isatty(stdin_fd).is_ok_and(|v| v) {
            port_io::term_fd(stdin_fd.as_raw_fd()).unwrap()
        } else {
            port_io::term_fixed_size(0, 0)
        };
        Ok(vec![PortDescription::console(
            Some(port_io::input_empty().unwrap()),
            Some(port_io::output_file(file).unwrap()),
            term_fd,
        )])
    } else {
        let (input_fd, output_fd, err_fd) = match cfg {
            Some(c) => (c.input_fd, c.output_fd, c.err_fd),
            None => (STDIN_FILENO, STDOUT_FILENO, STDERR_FILENO),
        };
        let input_is_terminal =
            input_fd >= 0 && isatty(unsafe { BorrowedFd::borrow_raw(input_fd) }).unwrap_or(false);
        let output_is_terminal =
            output_fd >= 0 && isatty(unsafe { BorrowedFd::borrow_raw(output_fd) }).unwrap_or(false);
        let error_is_terminal =
            err_fd >= 0 && isatty(unsafe { BorrowedFd::borrow_raw(err_fd) }).unwrap_or(false);

        let term_fd = if input_is_terminal {
            Some(unsafe { BorrowedFd::borrow_raw(input_fd) })
        } else if output_is_terminal {
            Some(unsafe { BorrowedFd::borrow_raw(output_fd) })
        } else if error_is_terminal {
            Some(unsafe { BorrowedFd::borrow_raw(err_fd) })
        } else {
            None
        };

        let forwarding_sigint;
        let console_input = if input_is_terminal && input_fd >= 0 {
            forwarding_sigint = false;
            Some(port_io::input_to_raw_fd_dup(input_fd).unwrap())
        } else {
            #[cfg(target_os = "linux")]
            {
                forwarding_sigint = true;
                let sigint_input = port_io::PortInputSigInt::new();
                let sigint_input_fd = sigint_input.sigint_evt().as_raw_fd();
                register_sigint_handler(sigint_input_fd).map_err(RegisterFsSigwinch)?;
                Some(Box::new(sigint_input) as _)
            }
            #[cfg(not(target_os = "linux"))]
            {
                forwarding_sigint = false;
                Some(port_io::input_empty().unwrap())
            }
        };

        let console_output = if output_is_terminal && output_fd >= 0 {
            Some(port_io::output_to_raw_fd_dup(output_fd).unwrap())
        } else {
            Some(port_io::output_to_log_as_err())
        };

        let terminal_properties = term_fd
            .map(|fd| port_io::term_fd(fd.as_raw_fd()).unwrap())
            .unwrap_or_else(|| port_io::term_fixed_size(0, 0));

        setup_terminal_raw_mode(vmm, term_fd, forwarding_sigint);

        let mut ports = vec![PortDescription::console(
            console_input,
            console_output,
            terminal_properties,
        )];

        if input_fd >= 0 && !input_is_terminal {
            ports.push(PortDescription::input_pipe(
                "krun-stdin",
                port_io::input_to_raw_fd_dup(input_fd).unwrap(),
            ));
        }

        if output_fd >= 0 && !output_is_terminal {
            ports.push(PortDescription::output_pipe(
                "krun-stdout",
                port_io::output_to_raw_fd_dup(output_fd).unwrap(),
            ));
        };

        if err_fd >= 0 && !error_is_terminal {
            ports.push(PortDescription::output_pipe(
                "krun-stderr",
                port_io::output_to_raw_fd_dup(err_fd).unwrap(),
            ));
        }

        Ok(ports)
    }
}

#[cfg(not(target_os = "windows"))]
fn setup_terminal_raw_mode(
    vmm: &mut Vmm,
    term_fd: Option<BorrowedFd<'_>>,
    handle_signals_by_terminal: bool,
) {
    if let Some(term_fd) = term_fd {
        match term_set_raw_mode(term_fd, handle_signals_by_terminal) {
            Ok(old_mode) => {
                let raw_fd = term_fd.as_raw_fd();
                vmm.exit_observers.push(Arc::new(Mutex::new(move |_: i32| {
                    if let Err(e) =
                        term_restore_mode(unsafe { BorrowedFd::borrow_raw(raw_fd) }, &old_mode)
                    {
                        log::error!("Failed to restore terminal mode: {e}")
                    }
                })));
            }
            Err(e) => {
                log::error!("Failed to set terminal to raw mode: {e}")
            }
        };
    }
}

#[cfg(not(target_os = "windows"))]
fn create_explicit_ports(
    vmm: &mut Vmm,
    port_configs: Vec<PortConfig>,
) -> std::result::Result<Vec<PortDescription>, StartMicrovmError> {
    let mut ports = Vec::with_capacity(port_configs.len());

    for port_cfg in port_configs {
        let port_desc = match port_cfg {
            PortConfig::Tty { name, tty_fd } => {
                assert!(tty_fd > 0, "PortConfig::Tty must have a valid tty_fd");
                let term_fd = unsafe { BorrowedFd::borrow_raw(tty_fd) };
                setup_terminal_raw_mode(vmm, Some(term_fd), false);

                PortDescription {
                    name: name.into(),
                    input: Some(port_io::input_to_raw_fd_dup(tty_fd).unwrap()),
                    output: Some(port_io::output_to_raw_fd_dup(tty_fd).unwrap()),
                    terminal: Some(port_io::term_fd(tty_fd).unwrap()),
                }
            }
            PortConfig::InOut {
                name,
                input_fd,
                output_fd,
            } => PortDescription {
                name: name.into(),
                input: if input_fd < 0 {
                    None
                } else {
                    Some(port_io::input_to_raw_fd_dup(input_fd).unwrap())
                },
                output: if output_fd < 0 {
                    None
                } else {
                    Some(port_io::output_to_raw_fd_dup(output_fd).unwrap())
                },
                terminal: None,
            },
            PortConfig::Custom {
                name,
                input,
                output,
            } => PortDescription {
                name: name.into(),
                input: Some(input),
                output: Some(output),
                terminal: None,
            },
        };

        ports.push(port_desc);
    }

    Ok(ports)
}

#[cfg(target_os = "windows")]
fn create_explicit_ports(
    port_configs: Vec<PortConfig>,
) -> std::result::Result<Vec<PortDescription>, StartMicrovmError> {
    use self::StartMicrovmError::*;

    let mut ports = Vec::with_capacity(port_configs.len());

    for port_cfg in port_configs {
        let port_desc = match port_cfg {
            PortConfig::ConsoleOutputFile { path } => {
                let file = File::create(path).map_err(OpenConsoleFile)?;

                PortDescription::console(
                    Some(port_io::input_empty().unwrap()),
                    Some(port_io::output_file(file).unwrap()),
                    port_io::term_fixed_size(0, 0),
                )
            }
            PortConfig::NamedPipe { name, pipe_name } => {
                let (input, output) = port_io::named_pipe(&pipe_name).map_err(OpenConsolePipe)?;

                PortDescription {
                    name: name.into(),
                    input: Some(input),
                    output: Some(output),
                    terminal: None,
                }
            }
        };

        ports.push(port_desc);
    }

    Ok(ports)
}

#[cfg(not(target_os = "windows"))]
fn attach_console_devices(
    vmm: &mut Vmm,
    event_manager: &mut EventManager,
    intc: IrqChip,
    vm_resources: &VmResources,
    cfg: Option<VirtioConsoleConfigMode>,
    id_number: u32,
) -> std::result::Result<(), StartMicrovmError> {
    use self::StartMicrovmError::*;

    let creating_implicit_console = cfg.is_none();

    let ports = match cfg {
        None => autoconfigure_console_ports(vmm, vm_resources, None, creating_implicit_console)?,
        Some(VirtioConsoleConfigMode::Autoconfigure(autocfg)) => autoconfigure_console_ports(
            vmm,
            vm_resources,
            Some(&autocfg),
            creating_implicit_console,
        )?,
        Some(VirtioConsoleConfigMode::Explicit(ports)) => create_explicit_ports(vmm, ports)?,
    };

    let console = Arc::new(Mutex::new(devices::virtio::Console::new(ports).unwrap()));

    vmm.exit_observers.push(console.clone());

    event_manager
        .add_subscriber(console.clone())
        .map_err(RegisterEvent)?;

    #[cfg(target_os = "linux")]
    register_sigwinch_handler(console.lock().unwrap().get_sigwinch_fd())
        .map_err(RegisterFsSigwinch)?;

    // The device mutex mustn't be locked here otherwise it will deadlock.
    attach_mmio_device(vmm, format!("hvc{id_number}"), intc, console)
        .map_err(RegisterConsoleDevice)?;

    Ok(())
}

#[cfg(target_os = "windows")]
fn attach_console_devices(
    vmm: &mut Vmm,
    event_manager: &mut EventManager,
    intc: IrqChip,
    cfg: VirtioConsoleConfigMode,
    id_number: u32,
) -> std::result::Result<(), StartMicrovmError> {
    use self::StartMicrovmError::*;

    let ports = match cfg {
        VirtioConsoleConfigMode::Explicit(ports) => create_explicit_ports(ports)?,
    };

    let console = Arc::new(Mutex::new(devices::virtio::Console::new(ports).unwrap()));

    vmm.exit_observers.push(console.clone());

    event_manager
        .add_subscriber(console.clone())
        .map_err(RegisterEvent)?;

    attach_mmio_device(vmm, format!("hvc{id_number}"), intc, console)
        .map_err(RegisterConsoleDevice)?;

    Ok(())
}

#[cfg(feature = "net")]
fn attach_net_devices(
    vmm: &mut Vmm,
    net_devices: &NetBuilder,
    intc: IrqChip,
) -> Result<(), StartMicrovmError> {
    for net_device in net_devices.list.iter() {
        let id = net_device.lock().unwrap().id().to_string();

        attach_mmio_device(vmm, id, intc.clone(), net_device.clone())
            .map_err(StartMicrovmError::RegisterNetDevice)?;
    }
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn attach_unixsock_vsock_device(
    vmm: &mut Vmm,
    unix_vsock: &Arc<Mutex<Vsock>>,
    event_manager: &mut EventManager,
    intc: IrqChip,
) -> std::result::Result<(), StartMicrovmError> {
    use self::StartMicrovmError::*;

    event_manager
        .add_subscriber(unix_vsock.clone())
        .map_err(RegisterEvent)?;

    let id = String::from(unix_vsock.lock().unwrap().id());

    // The device mutex mustn't be locked here otherwise it will deadlock.
    attach_mmio_device(vmm, id, intc, unix_vsock.clone()).map_err(RegisterVsockDevice)?;

    Ok(())
}

#[cfg(not(feature = "tee"))]
fn attach_msb_metrics_device(
    vmm: &mut Vmm,
    event_manager: &mut EventManager,
    intc: IrqChip,
    metrics: utils::metrics::MetricsWriter,
) -> std::result::Result<(), StartMicrovmError> {
    use self::StartMicrovmError::*;

    let device = Arc::new(Mutex::new(
        devices::virtio::MsbMetrics::new(metrics).unwrap(),
    ));

    event_manager
        .add_subscriber(device.clone())
        .map_err(RegisterEvent)?;

    let id = String::from(device.lock().unwrap().id());

    attach_mmio_device(vmm, id, intc, device).map_err(RegisterMsbMetricsDevice)?;

    Ok(())
}

#[cfg(not(feature = "tee"))]
fn attach_balloon_device(
    vmm: &mut Vmm,
    event_manager: &mut EventManager,
    intc: IrqChip,
    metrics: utils::metrics::MetricsWriter,
    stats_polling_interval: Option<std::time::Duration>,
) -> std::result::Result<(), StartMicrovmError> {
    use self::StartMicrovmError::*;

    let balloon = Arc::new(Mutex::new(
        devices::virtio::Balloon::new(metrics, stats_polling_interval).unwrap(),
    ));

    event_manager
        .add_subscriber(balloon.clone())
        .map_err(RegisterEvent)?;

    let id = String::from(balloon.lock().unwrap().id());

    // The device mutex mustn't be locked here otherwise it will deadlock.
    attach_mmio_device(vmm, id, intc.clone(), balloon).map_err(RegisterBalloonDevice)?;

    Ok(())
}

#[cfg(not(feature = "tee"))]
fn attach_cpu_device(
    vmm: &mut Vmm,
    event_manager: &mut EventManager,
    intc: IrqChip,
    cpu_device: Arc<Mutex<devices::virtio::Cpu>>,
) -> std::result::Result<(), StartMicrovmError> {
    use self::StartMicrovmError::*;

    event_manager
        .add_subscriber(cpu_device.clone())
        .map_err(RegisterEvent)?;

    let id = String::from(cpu_device.lock().unwrap().id());

    // The device mutex mustn't be locked here otherwise it will deadlock.
    attach_mmio_device(vmm, id, intc.clone(), cpu_device).map_err(RegisterBalloonDevice)?;

    Ok(())
}

#[cfg(not(feature = "tee"))]
fn attach_mem_device(
    vmm: &mut Vmm,
    event_manager: &mut EventManager,
    intc: IrqChip,
    mem_device: Arc<Mutex<devices::virtio::Mem>>,
) -> std::result::Result<(), StartMicrovmError> {
    use self::StartMicrovmError::*;

    event_manager
        .add_subscriber(mem_device.clone())
        .map_err(RegisterEvent)?;

    let id = String::from(mem_device.lock().unwrap().id());

    // The device mutex mustn't be locked here otherwise it will deadlock.
    attach_mmio_device(vmm, id, intc.clone(), mem_device).map_err(RegisterBalloonDevice)?;

    Ok(())
}

#[cfg(feature = "blk")]
fn attach_block_devices(
    vmm: &mut Vmm,
    block_devs: &BlockBuilder,
    intc: IrqChip,
) -> std::result::Result<(), StartMicrovmError> {
    use self::StartMicrovmError::*;

    for block in block_devs.list.iter() {
        let id = String::from(block.lock().unwrap().id());

        // The device mutex mustn't be locked here otherwise it will deadlock.
        attach_mmio_device(vmm, id, intc.clone(), block.clone()).map_err(RegisterBlockDevice)?;
    }

    Ok(())
}

#[cfg(all(not(feature = "tee"), not(target_os = "windows")))]
fn attach_rng_device(
    vmm: &mut Vmm,
    event_manager: &mut EventManager,
    intc: IrqChip,
) -> std::result::Result<(), StartMicrovmError> {
    use self::StartMicrovmError::*;

    let rng = Arc::new(Mutex::new(devices::virtio::Rng::new().unwrap()));

    event_manager
        .add_subscriber(rng.clone())
        .map_err(RegisterEvent)?;

    let id = String::from(rng.lock().unwrap().id());

    // The device mutex mustn't be locked here otherwise it will deadlock.
    attach_mmio_device(vmm, id, intc.clone(), rng).map_err(RegisterRngDevice)?;

    Ok(())
}

#[cfg(feature = "gpu")]
#[allow(clippy::too_many_arguments)]
fn attach_gpu_device(
    vmm: &mut Vmm,
    shm_manager: &mut ShmManager,
    #[cfg(not(feature = "tee"))] mut export_table: Option<ExportTable>,
    intc: IrqChip,
    virgl_flags: u32,
    displays: Box<[DisplayInfo]>,
    display_backend: DisplayBackend<'static>,
    #[cfg(target_os = "macos")] map_sender: Sender<WorkerMessage>,
) -> std::result::Result<(), StartMicrovmError> {
    use self::StartMicrovmError::*;

    let gpu = Arc::new(Mutex::new(
        devices::virtio::Gpu::new(
            virgl_flags,
            displays,
            display_backend,
            #[cfg(target_os = "macos")]
            map_sender,
        )
        .unwrap(),
    ));

    let id = String::from(gpu.lock().unwrap().id());

    if let Some(shm_region) = shm_manager.gpu_region() {
        gpu.lock().unwrap().set_shm_region(VirtioShmRegion {
            host_addr: vmm
                .guest_memory
                .get_host_address(shm_region.guest_addr)
                .map_err(StartMicrovmError::ShmHostAddr)? as u64,
            guest_addr: shm_region.guest_addr.raw_value(),
            size: shm_region.size,
        });
    }

    #[cfg(not(feature = "tee"))]
    if let Some(export_table) = export_table.take() {
        gpu.lock().unwrap().set_export_table(export_table);
    }

    // The device mutex mustn't be locked here otherwise it will deadlock.
    attach_mmio_device(vmm, id, intc, gpu).map_err(RegisterGpuDevice)?;

    Ok(())
}

#[cfg(feature = "input")]
fn attach_input_devices(
    vmm: &mut Vmm,
    input_backends: &[(
        krun_input::InputConfigBackend<'static>,
        krun_input::InputEventProviderBackend<'static>,
    )],
    intc: IrqChip,
) -> std::result::Result<(), StartMicrovmError> {
    use self::StartMicrovmError::*;

    for (index, (config_backend, events_backend)) in input_backends.iter().enumerate() {
        let input_device = Arc::new(Mutex::new(
            devices::virtio::input::Input::new(*config_backend, *events_backend).unwrap(),
        ));

        let id = format!("input{}", index);
        attach_mmio_device(vmm, id, intc.clone(), input_device).map_err(RegisterInputDevice)?;
    }

    Ok(())
}

#[cfg(feature = "snd")]
fn attach_snd_device(vmm: &mut Vmm, intc: IrqChip) -> std::result::Result<(), StartMicrovmError> {
    use self::StartMicrovmError::*;

    let snd = Arc::new(Mutex::new(devices::virtio::Snd::new().unwrap()));
    let id = String::from(snd.lock().unwrap().id());

    // The device mutex mustn't be locked here otherwise it will deadlock.
    attach_mmio_device(vmm, id, intc, snd).map_err(RegisterSndDevice)?;

    Ok(())
}

#[cfg(test)]
pub mod tests {
    use super::*;
    #[cfg(not(target_os = "windows"))]
    use crate::vmm_config::kernel_bundle::KernelBundle;

    #[test]
    #[cfg(all(target_arch = "x86_64", not(target_os = "windows")))]
    fn image_bz2_skips_false_header_candidates() {
        use std::io::Write;

        const KERNEL_DATA: &[u8] = b"\x7fELFminimal-kernel";

        let mut encoder = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::default());
        encoder.write_all(KERNEL_DATA).unwrap();
        let compressed_kernel = encoder.finish().unwrap();

        let mut image = b"Image-prefix-BZh0-not-a-stream".to_vec();
        image.extend_from_slice(&compressed_kernel);

        assert_eq!(decode_image_bz2_kernel(&image).unwrap(), KERNEL_DATA);
    }

    #[cfg(not(target_os = "windows"))]
    fn default_guest_memory(
        mem_size_mib: usize,
    ) -> std::result::Result<
        (GuestMemoryMmap, ArchMemoryInfo, ShmManager, PayloadConfig),
        StartMicrovmError,
    > {
        let mut vm_resources = VmResources::default();
        vm_resources.kernel_bundle = Some(KernelBundle {
            host_addr: 0x1000,
            guest_addr: 0x1000,
            entry_addr: 0x1000,
            size: 0x1000,
        });

        create_guest_memory(mem_size_mib, &vm_resources, &Payload::Empty)
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn windows_explicit_console_output_file_creates_console_port() {
        let path = std::env::temp_dir().join(format!(
            "msb-krun-vmm-console-output-{}.log",
            std::process::id()
        ));

        let ports =
            create_explicit_ports(vec![PortConfig::ConsoleOutputFile { path: path.clone() }])
                .unwrap();

        assert_eq!(ports.len(), 1);
        assert!(ports[0].name.is_empty());
        assert!(ports[0].input.is_some());
        assert!(ports[0].output.is_some());
        assert!(ports[0].terminal.is_some());

        let _ = std::fs::remove_file(path);
    }

    #[test]
    #[cfg(all(target_arch = "x86_64", not(target_os = "windows")))]
    fn test_create_vcpus_x86_64() {
        let vcpu_count = 2;

        let vcpu_config = VcpuConfig {
            vcpu_count,
            max_vcpu_count: vcpu_count,
            ht_enabled: false,
            cpu_template: None,
        };

        let (guest_memory, _arch_memory_info, _shm_manager, _payload_config) =
            default_guest_memory(128).unwrap();
        let vm = setup_vm(&guest_memory, false, vcpu_config.vcpu_count).unwrap();
        let _kvmioapic = KvmIoapic::new(&vm.fd()).unwrap();

        // Dummy entry_addr, vcpus will not boot.
        let entry_addr = GuestAddress(0);
        let bus = devices::Bus::new();
        let vcpu_vec = create_vcpus_x86_64(
            &vm,
            &vcpu_config,
            &guest_memory,
            entry_addr,
            &bus,
            &EventFd::new(utils::eventfd::EFD_NONBLOCK).unwrap(),
            true,
            utils::metrics::MetricsWriter::default(),
        )
        .unwrap();
        assert_eq!(vcpu_vec.len(), vcpu_count as usize);
    }

    #[test]
    #[cfg(all(target_arch = "aarch64", target_os = "linux"))]
    fn test_create_vcpus_aarch64() {
        let (guest_memory, arch_memory_info, _shm_manager, _payload_config) =
            default_guest_memory(128).unwrap();
        let vm = setup_vm(&guest_memory, false, vcpu_config.vcpu_count).unwrap();
        let vcpu_count = 2;

        let vcpu_config = VcpuConfig {
            vcpu_count,
            max_vcpu_count: vcpu_count,
            ht_enabled: false,
            cpu_template: None,
        };

        // Dummy entry_addr, vcpus will not boot.
        let entry_addr = GuestAddress(0);
        let vcpu_vec = create_vcpus_aarch64(
            &vm,
            &vcpu_config,
            &arch_memory_info,
            entry_addr,
            &EventFd::new(utils::eventfd::EFD_NONBLOCK).unwrap(),
            utils::metrics::MetricsWriter::default(),
        )
        .unwrap();
        assert_eq!(vcpu_vec.len(), vcpu_count as usize);
    }

    #[test]
    fn test_error_messages() {
        use crate::builder::StartMicrovmError::*;
        let err = AttachBlockDevice(io::Error::from_raw_os_error(0));
        let _ = format!("{err}{err:?}");

        let err = CreateRateLimiter(io::Error::from_raw_os_error(0));
        let _ = format!("{err}{err:?}");

        let err = Internal(Error::Serial(io::Error::from_raw_os_error(0)));
        let _ = format!("{err}{err:?}");

        #[cfg(not(target_os = "windows"))]
        {
            let err = InvalidKernelBundle(vm_memory::mmap::MmapRegionError::InvalidPointer);
            let _ = format!("{err}{err:?}");
        }

        let err = KernelCmdline(String::from("dummy --cmdline"));
        let _ = format!("{err}{err:?}");

        let err = LoadCommandline(kernel::cmdline::Error::TooLarge);
        let _ = format!("{err}{err:?}");

        let err = MicroVMAlreadyRunning;
        let _ = format!("{err}{err:?}");

        let err = MissingKernelConfig;
        let _ = format!("{err}{err:?}");

        let err = MissingMemSizeConfig;
        let _ = format!("{err}{err:?}");

        let err = NetDeviceNotConfigured;
        let _ = format!("{err}{err:?}");

        let err = OpenBlockDevice(io::Error::from_raw_os_error(0));
        let _ = format!("{err}{err:?}");

        let err = RegisterBlockDevice(device_manager::mmio::Error::EventFd(
            io::Error::from_raw_os_error(0),
        ));
        let _ = format!("{err}{err:?}");

        let err = RegisterEvent(EventManagerError::EpollCreate(
            io::Error::from_raw_os_error(0),
        ));
        let _ = format!("{err}{err:?}");

        let err = RegisterNetDevice(device_manager::mmio::Error::EventFd(
            io::Error::from_raw_os_error(0),
        ));
        let _ = format!("{err}{err:?}");

        let err = RegisterVsockDevice(device_manager::mmio::Error::EventFd(
            io::Error::from_raw_os_error(0),
        ));
        let _ = format!("{err}{err:?}");
    }

    #[test]
    fn test_kernel_cmdline_err_to_startuvm_err() {
        let err = StartMicrovmError::from(kernel::cmdline::Error::HasSpace);
        let _ = format!("{err}{err:?}");
    }
}
