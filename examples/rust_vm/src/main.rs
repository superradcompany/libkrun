//! Simple example demonstrating the msb_krun Rust API.
//!
//! Prerequisites:
//! - libkrunfw shared library (set KRUNFW_PATH or install system-wide)
//! - A Linux rootfs directory for the guest (set KRUN_ROOTFS_PATH to override rootfs-minimal)
//!
//! On macOS, the binary must be codesigned with the hypervisor entitlement:
//!   cd examples && make rust_vm

#[cfg(not(feature = "tee"))]
use std::path::PathBuf;

use msb_krun::{Result, VmBuilder};

//--------------------------------------------------------------------------------------------------
// Constants
//--------------------------------------------------------------------------------------------------

#[cfg(not(feature = "tee"))]
const ROOTFS_PATH_ENV: &str = "KRUN_ROOTFS_PATH";
#[cfg(not(feature = "tee"))]
const DEFAULT_ROOTFS_DIR: &str = "rootfs-minimal";

//--------------------------------------------------------------------------------------------------
// Functions
//--------------------------------------------------------------------------------------------------

fn main() -> Result<()> {
    env_logger::init();

    let krunfw_path = default_krunfw_path();
    #[cfg(not(feature = "tee"))]
    let rootfs_path = rootfs_path();

    #[cfg(not(feature = "tee"))]
    eprintln!("Entering VM (rootfs={})", rootfs_path.display());
    #[cfg(feature = "tee")]
    eprintln!("Entering VM");

    let builder = VmBuilder::new()
        .machine(|m| m.vcpus(2).memory_mib(1024))
        .kernel(|k| k.krunfw_path(&krunfw_path));

    #[cfg(not(feature = "tee"))]
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

fn default_krunfw_path() -> String {
    std::env::var("KRUNFW_PATH").unwrap_or_else(|_| {
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
    })
}

#[cfg(not(feature = "tee"))]
fn rootfs_path() -> PathBuf {
    std::env::var(ROOTFS_PATH_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|_| default_rootfs_path())
}

#[cfg(not(feature = "tee"))]
fn default_rootfs_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(DEFAULT_ROOTFS_DIR)
        .join(std::env::consts::ARCH)
}
