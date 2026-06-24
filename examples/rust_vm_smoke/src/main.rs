//! Configurable smoke example for exercising msb_krun device backends.
//!
//! Prerequisites:
//! - libkrunfw shared library (set KRUNFW_PATH or install system-wide)
//! - The rootfs-alpine git submodule initialized on Unix hosts
//! - On Windows, set KRUN_INITRAMFS_PATH to a Linux initramfs image
//! - To attach a block disk, build with `--features blk`, set KRUN_DISK_PATH, and set
//!   KRUN_DISK_FORMAT to raw, qcow2, or vmdk
//! - Optional disk settings: KRUN_DISK_ID and KRUN_DISK_READ_ONLY
//! - To attach a virtio-fs directory, set KRUN_FS_PATH and optionally KRUN_FS_TAG
//! - Optional virtio-fs DAX setting: KRUN_FS_SHM_SIZE
//! - On Windows, set KRUN_VIRTIO_CONSOLE_OUTPUT to test explicit file-backed virtio-console output
//! - On Windows, set KRUN_VIRTIO_CONSOLE_PIPE and optionally KRUN_VIRTIO_CONSOLE_PORT to test a named-pipe virtio-console port
//! - On Windows, set KRUN_VIRTIO_CONSOLE_PIPE_SMOKE to start a built-in named-pipe virtio-console smoke helper
//!
//! On macOS, the binary must be codesigned with the hypervisor entitlement:
//!   cd examples && make rust_vm_smoke

#[cfg(feature = "blk")]
use msb_krun::{CacheMode, DiskImageFormat, SyncMode};
use msb_krun::{ConfigError, Error, Result, VmBuilder};
#[cfg(target_os = "windows")]
use std::ffi::OsStr;
#[cfg(target_os = "windows")]
use std::os::windows::ffi::OsStrExt;
#[cfg(target_os = "windows")]
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
#[cfg(target_os = "windows")]
use std::ptr;
#[cfg(target_os = "windows")]
use std::sync::{mpsc, Arc, Mutex};
#[cfg(target_os = "windows")]
use std::thread;
#[cfg(target_os = "windows")]
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[cfg(target_os = "windows")]
use windows_sys::Win32::Foundation::{ERROR_PIPE_CONNECTED, HANDLE, INVALID_HANDLE_VALUE};
#[cfg(target_os = "windows")]
use windows_sys::Win32::Storage::FileSystem::{ReadFile, PIPE_ACCESS_DUPLEX};
#[cfg(target_os = "windows")]
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_WAIT,
};

//--------------------------------------------------------------------------------------------------
// Constants
//--------------------------------------------------------------------------------------------------

#[cfg(feature = "blk")]
const DISK_PATH_ENV: &str = "KRUN_DISK_PATH";

#[cfg(feature = "blk")]
const DISK_FORMAT_ENV: &str = "KRUN_DISK_FORMAT";

#[cfg(feature = "blk")]
const DISK_READ_ONLY_ENV: &str = "KRUN_DISK_READ_ONLY";

#[cfg(feature = "blk")]
const DISK_ID_ENV: &str = "KRUN_DISK_ID";

#[cfg(feature = "blk")]
const DEFAULT_DISK_ID: &str = "smoke";

const FS_PATH_ENV: &str = "KRUN_FS_PATH";
const FS_TAG_ENV: &str = "KRUN_FS_TAG";
const FS_SHM_SIZE_ENV: &str = "KRUN_FS_SHM_SIZE";
const DEFAULT_FS_TAG: &str = "hostshare";
#[cfg(target_os = "windows")]
const VIRTIO_CONSOLE_OUTPUT_ENV: &str = "KRUN_VIRTIO_CONSOLE_OUTPUT";
#[cfg(target_os = "windows")]
const VIRTIO_CONSOLE_PIPE_ENV: &str = "KRUN_VIRTIO_CONSOLE_PIPE";
#[cfg(target_os = "windows")]
const VIRTIO_CONSOLE_PIPE_SMOKE_ENV: &str = "KRUN_VIRTIO_CONSOLE_PIPE_SMOKE";
#[cfg(target_os = "windows")]
const VIRTIO_CONSOLE_PORT_ENV: &str = "KRUN_VIRTIO_CONSOLE_PORT";
#[cfg(target_os = "windows")]
const DEFAULT_VIRTIO_CONSOLE_PORT: &str = "agent";
#[cfg(target_os = "windows")]
const VIRTIO_CONSOLE_SMOKE_BUFFER_SIZE: usize = 4096;

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

#[cfg(feature = "blk")]
#[derive(Debug, Clone, PartialEq, Eq)]
struct SmokeDiskConfig {
    path: String,
    id: String,
    format: DiskImageFormat,
    read_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SmokeFsConfig {
    path: String,
    tag: String,
    shm_size: Option<usize>,
}

#[cfg(target_os = "windows")]
#[derive(Default)]
struct ConsoleSmokeState {
    received: Option<Vec<u8>>,
    error: Option<String>,
}

//--------------------------------------------------------------------------------------------------
// Methods
//--------------------------------------------------------------------------------------------------

#[cfg(feature = "blk")]
impl SmokeDiskConfig {
    fn format_name(&self) -> &'static str {
        match self.format {
            DiskImageFormat::Raw => "raw",
            DiskImageFormat::Qcow2 => "qcow2",
            DiskImageFormat::Vmdk => "vmdk",
        }
    }
}

//--------------------------------------------------------------------------------------------------
// Functions
//--------------------------------------------------------------------------------------------------

fn main() -> Result<()> {
    env_logger::init();

    let krunfw_path = std::env::var("KRUNFW_PATH").unwrap_or_else(|_| {
        #[cfg(target_os = "macos")]
        {
            "libkrunfw.5.dylib".to_string()
        }
        #[cfg(target_os = "linux")]
        {
            "libkrunfw.so.5".to_string()
        }
        #[cfg(target_os = "windows")]
        {
            "libkrunfw.dll".to_string()
        }
    });

    #[cfg(not(target_os = "windows"))]
    let rootfs_path = format!(
        "{}/rootfs-alpine/{}",
        env!("CARGO_MANIFEST_DIR"),
        std::env::consts::ARCH,
    );

    #[cfg(not(target_os = "windows"))]
    eprintln!("Entering VM (rootfs={rootfs_path})");
    #[cfg(target_os = "windows")]
    eprintln!("Entering VM");

    #[cfg(target_os = "windows")]
    let initramfs_path = std::env::var("KRUN_INITRAMFS_PATH").ok();

    let builder = VmBuilder::new().machine(|m| m.vcpus(2).memory_mib(1024));

    #[cfg(target_os = "windows")]
    let builder = if let Ok(path) = std::env::var(VIRTIO_CONSOLE_OUTPUT_ENV) {
        eprintln!("Attaching virtio-console output {path}");
        builder.console(|c| c.virtio_output(path).disable_implicit())
    } else {
        builder
    };

    #[cfg(target_os = "windows")]
    let (builder, console_smoke_state) = configure_windows_virtio_console(builder)?;

    let builder = if let Some(fs) = smoke_fs_config_from_env()? {
        eprintln!("Attaching virtio-fs {} ({})", fs.path, fs.tag);
        builder.fs(|f| {
            let f = f.tag(&fs.tag);
            if let Some(shm_size) = fs.shm_size {
                f.shm_size(shm_size).path(&fs.path)
            } else {
                f.path(&fs.path)
            }
        })
    } else {
        builder
    };

    #[cfg(feature = "blk")]
    let builder = if let Some(disk) = smoke_disk_config_from_env()? {
        eprintln!(
            "Attaching {} block disk {} ({})",
            disk.format_name(),
            disk.path,
            if disk.read_only {
                "read-only"
            } else {
                "read-write"
            }
        );
        builder.disk(|d| {
            d.path(&disk.path)
                .id(&disk.id)
                .format(disk.format)
                .read_only(disk.read_only)
                .cache(CacheMode::Writeback)
                .sync(SyncMode::Full)
        })
    } else {
        builder
    };

    #[cfg(not(feature = "blk"))]
    if std::env::var_os("KRUN_DISK_PATH").is_some()
        || std::env::var_os("KRUN_DISK_FORMAT").is_some()
        || std::env::var_os("KRUN_DISK_READ_ONLY").is_some()
        || std::env::var_os("KRUN_DISK_ID").is_some()
    {
        eprintln!("KRUN_DISK_* is set, but rust_vm_smoke was built without --features blk");
    }

    #[cfg(target_os = "windows")]
    let builder = builder.kernel(|k| {
        let k = k.krunfw_path(&krunfw_path);
        if let Some(initramfs_path) = &initramfs_path {
            k.initramfs_path(initramfs_path)
                .init_path("/init")
                .cmdline("root=/dev/ram0 rw")
        } else {
            k
        }
    });

    #[cfg(not(target_os = "windows"))]
    let builder = builder.kernel(|k| k.krunfw_path(&krunfw_path));

    #[cfg(all(not(feature = "tee"), not(target_os = "windows")))]
    let builder = builder.fs(|fs| fs.root(&rootfs_path));

    builder
        .exec(|e| {
            e.path("/bin/echo")
                .args(["Hello from libkrun VM!"])
                .env("HOME", "/root")
        })
        .on_exit(move |exit_code| {
            eprintln!("[on_exit] VM exiting with code {exit_code}");
            #[cfg(target_os = "windows")]
            report_windows_virtio_console_smoke(&console_smoke_state);
        })
        .build()?
        .enter()?;

    unreachable!()
}

fn smoke_fs_config_from_env() -> Result<Option<SmokeFsConfig>> {
    smoke_fs_config_from_lookup(|name| std::env::var(name).ok())
}

fn smoke_fs_config_from_lookup(
    mut get: impl FnMut(&str) -> Option<String>,
) -> Result<Option<SmokeFsConfig>> {
    let Some(path) = get(FS_PATH_ENV) else {
        return Ok(None);
    };

    let tag = get(FS_TAG_ENV).unwrap_or_else(|| DEFAULT_FS_TAG.to_string());
    if tag.trim().is_empty() {
        return Err(Error::Config(ConfigError::Filesystem(format!(
            "{FS_TAG_ENV} must not be empty"
        ))));
    }
    let shm_size = if let Some(value) = get(FS_SHM_SIZE_ENV) {
        Some(parse_usize_env(FS_SHM_SIZE_ENV, &value)?)
    } else {
        None
    };

    Ok(Some(SmokeFsConfig {
        path,
        tag,
        shm_size,
    }))
}

#[cfg(feature = "blk")]
fn smoke_disk_config_from_env() -> Result<Option<SmokeDiskConfig>> {
    smoke_disk_config_from_lookup(|name| std::env::var(name).ok())
}

#[cfg(feature = "blk")]
fn smoke_disk_config_from_lookup(
    mut get: impl FnMut(&str) -> Option<String>,
) -> Result<Option<SmokeDiskConfig>> {
    let Some(path) = get(DISK_PATH_ENV) else {
        return Ok(None);
    };

    let Some(format) = get(DISK_FORMAT_ENV) else {
        return Err(block_config_error(format!(
            "{DISK_FORMAT_ENV} must be set when {DISK_PATH_ENV} is set"
        )));
    };

    let format = parse_disk_format(&format)?;
    let read_only = if let Some(value) = get(DISK_READ_ONLY_ENV) {
        parse_bool_env(DISK_READ_ONLY_ENV, &value)?
    } else {
        matches!(format, DiskImageFormat::Vmdk)
    };

    if matches!(format, DiskImageFormat::Vmdk) && !read_only {
        return Err(block_config_error(format!(
            "{DISK_READ_ONLY_ENV}=0 cannot be used with {DISK_FORMAT_ENV}=vmdk"
        )));
    }

    Ok(Some(SmokeDiskConfig {
        path,
        id: get(DISK_ID_ENV).unwrap_or_else(|| DEFAULT_DISK_ID.to_string()),
        format,
        read_only,
    }))
}

#[cfg(feature = "blk")]
fn parse_disk_format(value: &str) -> Result<DiskImageFormat> {
    match value.trim().to_ascii_lowercase().as_str() {
        "raw" => Ok(DiskImageFormat::Raw),
        "qcow2" => Ok(DiskImageFormat::Qcow2),
        "vmdk" => Ok(DiskImageFormat::Vmdk),
        _ => Err(block_config_error(format!(
            "{DISK_FORMAT_ENV} must be raw, qcow2, or vmdk"
        ))),
    }
}

#[cfg(feature = "blk")]
fn parse_bool_env(name: &str, value: &str) -> Result<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(block_config_error(format!(
            "{name} must be 1/0, true/false, yes/no, or on/off"
        ))),
    }
}

#[cfg(feature = "blk")]
fn block_config_error(message: String) -> Error {
    Error::Config(ConfigError::Block(message))
}

fn parse_usize_env(name: &str, value: &str) -> Result<usize> {
    let value = value.trim().parse::<usize>().map_err(|_| {
        Error::Config(ConfigError::Filesystem(format!(
            "{name} must be a positive integer"
        )))
    })?;
    if value == 0 {
        return Err(Error::Config(ConfigError::Filesystem(format!(
            "{name} must be a positive integer"
        ))));
    }

    Ok(value)
}

#[cfg(target_os = "windows")]
fn configure_windows_virtio_console(
    builder: VmBuilder,
) -> Result<(VmBuilder, Option<Arc<Mutex<ConsoleSmokeState>>>)> {
    let port_name = std::env::var(VIRTIO_CONSOLE_PORT_ENV)
        .unwrap_or_else(|_| DEFAULT_VIRTIO_CONSOLE_PORT.to_string());

    if let Ok(pipe_name) = std::env::var(VIRTIO_CONSOLE_PIPE_ENV) {
        eprintln!("Attaching virtio-console named pipe {port_name} ({pipe_name})");
        return Ok((
            builder.console(|c| c.named_pipe(&port_name, pipe_name)),
            None,
        ));
    }

    if std::env::var_os(VIRTIO_CONSOLE_PIPE_SMOKE_ENV).is_none() {
        return Ok((builder, None));
    }

    let pipe_name = unique_console_smoke_pipe_name();
    let state = Arc::new(Mutex::new(ConsoleSmokeState::default()));
    let (ready_tx, ready_rx) = mpsc::channel();

    spawn_named_pipe_console_smoke_server(pipe_name.clone(), Arc::clone(&state), ready_tx);

    ready_rx
        .recv_timeout(Duration::from_secs(2))
        .map_err(|err| {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("named-pipe console smoke helper did not start: {err}"),
            )
        })??;

    eprintln!("Attaching virtio-console named-pipe smoke helper {port_name} at {pipe_name}");

    Ok((
        builder.console(|c| c.named_pipe(&port_name, pipe_name)),
        Some(state),
    ))
}

#[cfg(target_os = "windows")]
fn spawn_named_pipe_console_smoke_server(
    pipe_name: String,
    state: Arc<Mutex<ConsoleSmokeState>>,
    ready_tx: mpsc::Sender<std::io::Result<()>>,
) {
    thread::Builder::new()
        .name("rust-vm-smoke-named-pipe-console".to_string())
        .spawn(move || {
            if let Err(err) = run_named_pipe_console_smoke_server(&pipe_name, &state, ready_tx) {
                state.lock().unwrap().error = Some(err.to_string());
                eprintln!("named-pipe console smoke helper failed: {err}");
            }
        })
        .expect("failed to spawn named-pipe console smoke helper");
}

#[cfg(target_os = "windows")]
fn run_named_pipe_console_smoke_server(
    pipe_name: &str,
    state: &Arc<Mutex<ConsoleSmokeState>>,
    ready_tx: mpsc::Sender<std::io::Result<()>>,
) -> std::io::Result<()> {
    let wide_name = wide_null(pipe_name);
    let handle = unsafe {
        CreateNamedPipeW(
            wide_name.as_ptr(),
            PIPE_ACCESS_DUPLEX,
            PIPE_WAIT,
            1,
            VIRTIO_CONSOLE_SMOKE_BUFFER_SIZE as u32,
            VIRTIO_CONSOLE_SMOKE_BUFFER_SIZE as u32,
            0,
            ptr::null(),
        )
    };

    if handle == INVALID_HANDLE_VALUE {
        let err = std::io::Error::last_os_error();
        let _ = ready_tx.send(Err(std::io::Error::new(err.kind(), err.to_string())));
        return Err(err);
    }

    let handle = unsafe { OwnedHandle::from_raw_handle(handle as _) };
    ready_tx.send(Ok(())).unwrap();

    let connected = unsafe { ConnectNamedPipe(handle.as_raw_handle() as HANDLE, ptr::null_mut()) };
    if connected == 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() != Some(ERROR_PIPE_CONNECTED as i32) {
            return Err(err);
        }
    }
    eprintln!("named-pipe console smoke helper accepted libkrun connection");

    loop {
        let mut bytes = vec![0u8; VIRTIO_CONSOLE_SMOKE_BUFFER_SIZE];
        let mut bytes_read = 0;
        let ok = unsafe {
            ReadFile(
                handle.as_raw_handle() as HANDLE,
                bytes.as_mut_ptr(),
                bytes.len() as u32,
                &mut bytes_read,
                ptr::null_mut(),
            )
        };

        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        if bytes_read == 0 {
            continue;
        }

        bytes.truncate(bytes_read as usize);
        eprintln!(
            "named-pipe console smoke observed guest bytes: {}",
            String::from_utf8_lossy(&bytes)
        );
        state.lock().unwrap().received = Some(bytes);
        break;
    }

    unsafe {
        DisconnectNamedPipe(handle.as_raw_handle() as HANDLE);
    }

    Ok(())
}

#[cfg(target_os = "windows")]
fn report_windows_virtio_console_smoke(state: &Option<Arc<Mutex<ConsoleSmokeState>>>) {
    let Some(state) = state else {
        return;
    };

    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        {
            let state = state.lock().unwrap();
            if state.received.is_some() || state.error.is_some() || Instant::now() >= deadline {
                break;
            }
        }

        thread::sleep(Duration::from_millis(10));
    }

    let state = state.lock().unwrap();
    if let Some(bytes) = &state.received {
        eprintln!(
            "[on_exit] named-pipe console smoke captured: {}",
            String::from_utf8_lossy(bytes)
        );
    } else if let Some(error) = &state.error {
        eprintln!("[on_exit] named-pipe console smoke did not capture guest bytes: {error}");
    } else {
        eprintln!("[on_exit] named-pipe console smoke did not capture guest bytes");
    }
}

#[cfg(target_os = "windows")]
fn unique_console_smoke_pipe_name() -> String {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();

    format!(r"\\.\pipe\libkrun-rust-vm-smoke-console-{timestamp}")
}

#[cfg(target_os = "windows")]
fn wide_null(value: &str) -> Vec<u16> {
    OsStr::new(value).encode_wide().chain(Some(0)).collect()
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(all(test, feature = "blk"))]
mod tests {
    use super::*;

    #[test]
    fn disk_env_is_optional() {
        assert_eq!(smoke_disk_config_from_lookup(|_| None).unwrap(), None);
    }

    #[test]
    fn disk_env_parses_qcow2() {
        let config = smoke_disk_config_from_lookup(|name| match name {
            DISK_PATH_ENV => Some("disk.qcow2".to_string()),
            DISK_FORMAT_ENV => Some("qcow2".to_string()),
            DISK_ID_ENV => Some("data".to_string()),
            _ => None,
        })
        .unwrap()
        .unwrap();

        assert_eq!(config.path, "disk.qcow2");
        assert_eq!(config.id, "data");
        assert_eq!(config.format, DiskImageFormat::Qcow2);
        assert!(!config.read_only);
    }

    #[test]
    fn disk_env_defaults_vmdk_to_read_only() {
        let config = smoke_disk_config_from_lookup(|name| match name {
            DISK_PATH_ENV => Some("disk.vmdk".to_string()),
            DISK_FORMAT_ENV => Some("vmdk".to_string()),
            _ => None,
        })
        .unwrap()
        .unwrap();

        assert_eq!(config.format, DiskImageFormat::Vmdk);
        assert!(config.read_only);
    }

    #[test]
    fn disk_env_rejects_writable_vmdk() {
        let error = smoke_disk_config_from_lookup(|name| match name {
            DISK_PATH_ENV => Some("disk.vmdk".to_string()),
            DISK_FORMAT_ENV => Some("vmdk".to_string()),
            DISK_READ_ONLY_ENV => Some("0".to_string()),
            _ => None,
        })
        .unwrap_err();

        assert!(error
            .to_string()
            .contains("KRUN_DISK_READ_ONLY=0 cannot be used"));
    }

    #[test]
    fn disk_env_requires_format_with_path() {
        let error = smoke_disk_config_from_lookup(|name| match name {
            DISK_PATH_ENV => Some("disk.raw".to_string()),
            _ => None,
        })
        .unwrap_err();

        assert!(error.to_string().contains("KRUN_DISK_FORMAT must be set"));
    }
}

#[cfg(test)]
mod fs_tests {
    use super::*;

    #[test]
    fn fs_env_is_optional() {
        assert_eq!(smoke_fs_config_from_lookup(|_| None).unwrap(), None);
    }

    #[test]
    fn fs_env_defaults_tag() {
        let config = smoke_fs_config_from_lookup(|name| match name {
            FS_PATH_ENV => Some("share".to_string()),
            _ => None,
        })
        .unwrap()
        .unwrap();

        assert_eq!(config.path, "share");
        assert_eq!(config.tag, DEFAULT_FS_TAG);
        assert_eq!(config.shm_size, None);
    }

    #[test]
    fn fs_env_parses_tag() {
        let config = smoke_fs_config_from_lookup(|name| match name {
            FS_PATH_ENV => Some("share".to_string()),
            FS_TAG_ENV => Some("data".to_string()),
            _ => None,
        })
        .unwrap()
        .unwrap();

        assert_eq!(config.path, "share");
        assert_eq!(config.tag, "data");
        assert_eq!(config.shm_size, None);
    }

    #[test]
    fn fs_env_parses_shm_size() {
        let config = smoke_fs_config_from_lookup(|name| match name {
            FS_PATH_ENV => Some("share".to_string()),
            FS_SHM_SIZE_ENV => Some("536870912".to_string()),
            _ => None,
        })
        .unwrap()
        .unwrap();

        assert_eq!(config.path, "share");
        assert_eq!(config.tag, DEFAULT_FS_TAG);
        assert_eq!(config.shm_size, Some(536870912));
    }
}
