//! Windows named-pipe virtio-net example for the msb_krun Rust API.
//!
//! Prerequisites:
//! - libkrunfw shared library (set KRUNFW_PATH or install system-wide)
//! - set KRUN_INITRAMFS_PATH to a Linux initramfs image
//! - optionally set KRUN_NET_NAMED_PIPE to connect to an external message-mode pipe helper
//!
//! Without KRUN_NET_NAMED_PIPE, this example starts a built-in pipe observer and reports the first
//! Ethernet frame written by the guest.

use msb_krun::Result;

#[cfg(target_os = "windows")]
use msb_krun::VmBuilder;
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
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_MESSAGE,
    PIPE_TYPE_MESSAGE, PIPE_WAIT,
};

#[cfg(target_os = "windows")]
const NET_SMOKE_MAX_FRAME_SIZE: usize = 65562;
#[cfg(target_os = "windows")]
const NET_SMOKE_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];

#[cfg(target_os = "windows")]
fn main() -> Result<()> {
    env_logger::init();

    let krunfw_path = std::env::var("KRUNFW_PATH").unwrap_or_else(|_| "libkrunfw.dll".to_string());
    let initramfs_path = std::env::var("KRUN_INITRAMFS_PATH").ok();

    eprintln!("Entering VM with named-pipe virtio-net");

    let builder = VmBuilder::new().machine(|m| m.vcpus(2).memory_mib(1024));
    let (builder, net_smoke_state) = configure_windows_named_pipe_net(builder)?;

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

    builder
        .on_exit(move |exit_code| {
            eprintln!("[on_exit] VM exiting with code {exit_code}");
            report_windows_named_pipe_net_smoke(&net_smoke_state);
        })
        .build()?
        .enter()?;

    unreachable!()
}

#[cfg(not(target_os = "windows"))]
fn main() -> Result<()> {
    eprintln!("rust_vm_net currently demonstrates the Windows named-pipe virtio-net transport");
    Ok(())
}

#[cfg(target_os = "windows")]
#[derive(Default)]
struct NetSmokeState {
    first_frame: Option<FrameSummary>,
    error: Option<String>,
}

#[cfg(target_os = "windows")]
#[derive(Clone)]
struct FrameSummary {
    len: usize,
    dst_mac: [u8; 6],
    src_mac: [u8; 6],
    ethertype: u16,
}

#[cfg(target_os = "windows")]
fn configure_windows_named_pipe_net(
    builder: VmBuilder,
) -> Result<(VmBuilder, Option<Arc<Mutex<NetSmokeState>>>)> {
    if let Ok(pipe_name) = std::env::var("KRUN_NET_NAMED_PIPE") {
        eprintln!("Attaching virtio-net to named pipe {pipe_name}");
        return Ok((
            builder.net(|n| n.mac(NET_SMOKE_MAC).named_pipe(pipe_name)),
            None,
        ));
    }

    let pipe_name = unique_net_smoke_pipe_name();
    let state = Arc::new(Mutex::new(NetSmokeState::default()));
    let (ready_tx, ready_rx) = mpsc::channel();

    spawn_named_pipe_net_smoke_server(pipe_name.clone(), Arc::clone(&state), ready_tx);

    ready_rx
        .recv_timeout(Duration::from_secs(2))
        .map_err(|err| {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("named-pipe net smoke helper did not start: {err}"),
            )
        })??;

    eprintln!("Attaching virtio-net named-pipe smoke helper at {pipe_name}");

    Ok((
        builder.net(|n| n.mac(NET_SMOKE_MAC).named_pipe(pipe_name)),
        Some(state),
    ))
}

#[cfg(target_os = "windows")]
fn spawn_named_pipe_net_smoke_server(
    pipe_name: String,
    state: Arc<Mutex<NetSmokeState>>,
    ready_tx: mpsc::Sender<std::io::Result<()>>,
) {
    thread::Builder::new()
        .name("rust-vm-net-named-pipe-smoke".to_string())
        .spawn(move || {
            if let Err(err) = run_named_pipe_net_smoke_server(&pipe_name, &state, ready_tx) {
                state.lock().unwrap().error = Some(err.to_string());
                eprintln!("named-pipe net smoke helper failed: {err}");
            }
        })
        .expect("failed to spawn named-pipe net smoke helper");
}

#[cfg(target_os = "windows")]
fn run_named_pipe_net_smoke_server(
    pipe_name: &str,
    state: &Arc<Mutex<NetSmokeState>>,
    ready_tx: mpsc::Sender<std::io::Result<()>>,
) -> std::io::Result<()> {
    let wide_name = wide_null(pipe_name);
    let handle = unsafe {
        CreateNamedPipeW(
            wide_name.as_ptr(),
            PIPE_ACCESS_DUPLEX,
            PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT,
            1,
            NET_SMOKE_MAX_FRAME_SIZE as u32,
            NET_SMOKE_MAX_FRAME_SIZE as u32,
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
    eprintln!("named-pipe net smoke helper accepted libkrun connection");

    loop {
        let mut frame = vec![0u8; NET_SMOKE_MAX_FRAME_SIZE];
        let mut bytes_read = 0;
        let ok = unsafe {
            ReadFile(
                handle.as_raw_handle() as HANDLE,
                frame.as_mut_ptr(),
                frame.len() as u32,
                &mut bytes_read,
                ptr::null_mut(),
            )
        };

        if ok == 0 {
            let err = std::io::Error::last_os_error();
            eprintln!("named-pipe net smoke helper read failed: {err}");
            return Err(err);
        }
        if bytes_read == 0 {
            continue;
        }

        frame.truncate(bytes_read as usize);
        let summary = summarize_ethernet_frame(&frame);
        eprintln!(
            "named-pipe net smoke observed guest frame: len={} dst={} src={} ethertype=0x{:04x}",
            summary.len,
            format_mac(summary.dst_mac),
            format_mac(summary.src_mac),
            summary.ethertype,
        );

        state.lock().unwrap().first_frame = Some(summary);
        break;
    }

    unsafe {
        DisconnectNamedPipe(handle.as_raw_handle() as HANDLE);
    }

    Ok(())
}

#[cfg(target_os = "windows")]
fn report_windows_named_pipe_net_smoke(state: &Option<Arc<Mutex<NetSmokeState>>>) {
    let Some(state) = state else {
        return;
    };

    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        {
            let state = state.lock().unwrap();
            if state.first_frame.is_some() || state.error.is_some() || Instant::now() >= deadline {
                break;
            }
        }

        thread::sleep(Duration::from_millis(10));
    }

    let state = state.lock().unwrap();
    if let Some(frame) = &state.first_frame {
        eprintln!(
            "[on_exit] named-pipe net smoke captured frame len={} dst={} src={} ethertype=0x{:04x}",
            frame.len,
            format_mac(frame.dst_mac),
            format_mac(frame.src_mac),
            frame.ethertype,
        );
    } else if let Some(error) = &state.error {
        eprintln!("[on_exit] named-pipe net smoke did not capture a frame: {error}");
    } else {
        eprintln!("[on_exit] named-pipe net smoke did not capture a guest frame");
    }
}

#[cfg(target_os = "windows")]
fn summarize_ethernet_frame(frame: &[u8]) -> FrameSummary {
    let mut dst_mac = [0u8; 6];
    let mut src_mac = [0u8; 6];
    let mut ethertype = 0;

    if frame.len() >= 14 {
        dst_mac.copy_from_slice(&frame[0..6]);
        src_mac.copy_from_slice(&frame[6..12]);
        ethertype = u16::from_be_bytes([frame[12], frame[13]]);
    }

    FrameSummary {
        len: frame.len(),
        dst_mac,
        src_mac,
        ethertype,
    }
}

#[cfg(target_os = "windows")]
fn unique_net_smoke_pipe_name() -> String {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();

    format!(r"\\.\pipe\libkrun-rust-vm-net-smoke-{timestamp}")
}

#[cfg(target_os = "windows")]
fn format_mac(mac: [u8; 6]) -> String {
    format!(
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    )
}

#[cfg(target_os = "windows")]
fn wide_null(value: &str) -> Vec<u16> {
    OsStr::new(value).encode_wide().chain(Some(0)).collect()
}
