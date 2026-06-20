//! Simple example demonstrating the msb_krun Rust API.
//!
//! Prerequisites:
//! - libkrunfw shared library (set KRUNFW_PATH or install system-wide)
//! - The rootfs-alpine git submodule initialized on Unix hosts
//! - On Windows, set KRUN_INITRAMFS_PATH to a Linux initramfs image
//!
//! On macOS, the binary must be codesigned with the hypervisor entitlement:
//!   cd examples && make rust_vm

use msb_krun::{Result, VmBuilder};

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
