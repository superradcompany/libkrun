//! VM Builder for creating and configuring microVMs using nested builders.

use std::sync::atomic::AtomicI32;
use std::sync::Arc;

use utils::eventfd::{EventFd, EFD_NONBLOCK};
use vmm::resources::VirtioConsoleConfigMode;
use vmm::resources::VmResources;
use vmm::vmm_config::machine_config::VmConfig;
use vmm::vmm_config::machine_config::VmConfigError;

#[cfg(not(any(feature = "tee", feature = "aws-nitro")))]
use vmm::vmm_config::fs::CustomFsDeviceConfig;
#[cfg(not(feature = "tee"))]
use vmm::vmm_config::fs::FsDeviceConfig;

#[cfg(feature = "blk")]
use super::builders::DiskBuilder;
#[cfg(not(feature = "tee"))]
use super::builders::FsBuilder;
#[cfg(not(feature = "tee"))]
use super::builders::FsConfig;
use super::builders::{ConsoleBuilder, ExecBuilder, KernelBuilder, MachineBuilder};
#[cfg(feature = "net")]
use super::builders::{NetBuilder, NetConfig};

use super::error::{BuildError, ConfigError, Error, Result};
use super::vm::Vm;

#[cfg(feature = "blk")]
use devices::virtio::block::ImageType;
#[cfg(feature = "blk")]
use devices::virtio::CacheType;
#[cfg(feature = "blk")]
use vmm::vmm_config::block::BlockDeviceConfig;

#[cfg(feature = "net")]
use devices::virtio::net::device::VirtioNetBackend;
#[cfg(all(feature = "net", unix))]
use std::os::fd::IntoRawFd;
#[cfg(feature = "net")]
use vmm::vmm_config::net::NetworkInterfaceConfig;

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

/// Builder for creating and configuring a microVM.
///
/// Uses nested builders for organized configuration:
///
/// # Example
///
/// ```rust,no_run
/// use msb_krun::VmBuilder;
///
/// let vm = VmBuilder::new()
///     .machine(|m| m.vcpus(4).memory_mib(2048))
///     .fs(|fs| fs.root("/path/to/rootfs"))
///     .exec(|e| e.path("/bin/myapp").args(["--flag"]).env("HOME", "/root"))
///     .build()
///     .expect("Failed to build VM");
/// ```
pub struct VmBuilder {
    machine: MachineBuilder,
    kernel: KernelBuilder,
    #[cfg_attr(feature = "tee", allow(dead_code))]
    #[cfg(not(feature = "tee"))]
    fs: FsBuilder,
    console: ConsoleBuilder,
    exec: ExecBuilder,
    #[cfg(feature = "net")]
    net: NetBuilder,
    #[cfg(feature = "blk")]
    disk: DiskBuilder,
    exit_observers: Vec<Box<dyn Fn(i32) + Send + 'static>>,
}

//--------------------------------------------------------------------------------------------------
// Methods
//--------------------------------------------------------------------------------------------------

impl VmBuilder {
    /// Create a new VM builder with default configuration.
    ///
    /// Defaults:
    /// - 1 vCPU
    /// - 512 MiB memory
    /// - Hyperthreading disabled
    /// - Nested virtualization disabled
    pub fn new() -> Self {
        Self {
            machine: MachineBuilder::new(),
            kernel: KernelBuilder::new(),
            #[cfg(not(feature = "tee"))]
            fs: FsBuilder::new(),
            console: ConsoleBuilder::new(),
            exec: ExecBuilder::new(),
            #[cfg(feature = "net")]
            net: NetBuilder::new(),
            #[cfg(feature = "blk")]
            disk: DiskBuilder::new(),
            exit_observers: Vec::new(),
        }
    }

    /// Configure machine settings (vCPUs, memory, etc.).
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
    pub fn machine(mut self, f: impl FnOnce(MachineBuilder) -> MachineBuilder) -> Self {
        self.machine = f(self.machine);
        self
    }

    /// Configure kernel settings.
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
    pub fn kernel(mut self, f: impl FnOnce(KernelBuilder) -> KernelBuilder) -> Self {
        self.kernel = f(self.kernel);
        self
    }

    /// Configure filesystem mounts.
    ///
    /// Can be called multiple times to add multiple mounts.
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
    #[cfg(not(feature = "tee"))]
    pub fn fs(mut self, f: impl FnOnce(FsBuilder) -> FsBuilder) -> Self {
        let new_fs = f(FsBuilder::new());
        self.fs.configs.extend(new_fs.configs);
        self
    }

    /// Configure network devices.
    ///
    /// Can be called multiple times to add multiple devices.
    ///
    /// # Examples
    ///
    /// Unixgram from a pre-opened fd:
    ///
    /// ```rust,ignore
    /// VmBuilder::new()
    ///     .net(|n| n.mac([0x52, 0x54, 0x00, 0x12, 0x34, 0x56]).unixgram(fd));
    /// ```
    ///
    /// Unixgram connecting to a socket path:
    ///
    /// ```rust,ignore
    /// VmBuilder::new()
    ///     .net(|n| n.unixgram_path("/tmp/net.sock", true));
    /// ```
    ///
    /// Windows named pipe:
    ///
    /// ```rust,ignore
    /// VmBuilder::new()
    ///     .net(|n| n.named_pipe(r"\\.\pipe\libkrun-net0"));
    /// ```
    ///
    /// Custom backend:
    ///
    /// ```rust,ignore
    /// VmBuilder::new()
    ///     .net(|n| n.custom(Box::new(my_backend)));
    /// ```
    #[cfg(feature = "net")]
    pub fn net(mut self, f: impl FnOnce(NetBuilder) -> NetBuilder) -> Self {
        let new_net = f(NetBuilder::new());
        self.net.configs.extend(new_net.configs);
        self
    }

    /// Configure block devices.
    ///
    /// Can be called multiple times to add multiple devices. Devices receive
    /// deterministic guest names by attach order (`/dev/vda`, `/dev/vdb`,
    /// ...). For stable addressing across reorderings, set a custom `id()` —
    /// the guest can then reach the disk via `/dev/disk/by-id/virtio-<id>`.
    /// VMDK images must be configured as read-only.
    ///
    /// # Examples
    ///
    /// Single rootfs disk:
    ///
    /// ```rust,no_run
    /// # use msb_krun::VmBuilder;
    /// VmBuilder::new()
    ///     .disk(|d| d.path("/path/to/disk.img").read_only(true));
    /// ```
    ///
    /// Rootfs plus data and cache volumes with stable ids:
    ///
    /// ```rust,no_run
    /// # use msb_krun::{VmBuilder, DiskImageFormat};
    /// VmBuilder::new()
    ///     .disk(|d| d.path("/img/root.raw"))
    ///     .disk(|d| d.path("/img/data.qcow2").format(DiskImageFormat::Qcow2).id("data"))
    ///     .disk(|d| d.path("/img/cache.raw").id("cache").read_only(true));
    /// ```
    #[cfg(feature = "blk")]
    pub fn disk(mut self, f: impl FnOnce(DiskBuilder) -> DiskBuilder) -> Self {
        let new_disk = f(DiskBuilder::new()).finalize();
        self.disk.configs.extend(new_disk.configs);
        self
    }

    /// Configure console and output settings.
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
    pub fn console(mut self, f: impl FnOnce(ConsoleBuilder) -> ConsoleBuilder) -> Self {
        self.console = f(self.console);
        self
    }

    /// Register a callback that runs synchronously on graceful guest-initiated shutdown.
    ///
    /// Multiple observers are supported and are called in registration order.
    /// User callbacks execute after internal device cleanup (console reset, terminal restore).
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// # use msb_krun::VmBuilder;
    /// VmBuilder::new()
    ///     .on_exit(|exit_code| {
    ///         // flush logs, write final status, etc.
    ///         eprintln!("VM exited with code {exit_code}");
    ///     });
    /// ```
    pub fn on_exit(mut self, f: impl Fn(i32) + Send + 'static) -> Self {
        self.exit_observers.push(Box::new(f));
        self
    }

    /// Configure execution settings.
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
    pub fn exec(mut self, f: impl FnOnce(ExecBuilder) -> ExecBuilder) -> Self {
        self.exec = f(self.exec);
        self
    }

    /// Build the VM.
    ///
    /// This validates the configuration and creates a `Vm` instance ready to run.
    pub fn build(self) -> Result<Vm> {
        // Validate configuration
        if self.machine.vcpus == 0 {
            return Err(Error::Config(ConfigError::InvalidVcpuCount(0)));
        }

        if self.machine.memory_mib == 0 {
            return Err(Error::Config(ConfigError::InvalidMemorySize(0)));
        }

        if let Some(affinity) = &self.machine.vcpu_affinity {
            let expected = self.machine.max_vcpus.unwrap_or(self.machine.vcpus) as usize;
            if affinity.len() != expected {
                return Err(Error::Config(ConfigError::InvalidVcpuAffinityLength {
                    expected,
                    actual: affinity.len(),
                }));
            }

            #[cfg(not(any(target_os = "linux", target_os = "windows")))]
            return Err(Error::Config(ConfigError::VcpuAffinityUnsupported));

            #[cfg(target_os = "linux")]
            if let Some(cpu) = affinity.iter().find(|cpu| cpu.group != 0) {
                return Err(Error::Config(ConfigError::InvalidHostCpuGroup(cpu.group)));
            }
        }

        // Build VmResources
        let mut vmr = VmResources::default();

        // Apply machine configuration
        let vm_config = VmConfig {
            vcpu_count: Some(self.machine.vcpus),
            mem_size_mib: Some(self.machine.memory_mib),
            max_vcpu_count: self.machine.max_vcpus,
            max_mem_size_mib: self.machine.max_memory_mib,
            ht_enabled: Some(self.machine.hyperthreading),
            ..Default::default()
        };
        vmr.set_vm_config(&vm_config)
            .map_err(|err| map_vm_config_error(&self.machine, err))?;

        #[cfg(any(target_os = "linux", target_os = "windows"))]
        {
            vmr.vcpu_affinity = self.machine.vcpu_affinity;
        }

        // Reserved CPU capacity is realized through the private msb-cpu device:
        // the guest driver converges on the requested online count and the
        // device's enforcement state parks vCPUs above it host-side.
        #[cfg(not(feature = "tee"))]
        if self
            .machine
            .max_vcpus
            .is_some_and(|max| max > self.machine.vcpus)
        {
            let cpu = devices::virtio::Cpu::new(
                self.machine.max_vcpus.unwrap_or(self.machine.vcpus) as u32,
                self.machine.vcpus as u32,
            )
            .map_err(|e| {
                Error::Build(BuildError::DeviceRegistration(format!(
                    "virtio-msb-cpu: {e:?}"
                )))
            })?;
            vmr.cpu_device = Some(std::sync::Arc::new(std::sync::Mutex::new(cpu)));
        }

        // Reserved memory capacity is realized through a virtio-mem device; the
        // VMM places its hotplug region during boot and `Vm::control_handle`
        // exposes the live resize knob.
        #[cfg(not(feature = "tee"))]
        if self
            .machine
            .max_memory_mib
            .is_some_and(|max| max > self.machine.memory_mib)
        {
            let mem = devices::virtio::Mem::new().map_err(|e| {
                Error::Build(BuildError::DeviceRegistration(format!("virtio-mem: {e:?}")))
            })?;
            vmr.mem_device = Some(std::sync::Arc::new(std::sync::Mutex::new(mem)));
        }
        vmr.nested_enabled = self.machine.nested_virt;
        vmr.split_irqchip = self.machine.split_irqchip;
        vmr.request_vsock = self.machine.vsock;
        vmr.enable_balloon = self.machine.balloon;
        vmr.balloon_stats_interval = self.machine.balloon_stats_interval;
        vmr.enable_rng = self.machine.rng;
        vmr.enable_msb_metrics = self.machine.msb_metrics;

        // Apply filesystem configuration
        #[cfg(not(feature = "tee"))]
        for config in self.fs.configs {
            match config {
                FsConfig::Path {
                    tag,
                    path,
                    shm_size,
                } => {
                    let fs_config = FsDeviceConfig {
                        fs_id: tag,
                        shared_dir: path.to_string_lossy().to_string(),
                        shm_size,
                        allow_root_dir_delete: false,
                    };
                    vmr.fs.push(fs_config);
                }
                #[cfg(not(any(feature = "tee", feature = "aws-nitro")))]
                FsConfig::Custom { tag, backend } => {
                    let backend: Box<dyn devices::virtio::fs::DynFileSystem> = backend;
                    let custom_config = CustomFsDeviceConfig {
                        fs_id: tag,
                        backend: Arc::from(backend),
                        shm_size: None,
                    };
                    vmr.custom_fs.push(custom_config);
                }
            }
        }

        // Apply console configuration
        if let Some(output) = self.console.output {
            vmr.console_output = Some(output);
        }

        #[cfg(feature = "snd")]
        {
            vmr.set_snd_device(self.console.sound);
        }

        #[cfg(feature = "gpu")]
        {
            if let Some(virgl_flags) = self.console.gpu_virgl_flags {
                vmr.set_gpu_virgl_flags(virgl_flags);
            }

            if let Some(shm_size) = self.console.gpu_shm_size {
                vmr.set_gpu_shm_size(shm_size);
            }
        }

        // Apply console port configuration
        if !self.console.ports.is_empty() {
            vmr.virtio_consoles
                .push(VirtioConsoleConfigMode::Explicit(self.console.ports));
        }

        if self.console.disable_implicit {
            vmr.disable_implicit_console = true;
        }

        // Apply network configuration
        #[cfg(feature = "net")]
        for (i, config) in self.net.configs.into_iter().enumerate() {
            let (mac, backend) = match config {
                #[cfg(unix)]
                NetConfig::UnixgramFd { mac, fd } => {
                    (mac, VirtioNetBackend::UnixgramFd(fd.into_raw_fd()))
                }
                #[cfg(unix)]
                NetConfig::UnixgramPath {
                    mac,
                    path,
                    send_vfkit_magic,
                } => (mac, VirtioNetBackend::UnixgramPath(path, send_vfkit_magic)),
                #[cfg(unix)]
                NetConfig::UnixstreamFd { mac, fd } => {
                    (mac, VirtioNetBackend::UnixstreamFd(fd.into_raw_fd()))
                }
                #[cfg(unix)]
                NetConfig::UnixstreamPath { mac, path } => {
                    (mac, VirtioNetBackend::UnixstreamPath(path))
                }
                #[cfg(target_os = "linux")]
                NetConfig::Tap { mac, name } => (mac, VirtioNetBackend::Tap(name)),
                #[cfg(windows)]
                NetConfig::NamedPipe { mac, name } => (mac, VirtioNetBackend::NamedPipe(name)),
                NetConfig::Custom { mac, backend } => (mac, VirtioNetBackend::Custom(backend)),
            };

            let mac = mac.unwrap_or_else(|| generate_mac(i));
            let iface_id = format!("eth{i}");

            let net_config = NetworkInterfaceConfig {
                iface_id,
                backend,
                mac,
                features: 0,
            };

            vmr.net
                .insert(net_config)
                .map_err(|e| Error::Config(ConfigError::Network(e.to_string())))?;
        }

        // Apply block device configuration
        #[cfg(feature = "blk")]
        for (i, config) in self.disk.configs.into_iter().enumerate() {
            let block_id = config
                .id
                .clone()
                .unwrap_or_else(|| format!("vd{}", vd_suffix(i)));
            let image_type: ImageType = config.format.into();
            let cache_type: CacheType = config.cache.into();
            let sync_mode: devices::virtio::block::SyncMode = config.sync.into();

            let blk_config = BlockDeviceConfig {
                block_id,
                cache_type,
                disk_image_path: config.path.to_string_lossy().to_string(),
                disk_image_format: image_type,
                is_disk_read_only: config.read_only,
                direct_io: config.direct_io,
                sync_mode,
            };

            vmr.add_block_device(blk_config)
                .map_err(|e| Error::Config(ConfigError::Block(e.to_string())))?;
        }

        // Format execution configuration
        let exec_path = self.exec.path;

        let args = if self.exec.args.is_empty() {
            None
        } else {
            Some(
                self.exec
                    .args
                    .iter()
                    .map(|s| format!("\"{}\"", s))
                    .collect::<Vec<_>>()
                    .join(" "),
            )
        };

        // The exec env rides the kernel command line as quoted `KEY="value"` words (api/vm.rs
        // get_env → KRUN_ENV), and the vmm cmdline layer rejects control/non-ASCII bytes — a tab
        // in a perfectly legitimate OCI image env value (e.g. gcc's GPG_KEYS) used to reach an
        // `.unwrap()` there and abort the whole VMM. Validate per variable here, where the name
        // is still known, so the caller gets an actionable error instead of a crash. Carrying
        // such values would need an encoded transport (tracked separately).
        for (key, value) in &self.exec.env {
            validate_cmdline_env(key, value).map_err(|reason| {
                Error::Build(BuildError::Start(format!(
                    "guest env var {key:?} cannot be carried on the kernel command line \
                     ({reason}); sanitize or drop this variable"
                )))
            })?;
        }

        let env = if self.exec.env.is_empty() {
            None
        } else {
            Some(
                self.exec
                    .env
                    .iter()
                    .map(|(k, v)| format!(" {}=\"{}\"", k, v))
                    .collect::<String>(),
            )
        };

        let rlimits = if self.exec.rlimits.is_empty() {
            None
        } else {
            Some(
                self.exec
                    .rlimits
                    .iter()
                    .map(|(r, s, h)| format!("{}:{}:{}", r, s, h))
                    .collect::<Vec<_>>()
                    .join(","),
            )
        };

        let exit_evt = EventFd::new(EFD_NONBLOCK)
            .map_err(|e| Error::Build(BuildError::Start(format!("exit EventFd: {e:?}"))))?;
        let exit_code = Arc::new(AtomicI32::new(i32::MAX));

        Ok(Vm::new(
            vmr,
            self.kernel.cmdline,
            exec_path,
            args,
            env,
            self.exec.workdir,
            rlimits,
            self.kernel.krunfw_path,
            self.kernel.initramfs_path,
            self.kernel.init_path,
            self.exit_observers,
            exit_evt,
            exit_code,
            self.machine.enable_inet_hijack,
        ))
    }
}

//--------------------------------------------------------------------------------------------------
// Trait Implementations
//--------------------------------------------------------------------------------------------------

impl Default for VmBuilder {
    fn default() -> Self {
        Self::new()
    }
}

//--------------------------------------------------------------------------------------------------
// Functions
//--------------------------------------------------------------------------------------------------

/// Generate a locally-administered MAC address from an interface index.
#[cfg(feature = "net")]
fn generate_mac(index: usize) -> [u8; 6] {
    [
        0x52,
        0x54,
        0x00,
        0x12,
        0x34,
        0x56u8.wrapping_add(index as u8),
    ]
}

/// Generate a virtio-blk device suffix matching the Linux kernel's
/// `disk_name()` bijective base-26 scheme from `block/genhd.c`:
/// `0→"a"`, `25→"z"`, `26→"aa"`, `27→"ab"`, `701→"zz"`, `702→"aaa"`.
///
/// The naïve `(b'a' + i) as char` formula rolls over past `z` into
/// invalid characters (`vd{`, `vd|`, ...), so this helper is required
/// once we support more than 26 disks.
#[cfg(feature = "blk")]
fn vd_suffix(mut index: usize) -> String {
    let mut buf = Vec::with_capacity(4);
    loop {
        buf.push(b'a' + (index % 26) as u8);
        if index < 26 {
            break;
        }
        index = index / 26 - 1;
    }
    buf.reverse();
    String::from_utf8(buf).expect("ASCII a-z only")
}

fn map_vm_config_error(machine: &MachineBuilder, err: VmConfigError) -> Error {
    match err {
        VmConfigError::InvalidVcpuCount => {
            Error::Config(ConfigError::InvalidVcpuCount(machine.vcpus))
        }
        VmConfigError::InvalidMemorySize => {
            Error::Config(ConfigError::InvalidMemorySize(machine.memory_mib))
        }
        VmConfigError::InvalidMaxVcpuCount => Error::Config(ConfigError::InvalidMaxVcpuCount(
            machine.max_vcpus.unwrap_or(machine.vcpus),
        )),
        VmConfigError::InvalidMaxMemorySize => Error::Config(ConfigError::InvalidMaxMemorySize(
            machine.max_memory_mib.unwrap_or(machine.memory_mib),
        )),
        VmConfigError::MaxCapacityUnsupported => Error::Config(ConfigError::MaxCapacityUnsupported),
    }
}

/// Check that one exec env pair can be carried as a quoted `KEY="value"` kernel-cmdline word: printable ASCII only (the vmm cmdline layer rejects everything else, and the kernel
/// would misparse it anyway), no double quotes (they would terminate the kernel's quote parsing mid-value), and no whitespace in the key (the kernel splits words on unquoted
/// whitespace, so a spaced key silently becomes two parameters).
fn validate_cmdline_env(key: &str, value: &str) -> std::result::Result<(), &'static str> {
    let printable = |s: &str| s.bytes().all(|b| (0x20..=0x7e).contains(&b));
    if !printable(key) || !printable(value) {
        return Err("it contains control or non-ASCII bytes");
    }
    if key.contains('"') || value.contains('"') {
        return Err("it contains a double quote");
    }
    if key.contains(' ') {
        return Err("the key contains whitespace");
    }
    Ok(())
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::builders::HostCpuId;

    #[test]
    fn build_rejects_invalid_machine_config() {
        let err = match VmBuilder::new()
            .machine(|machine| machine.vcpus(3).hyperthreading(true))
            .build()
        {
            Ok(_) => panic!("odd vCPU count with hyperthreading should fail"),
            Err(err) => err,
        };

        match err {
            Error::Config(ConfigError::InvalidVcpuCount(3)) => {}
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn build_rejects_max_vcpus_below_effective_count() {
        let err = match VmBuilder::new()
            .machine(|machine| machine.vcpus(4).max_vcpus(2))
            .build()
        {
            Ok(_) => panic!("max vCPUs below the effective count should fail"),
            Err(err) => err,
        };

        match err {
            Error::Config(ConfigError::InvalidMaxVcpuCount(2)) => {}
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn build_rejects_max_memory_below_effective_size() {
        let err = match VmBuilder::new()
            .machine(|machine| machine.memory_mib(2048).max_memory_mib(1024))
            .build()
        {
            Ok(_) => panic!("max memory below the effective size should fail"),
            Err(err) => err,
        };

        match err {
            Error::Config(ConfigError::InvalidMaxMemorySize(1024)) => {}
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn validate_cmdline_env_rejects_uncarryable_values() {
        // Ordinary env, including spaces in the value (quoted on the cmdline), is fine.
        assert!(validate_cmdline_env("PATH", "/usr/local/bin:/usr/bin").is_ok());
        assert!(validate_cmdline_env("GREETING", "hello world").is_ok());

        // Regression: gcc:13-bookworm's GPG_KEYS/GCC_MIRRORS carry tab separators, which
        // previously reached the vmm cmdline unwrap and aborted the process (InvalidAscii).
        assert!(validate_cmdline_env("GPG_KEYS", "B215C163\t B3C42148").is_err());

        assert!(validate_cmdline_env("MOTD", "héllo").is_err());
        assert!(validate_cmdline_env("Q", "say \"hi\"").is_err());
        assert!(validate_cmdline_env("BAD KEY", "v").is_err());
        assert!(validate_cmdline_env("NL", "a\nb").is_err());
    }

    #[test]
    fn build_rejects_affinity_that_does_not_cover_max_vcpus() {
        let err = match VmBuilder::new()
            .machine(|machine| {
                machine
                    .vcpus(2)
                    .max_vcpus(4)
                    .vcpu_affinity(vec![HostCpuId::new(0), HostCpuId::new(1)])
            })
            .build()
        {
            Ok(_) => panic!("partial vCPU affinity map should fail"),
            Err(err) => err,
        };

        match err {
            Error::Config(ConfigError::InvalidVcpuAffinityLength {
                expected: 4,
                actual: 2,
            }) => {}
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    #[test]
    fn build_rejects_vcpu_affinity_on_unsupported_hosts() {
        let err = match VmBuilder::new()
            .machine(|machine| machine.vcpu_affinity(vec![HostCpuId::new(0)]))
            .build()
        {
            Ok(_) => panic!("vCPU affinity should fail on unsupported hosts"),
            Err(err) => err,
        };

        match err {
            Error::Config(ConfigError::VcpuAffinityUnsupported) => {}
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn build_rejects_nonzero_processor_group_on_linux() {
        let err = match VmBuilder::new()
            .machine(|machine| machine.vcpu_affinity(vec![HostCpuId::in_group(1, 0)]))
            .build()
        {
            Ok(_) => panic!("nonzero processor group should fail on Linux"),
            Err(err) => err,
        };

        match err {
            Error::Config(ConfigError::InvalidHostCpuGroup(1)) => {}
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[cfg(feature = "blk")]
    #[test]
    fn vd_suffix_matches_kernel_scheme() {
        assert_eq!(vd_suffix(0), "a");
        assert_eq!(vd_suffix(25), "z");
        assert_eq!(vd_suffix(26), "aa");
        assert_eq!(vd_suffix(27), "ab");
        assert_eq!(vd_suffix(51), "az");
        assert_eq!(vd_suffix(52), "ba");
        assert_eq!(vd_suffix(701), "zz");
        assert_eq!(vd_suffix(702), "aaa");
    }

    #[cfg(feature = "blk")]
    #[test]
    fn disk_builder_preserves_insertion_order() {
        use crate::api::builders::DiskImageFormat;

        let builder = VmBuilder::new()
            .disk(|d| d.path("/a.raw"))
            .disk(|d| d.path("/b.qcow2").format(DiskImageFormat::Qcow2))
            .disk(|d| {
                d.path("/c.vmdk")
                    .format(DiskImageFormat::Vmdk)
                    .read_only(true)
            });

        let paths: Vec<_> = builder
            .disk
            .configs
            .iter()
            .map(|c| c.path.to_string_lossy().into_owned())
            .collect();
        assert_eq!(paths, vec!["/a.raw", "/b.qcow2", "/c.vmdk"]);
        assert_eq!(builder.disk.configs[1].format, DiskImageFormat::Qcow2);
        assert!(builder.disk.configs[2].read_only);
    }

    #[cfg(feature = "blk")]
    #[test]
    fn disk_builder_auto_id_then_custom() {
        let builder = VmBuilder::new()
            .disk(|d| d.path("/a.raw"))
            .disk(|d| d.path("/b.raw").id("data"))
            .disk(|d| d.path("/c.raw"));

        assert!(builder.disk.configs[0].id.is_none());
        assert_eq!(builder.disk.configs[1].id.as_deref(), Some("data"));
        assert!(builder.disk.configs[2].id.is_none());
    }

    #[cfg(feature = "blk")]
    #[test]
    fn disk_builder_per_disk_settings_dont_leak() {
        use crate::api::builders::{CacheMode, SyncMode};

        let builder = VmBuilder::new()
            .disk(|d| {
                d.path("/a.raw")
                    .read_only(true)
                    .cache(CacheMode::Unsafe)
                    .direct_io(true)
                    .sync(SyncMode::None)
            })
            .disk(|d| d.path("/b.raw"));

        assert!(builder.disk.configs[0].read_only);
        assert_eq!(builder.disk.configs[0].cache, CacheMode::Unsafe);
        assert!(builder.disk.configs[0].direct_io);
        assert_eq!(builder.disk.configs[0].sync, SyncMode::None);

        assert!(!builder.disk.configs[1].read_only);
        assert_eq!(builder.disk.configs[1].cache, CacheMode::Writeback);
        assert!(!builder.disk.configs[1].direct_io);
        assert_eq!(builder.disk.configs[1].sync, SyncMode::Full);
    }
}
