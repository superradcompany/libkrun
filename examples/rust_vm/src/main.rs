//! Simple example demonstrating the msb_krun Rust API.
//!
//! Prerequisites:
//! - libkrunfw shared library (set KRUNFW_PATH or install system-wide)
//! - The rootfs-alpine git submodule initialized on Unix hosts
//! - On Windows, set KRUN_INITRAMFS_PATH to a Linux initramfs image
//! - To attach a block disk, build with `--features blk`, set KRUN_DISK_PATH, and set
//!   KRUN_DISK_FORMAT to raw, qcow2, or vmdk
//! - Optional disk settings: KRUN_DISK_ID and KRUN_DISK_READ_ONLY
//! - To attach a virtio-fs directory, set KRUN_FS_PATH and optionally KRUN_FS_TAG
//!
//! On macOS, the binary must be codesigned with the hypervisor entitlement:
//!   cd examples && make rust_vm

#[cfg(feature = "blk")]
use msb_krun::{CacheMode, DiskImageFormat, SyncMode};
use msb_krun::{ConfigError, Error, Result, VmBuilder};

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
const DEFAULT_FS_TAG: &str = "hostshare";

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

    let builder = if let Some(fs) = smoke_fs_config_from_env()? {
        eprintln!("Attaching virtio-fs {} ({})", fs.path, fs.tag);
        builder.fs(|f| f.tag(&fs.tag).path(&fs.path))
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
        eprintln!("KRUN_DISK_* is set, but rust_vm was built without --features blk");
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
        .on_exit(|exit_code| {
            eprintln!("[on_exit] VM exiting with code {exit_code}");
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

    Ok(Some(SmokeFsConfig { path, tag }))
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
    }
}
