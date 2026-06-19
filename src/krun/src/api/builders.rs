//! Sub-builders for VmBuilder nested configuration.

use std::path::{Path, PathBuf};
use std::time::Duration;

#[cfg(not(target_os = "windows"))]
use devices::virtio::console::port_io::{
    ConsolePortBackend, ConsolePortBackendInputAdapter, ConsolePortBackendOutputAdapter,
};
#[cfg(not(target_os = "windows"))]
use vmm::resources::PortConfig;

#[cfg(all(
    not(any(feature = "tee", feature = "aws-nitro")),
    not(target_os = "windows")
))]
use crate::backends::fs::DynFileSystem;

#[cfg(feature = "net")]
use std::os::fd::OwnedFd;
#[cfg(not(target_os = "windows"))]
use std::os::fd::RawFd;
#[cfg(not(target_os = "windows"))]
use std::sync::Arc;

#[cfg(feature = "net")]
use crate::backends::net::NetBackend;

//--------------------------------------------------------------------------------------------------
// Types: Machine Builder
//--------------------------------------------------------------------------------------------------

/// Builder for machine configuration (vCPUs, memory, etc.).
///
/// # Example
///
/// ```rust,no_run
/// # use msb_krun::VmBuilder;
/// VmBuilder::new()
///     .machine(|m| {
///         m.vcpus(4)
///             .memory_mib(2048)
///             .hyperthreading(true)
///             .nested_virt(true)
///     });
/// ```
#[derive(Debug, Clone)]
pub struct MachineBuilder {
    pub(crate) vcpus: u8,
    pub(crate) memory_mib: usize,
    pub(crate) hyperthreading: bool,
    pub(crate) nested_virt: bool,
    pub(crate) split_irqchip: bool,
    pub(crate) vsock: bool,
    pub(crate) balloon: bool,
    pub(crate) balloon_stats_interval: Option<Duration>,
    pub(crate) rng: bool,
    pub(crate) enable_inet_hijack: bool,
}

//--------------------------------------------------------------------------------------------------
// Types: Kernel Builder
//--------------------------------------------------------------------------------------------------

/// Builder for kernel configuration.
///
/// # Example
///
/// ```rust,no_run
/// # use msb_krun::VmBuilder;
/// VmBuilder::new()
///     .kernel(|k| {
///         k.krunfw_path("/path/to/libkrunfw.dylib")
///             .cmdline("debug")
///     });
/// ```
#[derive(Debug, Clone, Default)]
pub struct KernelBuilder {
    pub(crate) cmdline: Option<String>,
    pub(crate) krunfw_path: Option<PathBuf>,
    pub(crate) init_path: Option<String>,
}

//--------------------------------------------------------------------------------------------------
// Types: Filesystem Builder
//--------------------------------------------------------------------------------------------------

/// Builder for filesystem configuration.
///
/// # Examples
///
/// Root filesystem only:
///
/// ```rust,no_run
/// # use msb_krun::VmBuilder;
/// VmBuilder::new()
///     .fs(|fs| fs.root("/path/to/rootfs"));
/// ```
///
/// Root filesystem with additional named mounts:
///
/// ```rust,no_run
/// # use msb_krun::VmBuilder;
/// VmBuilder::new()
///     .fs(|fs| fs.root("/path/to/rootfs"))
///     .fs(|fs| fs.tag("data").shm_size(1 << 30).path("/host/data"))
///     .fs(|fs| fs.tag("logs").path("/host/logs"));
/// ```
///
/// Custom filesystem backend:
///
/// ```rust,ignore
/// VmBuilder::new()
///     .fs(|fs| fs.tag("myfs").custom(Box::new(my_backend)));
/// ```
pub struct FsBuilder {
    pub(crate) configs: Vec<FsConfig>,
    current_tag: Option<String>,
    current_shm_size: Option<usize>,
}

/// Configuration for a single filesystem mount.
pub enum FsConfig {
    /// Path-based filesystem (passthrough).
    Path {
        tag: String,
        path: PathBuf,
        shm_size: Option<usize>,
    },
    /// Custom filesystem backend.
    #[cfg(all(
        not(any(feature = "tee", feature = "aws-nitro")),
        not(target_os = "windows")
    ))]
    Custom {
        tag: String,
        backend: Box<dyn DynFileSystem + Send + Sync>,
    },
}

//--------------------------------------------------------------------------------------------------
// Types: Network Builder
//--------------------------------------------------------------------------------------------------

/// Builder for network configuration.
///
/// # Example
///
/// ```rust,ignore
/// VmBuilder::new()
///     .net(|n| n.mac([0x52, 0x54, 0x00, 0x12, 0x34, 0x56]).custom(my_backend));
/// ```
#[cfg(feature = "net")]
pub struct NetBuilder {
    pub(crate) configs: Vec<NetConfig>,
    current_mac: Option<[u8; 6]>,
}

/// Configuration for a single network device.
#[cfg(feature = "net")]
pub enum NetConfig {
    /// Unixgram backend from a pre-opened fd.
    UnixgramFd { mac: Option<[u8; 6]>, fd: OwnedFd },
    /// Unixgram backend connecting to a socket path.
    UnixgramPath {
        mac: Option<[u8; 6]>,
        path: PathBuf,
        send_vfkit_magic: bool,
    },
    /// Unixstream backend from a pre-opened fd.
    UnixstreamFd { mac: Option<[u8; 6]>, fd: OwnedFd },
    /// Unixstream backend connecting to a socket path.
    UnixstreamPath { mac: Option<[u8; 6]>, path: PathBuf },
    /// TAP backend (Linux only).
    #[cfg(target_os = "linux")]
    Tap { mac: Option<[u8; 6]>, name: String },
    /// Custom network backend.
    Custom {
        mac: Option<[u8; 6]>,
        backend: Box<dyn NetBackend + Send>,
    },
}

//--------------------------------------------------------------------------------------------------
// Types: Console Builder
//--------------------------------------------------------------------------------------------------

/// Builder for console/output configuration.
///
/// # Example
///
/// ```rust,no_run
/// # use msb_krun::VmBuilder;
/// VmBuilder::new()
///     .console(|c| c.output("/tmp/vm.log"));
/// ```
///
/// With the `gpu` and `snd` features:
///
/// ```rust,ignore
/// VmBuilder::new()
///     .console(|c| {
///         c.output("/tmp/vm.log")
///             .sound(true)
///             .gpu_virgl_flags(0x1)
///             .gpu_shm_size(1 << 28)
///     });
/// ```
#[derive(Default)]
pub struct ConsoleBuilder {
    pub(crate) output: Option<PathBuf>,
    #[cfg(not(target_os = "windows"))]
    pub(crate) ports: Vec<PortConfig>,
    pub(crate) disable_implicit: bool,
    #[cfg(feature = "snd")]
    pub(crate) sound: bool,
    #[cfg(feature = "gpu")]
    pub(crate) gpu_virgl_flags: Option<u32>,
    #[cfg(feature = "gpu")]
    pub(crate) gpu_shm_size: Option<usize>,
}

//--------------------------------------------------------------------------------------------------
// Types: Exec Builder
//--------------------------------------------------------------------------------------------------

/// Builder for execution configuration.
///
/// # Examples
///
/// Setting environment variables one at a time with `.env()`:
///
/// ```rust,no_run
/// # use msb_krun::VmBuilder;
/// VmBuilder::new()
///     .exec(|e| {
///         e.path("/bin/myapp")
///             .args(["--flag", "value"])
///             .env("HOME", "/root")
///             .env("LANG", "en_US.UTF-8")
///             .workdir("/app")
///             .rlimit("NOFILE", 1024, 4096)
///     });
/// ```
///
/// Setting environment variables in bulk with `.envs()`:
///
/// ```rust,no_run
/// # use msb_krun::VmBuilder;
/// VmBuilder::new()
///     .exec(|e| {
///         e.path("/bin/myapp")
///             .envs([("HOME", "/root"), ("LANG", "en_US.UTF-8")])
///     });
/// ```
#[derive(Debug, Clone, Default)]
pub struct ExecBuilder {
    pub(crate) path: Option<String>,
    pub(crate) args: Vec<String>,
    pub(crate) env: Vec<(String, String)>,
    pub(crate) workdir: Option<String>,
    pub(crate) rlimits: Vec<(String, u64, u64)>,
}

//--------------------------------------------------------------------------------------------------
// Types: Disk Builder
//--------------------------------------------------------------------------------------------------

/// Supported disk image formats.
#[cfg(feature = "blk")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskImageFormat {
    Raw,
    Qcow2,
    Vmdk,
}

/// Host-side cache behavior for a block device.
///
/// Mirrors libkrun's internal `CacheType`. `Writeback` honors guest flush
/// requests via `fsync`; `Unsafe` advertises flush but makes it a noop.
#[cfg(feature = "blk")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheMode {
    Writeback,
    Unsafe,
}

/// Host-side sync behavior for a block device.
///
/// `None` skips all host sync (fastest, data-loss risk). `Relaxed` honors
/// `VIRTIO_BLK_F_FLUSH` but relaxes hardware sync. `Full` forces strict
/// hardware flush.
#[cfg(feature = "blk")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncMode {
    None,
    Relaxed,
    Full,
}

/// Builder for block device configuration.
///
/// # Example
///
/// ```rust,no_run
/// # use msb_krun::VmBuilder;
/// VmBuilder::new()
///     .disk(|d| d.path("/path/to/disk.img").read_only(true));
/// ```
#[cfg(feature = "blk")]
#[derive(Debug, Clone)]
pub struct DiskBuilder {
    pub(crate) configs: Vec<DiskConfig>,
    current_path: Option<PathBuf>,
    current_id: Option<String>,
    current_read_only: bool,
    current_format: DiskImageFormat,
    current_cache: CacheMode,
    current_direct_io: bool,
    current_sync: SyncMode,
}

/// Configuration for a single block device.
#[cfg(feature = "blk")]
#[derive(Debug, Clone)]
pub struct DiskConfig {
    pub path: PathBuf,
    /// Caller-chosen block id. When `None`, the builder auto-assigns
    /// `vd<suffix>` using the kernel's bijective base-26 scheme.
    pub id: Option<String>,
    pub read_only: bool,
    pub format: DiskImageFormat,
    pub cache: CacheMode,
    pub direct_io: bool,
    pub sync: SyncMode,
}

//--------------------------------------------------------------------------------------------------
// Methods: Machine Builder
//--------------------------------------------------------------------------------------------------

impl MachineBuilder {
    /// Create a new machine builder with defaults.
    pub fn new() -> Self {
        Self {
            vcpus: 1,
            memory_mib: 512,
            hyperthreading: false,
            nested_virt: false,
            split_irqchip: false,
            vsock: false,
            balloon: true,
            balloon_stats_interval: Some(Duration::from_secs(1)),
            rng: true,
            enable_inet_hijack: false,
        }
    }

    /// Set the number of virtual CPUs.
    pub fn vcpus(mut self, count: u8) -> Self {
        self.vcpus = count;
        self
    }

    /// Set the memory size in MiB.
    pub fn memory_mib(mut self, mib: usize) -> Self {
        self.memory_mib = mib;
        self
    }

    /// Enable or disable hyperthreading.
    pub fn hyperthreading(mut self, enabled: bool) -> Self {
        self.hyperthreading = enabled;
        self
    }

    /// Enable or disable nested virtualization.
    pub fn nested_virt(mut self, enabled: bool) -> Self {
        self.nested_virt = enabled;
        self
    }

    /// Enable the userspace split irqchip on x86_64.
    ///
    /// The default in-kernel IOAPIC is hardcoded by KVM to 24 pins, leaving
    /// only 11 IRQs for virtio-mmio devices. The userspace split irqchip
    /// emulates a larger IOAPIC (256 pins) and raises the usable range to
    /// 219 IRQs, which is needed for VMs with many virtio-mmio devices
    /// (e.g. lots of virtio-fs mounts or block devices). No effect on
    /// aarch64 or riscv64.
    pub fn split_irqchip(mut self, enabled: bool) -> Self {
        self.split_irqchip = enabled;
        self
    }

    /// Force-attach a virtio-vsock device to the guest.
    ///
    /// By default, vsock is only attached when needed as a TSI transport
    /// (no virtio-net → HIJACK_INET, or single root virtio-fs on Linux →
    /// HIJACK_UNIX). Set this to `true` when the guest needs a vsock for
    /// its own purposes even though TSI would not otherwise require one.
    pub fn vsock(mut self, enabled: bool) -> Self {
        self.vsock = enabled;
        self
    }

    /// Enable or disable the virtio-balloon device.
    ///
    /// The device is enabled by default for general-purpose VMs. Latency-sensitive
    /// guests that never resize memory can disable it to avoid one virtio-mmio
    /// device and its guest-side probe.
    pub fn balloon(mut self, enabled: bool) -> Self {
        self.balloon = enabled;
        self
    }

    /// Set the virtio-balloon guest memory statistics polling interval.
    ///
    /// `Some(interval)` advertises and drives the stats queue at the requested
    /// cadence. `None` disables balloon stats polling while still allowing the
    /// balloon device itself to be attached.
    pub fn balloon_stats_interval(mut self, interval: Option<Duration>) -> Self {
        self.balloon_stats_interval = interval;
        self
    }

    /// Enable or disable the virtio-rng device.
    ///
    /// The device is enabled by default. Guests with a known nonblocking boot
    /// path can disable it to remove one virtio-mmio device from cold start.
    pub fn rng(mut self, enabled: bool) -> Self {
        self.rng = enabled;
        self
    }

    /// Enable the automatic TSI INET hijack fallback.
    ///
    /// When set to `true` and no virtio-net device is configured,
    /// `TsiFlags::HIJACK_INET` is enabled so the guest's INET socket
    /// calls are transparently bridged to the host through vsock. This
    /// is useful for guests that need outbound connectivity without the
    /// caller setting up a virtio-net backend.
    ///
    /// Defaults to `false` — guests with no virtio-net are air-gapped
    /// by default. Callers must opt in to TSI when they want host
    /// network access without an explicit network device.
    ///
    /// Note: `HIJACK_UNIX` (used by virtio-fs on Linux for AF_UNIX
    /// hijack) is gated on `HIJACK_INET` already being set, so leaving
    /// INET hijack off also leaves the UNIX hijack auto-enable path off.
    pub fn enable_inet_hijack(mut self, enabled: bool) -> Self {
        self.enable_inet_hijack = enabled;
        self
    }
}

impl Default for MachineBuilder {
    fn default() -> Self {
        Self::new()
    }
}

//--------------------------------------------------------------------------------------------------
// Methods: Kernel Builder
//--------------------------------------------------------------------------------------------------

impl KernelBuilder {
    /// Create a new kernel builder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append to the kernel command line.
    pub fn cmdline(mut self, cmdline: &str) -> Self {
        if let Some(ref mut existing) = self.cmdline {
            existing.push(' ');
            existing.push_str(cmdline);
        } else {
            self.cmdline = Some(cmdline.to_string());
        }
        self
    }

    /// Set an explicit path to the libkrunfw shared library.
    ///
    /// When not set, the OS dynamic linker's default search path is used.
    pub fn krunfw_path(mut self, path: impl AsRef<Path>) -> Self {
        self.krunfw_path = Some(path.as_ref().to_path_buf());
        self
    }

    /// Set the path to the init binary inside the guest.
    ///
    /// This controls the kernel `init=` parameter. When not set, defaults
    /// to `/init.krun`.
    pub fn init_path(mut self, path: impl AsRef<Path>) -> Self {
        self.init_path = Some(path.as_ref().to_string_lossy().to_string());
        self
    }
}

//--------------------------------------------------------------------------------------------------
// Methods: Filesystem Builder
//--------------------------------------------------------------------------------------------------

impl FsBuilder {
    /// Create a new filesystem builder.
    pub fn new() -> Self {
        Self {
            configs: Vec::new(),
            current_tag: None,
            current_shm_size: None,
        }
    }

    /// Set the root filesystem path.
    ///
    /// Uses the virtiofs tag `/dev/root`, matching the kernel's expected root device name.
    pub fn root(mut self, path: impl AsRef<Path>) -> Self {
        self.configs.push(FsConfig::Path {
            tag: "/dev/root".to_string(),
            path: path.as_ref().to_path_buf(),
            shm_size: None,
        });
        self
    }

    /// Set the tag for the next mount.
    pub fn tag(mut self, tag: &str) -> Self {
        self.current_tag = Some(tag.to_string());
        self
    }

    /// Set the path for a filesystem mount.
    ///
    /// If `tag()` was called previously, uses that tag; otherwise generates one.
    pub fn path(mut self, path: impl AsRef<Path>) -> Self {
        let tag = self
            .current_tag
            .take()
            .unwrap_or_else(|| format!("fs{}", self.configs.len()));
        let shm_size = self.current_shm_size.take();

        self.configs.push(FsConfig::Path {
            tag,
            path: path.as_ref().to_path_buf(),
            shm_size,
        });
        self
    }

    /// Set the DAX shared memory size for the next mount.
    pub fn shm_size(mut self, size: usize) -> Self {
        self.current_shm_size = Some(size);
        self
    }

    /// Use a custom filesystem backend.
    #[cfg(all(
        not(any(feature = "tee", feature = "aws-nitro")),
        not(target_os = "windows")
    ))]
    pub fn custom(mut self, backend: Box<dyn DynFileSystem + Send + Sync>) -> Self {
        let tag = self
            .current_tag
            .take()
            .unwrap_or_else(|| format!("fs{}", self.configs.len()));

        self.configs.push(FsConfig::Custom { tag, backend });
        self
    }
}

impl Default for FsBuilder {
    fn default() -> Self {
        Self::new()
    }
}

//--------------------------------------------------------------------------------------------------
// Methods: Network Builder
//--------------------------------------------------------------------------------------------------

#[cfg(feature = "net")]
impl NetBuilder {
    /// Create a new network builder.
    pub fn new() -> Self {
        Self {
            configs: Vec::new(),
            current_mac: None,
        }
    }

    /// Set the MAC address for the next network device.
    pub fn mac(mut self, mac: [u8; 6]) -> Self {
        self.current_mac = Some(mac);
        self
    }

    /// Attach a unixgram network backend from a pre-opened fd.
    pub fn unixgram(mut self, fd: OwnedFd) -> Self {
        let mac = self.current_mac.take();
        self.configs.push(NetConfig::UnixgramFd { mac, fd });
        self
    }

    /// Attach a unixgram network backend connecting to a socket path.
    pub fn unixgram_path(mut self, path: impl AsRef<Path>, send_vfkit_magic: bool) -> Self {
        let mac = self.current_mac.take();
        self.configs.push(NetConfig::UnixgramPath {
            mac,
            path: path.as_ref().to_path_buf(),
            send_vfkit_magic,
        });
        self
    }

    /// Attach a unixstream network backend from a pre-opened fd.
    pub fn unixstream(mut self, fd: OwnedFd) -> Self {
        let mac = self.current_mac.take();
        self.configs.push(NetConfig::UnixstreamFd { mac, fd });
        self
    }

    /// Attach a unixstream network backend connecting to a socket path.
    pub fn unixstream_path(mut self, path: impl AsRef<Path>) -> Self {
        let mac = self.current_mac.take();
        self.configs.push(NetConfig::UnixstreamPath {
            mac,
            path: path.as_ref().to_path_buf(),
        });
        self
    }

    /// Attach a TAP network backend.
    #[cfg(target_os = "linux")]
    pub fn tap(mut self, name: impl Into<String>) -> Self {
        let mac = self.current_mac.take();
        self.configs.push(NetConfig::Tap {
            mac,
            name: name.into(),
        });
        self
    }

    /// Use a custom network backend.
    pub fn custom(mut self, backend: Box<dyn NetBackend + Send>) -> Self {
        let mac = self.current_mac.take();
        self.configs.push(NetConfig::Custom { mac, backend });
        self
    }
}

#[cfg(feature = "net")]
impl Default for NetBuilder {
    fn default() -> Self {
        Self::new()
    }
}

//--------------------------------------------------------------------------------------------------
// Methods: Console Builder
//--------------------------------------------------------------------------------------------------

impl ConsoleBuilder {
    /// Create a new console builder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the path to send console output.
    pub fn output(mut self, path: impl AsRef<Path>) -> Self {
        self.output = Some(path.as_ref().to_path_buf());
        self
    }

    /// Enable the virtio-snd device.
    #[cfg(feature = "snd")]
    pub fn sound(mut self, enabled: bool) -> Self {
        self.sound = enabled;
        self
    }

    /// Set GPU virgl flags.
    #[cfg(feature = "gpu")]
    pub fn gpu_virgl_flags(mut self, flags: u32) -> Self {
        self.gpu_virgl_flags = Some(flags);
        self
    }

    /// Set GPU shared memory size.
    #[cfg(feature = "gpu")]
    pub fn gpu_shm_size(mut self, size: usize) -> Self {
        self.gpu_shm_size = Some(size);
        self
    }

    /// Add a bidirectional I/O port to the console device.
    ///
    /// Creates a named port accessible in the guest via `/sys/class/virtio-ports/<name>`.
    /// The host reads from `input_fd` and writes to `output_fd`. Pass the same FD for both
    /// when using a bidirectional socket.
    #[cfg(not(target_os = "windows"))]
    pub fn port(mut self, name: &str, input_fd: RawFd, output_fd: RawFd) -> Self {
        self.ports.push(PortConfig::InOut {
            name: name.to_string(),
            input_fd,
            output_fd,
        });
        self
    }

    /// Add a TTY port to the console device.
    ///
    /// Creates a named port accessible in the guest via `/sys/class/virtio-ports/<name>`.
    /// The `tty_fd` must be a valid terminal file descriptor. Terminal raw mode is configured
    /// automatically.
    #[cfg(not(target_os = "windows"))]
    pub fn port_tty(mut self, name: &str, tty_fd: RawFd) -> Self {
        self.ports.push(PortConfig::Tty {
            name: name.to_string(),
            tty_fd,
        });
        self
    }

    /// Disable the implicit console device.
    ///
    /// By default libkrun creates an implicit console that reads from `STDIN_FILENO`.
    /// Call this to suppress that console when using only explicit ports.
    pub fn disable_implicit(mut self) -> Self {
        self.disable_implicit = true;
        self
    }

    /// Add a custom console port backend.
    ///
    /// The backend provides bidirectional byte I/O without file descriptors
    /// on the data path, matching the pattern of
    /// [`NetBuilder::custom()`](NetBuilder::custom) and
    /// [`FsBuilder::custom()`](FsBuilder::custom).
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// VmBuilder::new()
    ///     .console(|c| c.custom("agent", Box::new(my_backend)))
    /// ```
    #[cfg(not(target_os = "windows"))]
    pub fn custom(mut self, name: &str, backend: Box<dyn ConsolePortBackend>) -> Self {
        let backend: Arc<dyn ConsolePortBackend> = Arc::from(backend);
        let input = Box::new(ConsolePortBackendInputAdapter::new(Arc::clone(&backend)));
        let output = Box::new(ConsolePortBackendOutputAdapter::new(backend));
        self.ports.push(PortConfig::Custom {
            name: name.to_string(),
            input,
            output,
        });
        self
    }
}

//--------------------------------------------------------------------------------------------------
// Methods: Exec Builder
//--------------------------------------------------------------------------------------------------

impl ExecBuilder {
    /// Create a new exec builder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the executable path.
    pub fn path(mut self, path: impl AsRef<Path>) -> Self {
        self.path = Some(path.as_ref().to_string_lossy().to_string());
        self
    }

    /// Set command line arguments.
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.args = args.into_iter().map(|s| s.as_ref().to_string()).collect();
        self
    }

    /// Add a single environment variable.
    pub fn env(mut self, key: &str, value: &str) -> Self {
        self.env.push((key.to_string(), value.to_string()));
        self
    }

    /// Add multiple environment variables.
    pub fn envs<I, K, V>(mut self, envs: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        self.env.extend(
            envs.into_iter()
                .map(|(k, v)| (k.as_ref().to_string(), v.as_ref().to_string())),
        );
        self
    }

    /// Set the working directory.
    pub fn workdir(mut self, path: impl AsRef<Path>) -> Self {
        self.workdir = Some(path.as_ref().to_string_lossy().to_string());
        self
    }

    /// Set a resource limit.
    pub fn rlimit(mut self, resource: &str, soft: u64, hard: u64) -> Self {
        self.rlimits.push((resource.to_string(), soft, hard));
        self
    }
}

//--------------------------------------------------------------------------------------------------
// Methods: Disk Builder
//--------------------------------------------------------------------------------------------------

#[cfg(feature = "blk")]
impl DiskBuilder {
    /// Create a new disk builder.
    pub fn new() -> Self {
        Self {
            configs: Vec::new(),
            current_path: None,
            current_id: None,
            current_read_only: false,
            current_format: DiskImageFormat::Raw,
            current_cache: CacheMode::Writeback,
            current_direct_io: false,
            current_sync: SyncMode::Full,
        }
    }

    /// Set the disk image format for the current disk.
    pub fn format(mut self, format: DiskImageFormat) -> Self {
        self.current_format = format;
        self
    }

    /// Set a stable block id for the current disk.
    ///
    /// When unset, the builder auto-assigns `vd<suffix>` by insertion order
    /// (matching the kernel's bijective base-26 scheme: `vda`..`vdz`,
    /// `vdaa`..`vdzz`, `vdaaa`...). Setting a custom id lets the guest
    /// address the device via `/dev/disk/by-id/virtio-<id>` independent of
    /// attach order.
    pub fn id(mut self, id: impl Into<String>) -> Self {
        self.current_id = Some(id.into());
        self
    }

    /// Set the path for a block device.
    pub fn path(mut self, path: impl AsRef<Path>) -> Self {
        // Finalize any pending config
        if let Some(pending_path) = self.current_path.take() {
            self.configs.push(DiskConfig {
                path: pending_path,
                id: self.current_id.take(),
                read_only: self.current_read_only,
                format: self.current_format,
                cache: self.current_cache,
                direct_io: self.current_direct_io,
                sync: self.current_sync,
            });
            self.current_read_only = false;
            self.current_format = DiskImageFormat::Raw;
            self.current_cache = CacheMode::Writeback;
            self.current_direct_io = false;
            self.current_sync = SyncMode::Full;
        }

        self.current_path = Some(path.as_ref().to_path_buf());
        self
    }

    /// Set read-only mode for the current disk.
    pub fn read_only(mut self, ro: bool) -> Self {
        self.current_read_only = ro;
        self
    }

    /// Set the host-side cache behavior for the current disk.
    pub fn cache(mut self, mode: CacheMode) -> Self {
        self.current_cache = mode;
        self
    }

    /// Enable or disable `O_DIRECT` on the host backing file for the current disk.
    pub fn direct_io(mut self, enabled: bool) -> Self {
        self.current_direct_io = enabled;
        self
    }

    /// Set the host-side sync behavior for the current disk.
    pub fn sync(mut self, mode: SyncMode) -> Self {
        self.current_sync = mode;
        self
    }

    /// Finalize the builder (called internally).
    pub(crate) fn finalize(mut self) -> Self {
        if let Some(path) = self.current_path.take() {
            self.configs.push(DiskConfig {
                path,
                id: self.current_id.take(),
                read_only: self.current_read_only,
                format: self.current_format,
                cache: self.current_cache,
                direct_io: self.current_direct_io,
                sync: self.current_sync,
            });
        }
        self
    }
}

//--------------------------------------------------------------------------------------------------
// Trait Implementations: Disk Builder
//--------------------------------------------------------------------------------------------------

#[cfg(feature = "blk")]
impl Default for DiskBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "blk")]
impl From<DiskImageFormat> for devices::virtio::block::ImageType {
    fn from(format: DiskImageFormat) -> Self {
        match format {
            DiskImageFormat::Raw => devices::virtio::block::ImageType::Raw,
            DiskImageFormat::Qcow2 => devices::virtio::block::ImageType::Qcow2,
            DiskImageFormat::Vmdk => devices::virtio::block::ImageType::Vmdk,
        }
    }
}

#[cfg(feature = "blk")]
impl From<CacheMode> for devices::virtio::CacheType {
    fn from(mode: CacheMode) -> Self {
        match mode {
            CacheMode::Writeback => devices::virtio::CacheType::Writeback,
            CacheMode::Unsafe => devices::virtio::CacheType::Unsafe,
        }
    }
}

#[cfg(feature = "blk")]
impl From<SyncMode> for devices::virtio::block::SyncMode {
    fn from(mode: SyncMode) -> Self {
        match mode {
            SyncMode::None => devices::virtio::block::SyncMode::None,
            SyncMode::Relaxed => devices::virtio::block::SyncMode::Relaxed,
            SyncMode::Full => devices::virtio::block::SyncMode::Full,
        }
    }
}
