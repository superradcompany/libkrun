//! msb_krun - Native Rust API for libkrun microVMs.
//!
//! This crate provides a builder-pattern API for creating and entering microVMs
//! using libkrun's VMM infrastructure.
//!
//! # Lifecycle
//!
//! [`Vm::enter()`] never returns on success. When the guest shuts down, the
//! VMM calls `_exit()`, killing the entire process. `enter()` only returns
//! `Err` if something fails before the VMM takes over.
//!
//! # Example
//!
//! ```rust,no_run
//! use msb_krun::{VmBuilder, Result};
//!
//! fn main() -> Result<()> {
//!     VmBuilder::new()
//!         .machine(|m| m.vcpus(4).memory_mib(2048))
//!         .fs(|fs| fs.root("/path/to/rootfs"))
//!         .exec(|e| e.path("/bin/myapp").args(["--flag"]).env("HOME", "/root"))
//!         .build()?
//!         .enter()?;
//!
//!     unreachable!()
//! }
//! ```
//!
//! # Embedded kernels (`static-krunfw`)
//!
//! By default the guest kernel is loaded from the `libkrunfw` shared library at
//! runtime (`dlopen`, or the platform equivalent). Targets without a dynamic
//! loader — a fully static musl binary, for example — can instead link the
//! kernel in by enabling the `static-krunfw` feature; the accessor is then
//! resolved at link time and the embedding program supplies the kernel image.
//!
//! ## The embedder callback
//!
//! With `static-krunfw` the embedder must export a C-ABI `krunfw_get_kernel`:
//!
//! ```c
//! // Selects the image named by `path` (NUL-terminated, or NULL to select the
//! // default image when no `krunfw_path` was configured), writes its guest
//! // physical address, entry point and size to the out-parameters, and returns
//! // its host address. Returns NULL if `path` names no image.
//! char *krunfw_get_kernel(const char *path,
//!                         uint64_t *guest_addr,
//!                         uint64_t *entry_addr,
//!                         size_t *size);
//! ```
//!
//! The addresses and size are properties of the image, not the API: an x86_64
//! `vmlinux` image loads at `0x1000000` with the ELF entry point, while an
//! aarch64 or riscv64 `Image` loads and enters at `0x80000000`.
//!
//! A minimal Rust implementation, embedding one image (the path is ignored):
//!
//! ```rust,no_run
//! use std::os::raw::c_char;
//!
//! // The kernel image. It must be 64 KiB-aligned and writable: see "Alignment
//! // and writability" below. `include_bytes!` of a build-script output is
//! // typical.
//! #[repr(align(65536))]
//! struct Aligned([u8; 0x10_0000]);
//! static mut KERNEL: Aligned = Aligned([0; 0x10_0000]);
//!
//! #[no_mangle]
//! pub unsafe extern "C" fn krunfw_get_kernel(
//!     _path: *const c_char,
//!     guest_addr: *mut u64,
//!     entry_addr: *mut u64,
//!     size: *mut usize,
//! ) -> *mut c_char {
//!     let image = std::ptr::addr_of_mut!(KERNEL);
//!     unsafe {
//!         *guest_addr = 0x10_0000;
//!         *entry_addr = 0x10_0000;
//!         *size = std::mem::size_of_val(&*image);
//!     }
//!     image.cast()
//! }
//! ```
//!
//! ## Selecting an image
//!
//! [`KernelBuilder::krunfw_path`] sets `path`: `None` (the default) passes
//! `NULL`, and a path passes it as a NUL-terminated string. An embedder that
//! ships several images can convert it back to a [`Path`] and match on it; one
//! that embeds a single image can ignore it, as above.
//!
//! ```rust,no_run
//! use std::ffi::{CStr, OsStr};
//! use std::os::raw::c_char;
//! use std::path::Path;
//!
//! // The embedded images, 64 KiB-aligned and writable (see "Alignment and
//! // writability" below). They need not be the same size.
//! #[repr(align(65536))]
//! struct Image<const N: usize>([u8; N]);
//! static mut DEFAULT_IMAGE: Image<0x0c0_000> = Image([0; 0x0c0_000]);
//! static mut ALT_IMAGE: Image<0x10_0000> = Image([0; 0x10_0000]);
//!
//! /// Fills the out-parameters for `image` and returns its host address.
//! unsafe fn select<const N: usize>(
//!     image: *mut Image<N>,
//!     guest: u64,
//!     entry: u64,
//!     guest_addr: *mut u64,
//!     entry_addr: *mut u64,
//!     size: *mut usize,
//! ) -> *mut c_char {
//!     unsafe {
//!         *guest_addr = guest;
//!         *entry_addr = entry;
//!         *size = std::mem::size_of_val(&*image);
//!     }
//!     image.cast()
//! }
//!
//! #[no_mangle]
//! pub unsafe extern "C" fn krunfw_get_kernel(
//!     path: *const c_char,
//!     guest_addr: *mut u64,
//!     entry_addr: *mut u64,
//!     size: *mut usize,
//! ) -> *mut c_char {
//!     let path = if path.is_null() {
//!         None // `krunfw_path` was not configured: use the default image
//!     } else {
//!         // SAFETY: the host passes a NUL-terminated string valid for the call.
//!         let bytes = unsafe { CStr::from_ptr(path) }.to_bytes();
//!         // SAFETY: the host encoded the path with `OsStr::as_encoded_bytes`.
//!         Some(Path::new(unsafe { OsStr::from_encoded_bytes_unchecked(bytes) }))
//!     };
//!     unsafe {
//!         match path {
//!             None => select(
//!                 std::ptr::addr_of_mut!(DEFAULT_IMAGE),
//!                 0x10_0000,
//!                 0x10_0000,
//!                 guest_addr,
//!                 entry_addr,
//!                 size,
//!             ),
//!             Some(p) if p == Path::new("alt") => select(
//!                 std::ptr::addr_of_mut!(ALT_IMAGE),
//!                 0x8000_0000,
//!                 0x8000_0000,
//!                 guest_addr,
//!                 entry_addr,
//!                 size,
//!             ),
//!             _ => std::ptr::null_mut(), // no such image
//!         }
//!     }
//! }
//! ```
//!
//! ## Alignment and writability
//!
//! The image must be aligned to the host page size. The VMM checks
//! `utils::page_size()` (`sysconf(_SC_PAGESIZE)` on Unix, the system page size
//! on Windows), which is **not constant**: 4 KiB on x86_64, 16 KiB on Apple
//! Silicon, and 4/16/64 KiB on aarch64 Linux depending on the kernel's base
//! page size. libkrunfw generates `KERNEL_BUNDLE` with `aligned(65536)` on
//! Linux and macOS ("64k covers 4k/16k/64k Linux kernels") and `aligned(4096)`
//! on Windows (WHP uses 4 KiB pages); align to **64 KiB** to cover every target,
//! as the example above does with `#[repr(align(65536))]`. A type's size is
//! always a multiple of its alignment, so `size_of_val` of the aligned wrapper
//! is already rounded up to 64 KiB; report that (not the unpadded image length)
//! so the size reaches the VMM page-aligned, as libkrunfw's own bundles are.
//!
//! The image must also be **writable**: the guest may write to the kernel
//! pages, so a buffer in read-only memory (`.rodata`) can fault. Put it in a
//! `static mut` (or another writable, 64 KiB-aligned allocation).
//!
//! ## Kernel memory lifetime
//!
//! The host address returned by the accessor is mapped into the guest, so the
//! kernel bytes must stay valid, aligned and writable for the lifetime of the
//! [`Vm`] — and, since [`Vm::enter`] never returns, in practice for the rest of
//! the process. Keep them in a `static`/`static mut`, or in an allocation that
//! is never freed; a buffer dropped after [`VmBuilder::build`] is not enough.
//! When `static-krunfw` is disabled the crate keeps the `libkrunfw` mapping
//! alive itself.

//--------------------------------------------------------------------------------------------------
// Modules
//--------------------------------------------------------------------------------------------------

pub mod api;
pub mod backends;

//--------------------------------------------------------------------------------------------------
// Re-Exports
//--------------------------------------------------------------------------------------------------

pub use api::builder::VmBuilder;
#[cfg(feature = "blk")]
pub use api::builders::CacheMode;
#[cfg(feature = "blk")]
pub use api::builders::DiskBuilder;
#[cfg(feature = "blk")]
pub use api::builders::DiskImageFormat;
#[cfg(feature = "blk")]
pub use api::builders::DiskLayer;
pub use api::builders::FsBuilder;
#[cfg(feature = "net")]
pub use api::builders::NetBuilder;
#[cfg(feature = "blk")]
pub use api::builders::SyncMode;
pub use api::builders::VsockBuilder;
#[cfg(feature = "blk")]
pub use api::builders::WritebackLimit;
pub use api::builders::{
    ConsoleBuilder, ConsolePortOptions, ExecBuilder, HostCpuId, HostMemoryPolicy, KernelBuilder,
    MachineBuilder, MemoryPlacementResult, NumaBuilder, NumaDistance, NumaNodeBuilder,
    NumaNodeConfig, NumaTopology, PlacementReport, VcpuPlacementResult,
};
pub use api::error::{BuildError, ConfigError, Error, Result, RuntimeError};
pub use api::exit_handle::ExitHandle;
pub use api::metrics::{
    BlockDeviceMetrics, BlockMetrics, CpuMetrics, FilesystemMetrics, MemoryMetrics, MetricsHandle,
    VmMetrics,
};
pub use api::vm::Vm;
#[cfg(not(feature = "tee"))]
pub use api::vm::{
    VmControl, VmCpuState, VmExecutionState, VmGenerationId, VmGenerationRequest,
    VmGenerationState, VmGenerationWaitOutcome, VmMemoryRestoreSource, VmMemoryRestoreTarget,
    VmMemoryState, VmPauseGeneration,
};
#[cfg(feature = "blk")]
pub use api::{
    BlockBackendSpec, BlockImageFormat, BlockLayerSpec, BlockSyncMode, PreparedBlockBackend,
};
#[cfg(all(feature = "blk", not(feature = "tee")))]
pub use api::{BlockDeviceState, VirtioDeviceState};
#[cfg(not(feature = "tee"))]
pub use api::{
    ExecutionArchitecture, ExecutionBackend, ExecutionState, FullCaptureReason, GuestMemoryRange,
    IncrementalCaptureDecision, MemoryBaselineToken, MemoryCaptureKind, MemoryCaptureOptions,
    MemoryCapturePlan, MemoryCaptureSink, MemoryCaptureStats, MemoryGeneration,
    MemoryTopologyGeneration, VcpuExecutionState,
};
#[cfg(not(feature = "tee"))]
pub use api::{PrivateMemoryBacking, PrivateMemoryRegion};
#[cfg(feature = "net")]
pub use devices::virtio::net::rate_limit::{
    RateLimiterConfig, RateLimiterConfigError, TokenBucketConfig,
};

#[cfg(not(target_os = "windows"))]
pub use backends::console::ConsolePortBackend;

#[cfg(not(any(feature = "tee", feature = "aws-nitro")))]
pub use backends::fs::DynFileSystem;

#[cfg(feature = "net")]
pub use backends::net::NetBackend;

pub use backends::vsock::{
    VsockConnectRequest, VsockNotifier, VsockPortBackend, VsockShutdown, VsockStreamBackend,
};
