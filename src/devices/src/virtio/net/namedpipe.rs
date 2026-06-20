use std::collections::VecDeque;
use std::ffi::OsStr;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::ptr;
use std::sync::{Arc, Mutex};
use std::thread;

use utils::event::{EventSource, EventToken};
use utils::eventfd::EventFd;
use windows_sys::Win32::Foundation::{
    ERROR_BROKEN_PIPE, ERROR_IO_PENDING, ERROR_MORE_DATA, ERROR_NO_DATA, ERROR_PIPE_BUSY,
    ERROR_PIPE_NOT_CONNECTED, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_OVERLAPPED,
    FILE_GENERIC_READ, FILE_GENERIC_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::Pipes::{
    SetNamedPipeHandleState, WaitNamedPipeW, PIPE_READMODE_MESSAGE, PIPE_WAIT,
};
use windows_sys::Win32::System::Threading::CreateEventW;
use windows_sys::Win32::System::IO::{GetOverlappedResult, OVERLAPPED};

use crate::virtio::net::backend::ConnectError;

use super::backend::{NetBackend, ReadError, WriteError};
use super::{vnet_hdr_len, write_virtio_net_hdr, MAX_BUFFER_SIZE};

const PIPE_CONNECT_TIMEOUT_MS: u32 = 5000;
const PIPE_BUSY_RETRIES: usize = 10;

pub struct NamedPipe {
    handle: Arc<OwnedHandle>,
    rx_queue: Arc<Mutex<VecDeque<Vec<u8>>>>,
    rx_error: Arc<Mutex<Option<io::Error>>>,
    rx_event: Arc<EventFd>,
}

impl NamedPipe {
    pub fn open(name: String) -> Result<Self, ConnectError> {
        let handle = open_message_pipe(&name).map_err(ConnectError::CreateSocket)?;
        let pipe = Self {
            handle: Arc::new(handle),
            rx_queue: Arc::new(Mutex::new(VecDeque::new())),
            rx_error: Arc::new(Mutex::new(None)),
            rx_event: Arc::new(EventFd::new(0).map_err(ConnectError::CreateSocket)?),
        };

        pipe.spawn_reader();
        Ok(pipe)
    }

    fn spawn_reader(&self) {
        let handle = Arc::clone(&self.handle);
        let rx_queue = Arc::clone(&self.rx_queue);
        let rx_error = Arc::clone(&self.rx_error);
        let rx_event = Arc::clone(&self.rx_event);

        thread::Builder::new()
            .name("virtio-net named-pipe reader".to_string())
            .spawn(move || read_pipe_frames(handle, rx_queue, rx_error, rx_event))
            .expect("failed to spawn virtio-net named-pipe reader");
    }
}

impl NetBackend for NamedPipe {
    fn read_frame(&mut self, buf: &mut [u8]) -> Result<usize, ReadError> {
        let frame = {
            let mut rx_queue = self.rx_queue.lock().unwrap();
            let frame = rx_queue.pop_front();
            if rx_queue.is_empty() {
                if let Err(err) = self.rx_event.read() {
                    if err.kind() != io::ErrorKind::WouldBlock {
                        log::warn!("failed to drain named-pipe net event: {err}");
                    }
                }
            }
            frame
        };

        let Some(frame) = frame else {
            if let Some(err) = self.rx_error.lock().unwrap().take() {
                return Err(ReadError::Internal(err));
            }

            return Err(ReadError::NothingRead);
        };

        let hdr_len = write_virtio_net_hdr(buf);
        let end = hdr_len.checked_add(frame.len()).ok_or_else(|| {
            ReadError::Internal(io::Error::new(
                io::ErrorKind::InvalidData,
                "named-pipe frame length overflow",
            ))
        })?;
        if end > buf.len() {
            return Err(ReadError::Internal(io::Error::new(
                io::ErrorKind::InvalidData,
                "named-pipe frame is too large for virtio-net buffer",
            )));
        }

        buf[hdr_len..end].copy_from_slice(&frame);
        Ok(end)
    }

    fn write_frame(&mut self, hdr_len: usize, buf: &mut [u8]) -> Result<(), WriteError> {
        if buf.len() <= hdr_len {
            return Err(WriteError::NothingWritten);
        }

        let frame = &buf[hdr_len..];
        log::debug!("virtio-net named-pipe writing frame: bytes={}", frame.len());
        let bytes_written = overlapped_write(self.handle.as_raw_handle() as HANDLE, frame)
            .map_err(|err| {
                if is_pipe_closed(&err) {
                    WriteError::ProcessNotRunning
                } else {
                    WriteError::Internal(err)
                }
            })?;

        if bytes_written as usize != frame.len() {
            return Err(WriteError::Internal(io::Error::new(
                io::ErrorKind::WriteZero,
                "named-pipe message write completed with a short frame",
            )));
        }

        Ok(())
    }

    fn has_unfinished_write(&self) -> bool {
        false
    }

    fn try_finish_write(&mut self, _hdr_len: usize, _buf: &[u8]) -> Result<(), WriteError> {
        Ok(())
    }

    fn event_source(&self, token: EventToken) -> EventSource {
        EventSource::waitable_handle(self.rx_event.as_raw_handle(), token)
    }
}

fn open_message_pipe(name: &str) -> io::Result<OwnedHandle> {
    let wide_name = wide_null(name);

    for _ in 0..PIPE_BUSY_RETRIES {
        let handle = unsafe {
            CreateFileW(
                wide_name.as_ptr(),
                FILE_GENERIC_READ | FILE_GENERIC_WRITE,
                0,
                ptr::null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OVERLAPPED,
                ptr::null_mut(),
            )
        };

        if handle != INVALID_HANDLE_VALUE {
            let owned_handle = unsafe { OwnedHandle::from_raw_handle(handle as _) };
            let mode = PIPE_READMODE_MESSAGE | PIPE_WAIT;
            let ok = unsafe {
                SetNamedPipeHandleState(
                    owned_handle.as_raw_handle() as HANDLE,
                    &mode,
                    ptr::null(),
                    ptr::null(),
                )
            };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }

            return Ok(owned_handle);
        }

        let err = io::Error::last_os_error();
        if err.raw_os_error() != Some(ERROR_PIPE_BUSY as i32) {
            return Err(err);
        }

        let ok = unsafe { WaitNamedPipeW(wide_name.as_ptr(), PIPE_CONNECT_TIMEOUT_MS) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
    }

    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "named-pipe server stayed busy",
    ))
}

fn read_pipe_frames(
    handle: Arc<OwnedHandle>,
    rx_queue: Arc<Mutex<VecDeque<Vec<u8>>>>,
    rx_error: Arc<Mutex<Option<io::Error>>>,
    rx_event: Arc<EventFd>,
) {
    let max_frame_len = MAX_BUFFER_SIZE - vnet_hdr_len();

    loop {
        let mut frame = vec![0u8; max_frame_len];
        match overlapped_read(handle.as_raw_handle() as HANDLE, &mut frame) {
            Ok(0) => continue,
            Ok(bytes_read) => {
                frame.truncate(bytes_read as usize);
                rx_queue.lock().unwrap().push_back(frame);
                wake_rx_event(&rx_event);
                continue;
            }
            Err(err) => {
                let raw_os_error = err.raw_os_error();
                if raw_os_error == Some(ERROR_MORE_DATA as i32) {
                    *rx_error.lock().unwrap() = Some(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "named-pipe frame exceeds virtio-net buffer size",
                    ));
                } else if !is_pipe_closed(&err) {
                    *rx_error.lock().unwrap() = Some(err);
                }

                wake_rx_event(&rx_event);
                break;
            }
        }
    }
}

fn overlapped_read(handle: HANDLE, buf: &mut [u8]) -> io::Result<u32> {
    let mut operation = OverlappedOperation::new()?;
    let ok = unsafe {
        ReadFile(
            handle,
            buf.as_mut_ptr(),
            buf.len() as u32,
            ptr::null_mut(),
            operation.overlapped_mut(),
        )
    };

    operation.finish(handle, ok)
}

fn overlapped_write(handle: HANDLE, buf: &[u8]) -> io::Result<u32> {
    let mut operation = OverlappedOperation::new()?;
    let ok = unsafe {
        WriteFile(
            handle,
            buf.as_ptr(),
            buf.len() as u32,
            ptr::null_mut(),
            operation.overlapped_mut(),
        )
    };

    operation.finish(handle, ok)
}

struct OverlappedOperation {
    overlapped: OVERLAPPED,
    _event: OwnedHandle,
}

impl OverlappedOperation {
    fn new() -> io::Result<Self> {
        let event = unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) };
        if event.is_null() {
            return Err(io::Error::last_os_error());
        }

        let event = unsafe { OwnedHandle::from_raw_handle(event as _) };
        let mut overlapped = OVERLAPPED::default();
        overlapped.hEvent = event.as_raw_handle() as HANDLE;

        Ok(Self {
            overlapped,
            _event: event,
        })
    }

    fn overlapped_mut(&mut self) -> *mut OVERLAPPED {
        &mut self.overlapped
    }

    fn finish(&mut self, handle: HANDLE, ok: i32) -> io::Result<u32> {
        if ok == 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() != Some(ERROR_IO_PENDING as i32) {
                return Err(err);
            }
        }

        let mut bytes_transferred = 0;
        let ok =
            unsafe { GetOverlappedResult(handle, &self.overlapped, &mut bytes_transferred, 1) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(bytes_transferred)
    }
}

fn wake_rx_event(rx_event: &EventFd) {
    if let Err(err) = rx_event.write(1) {
        if err.kind() != io::ErrorKind::WouldBlock {
            log::warn!("failed to wake named-pipe net event: {err}");
        }
    }
}

fn is_pipe_closed(err: &io::Error) -> bool {
    matches!(
        err.raw_os_error(),
        Some(code)
            if code == ERROR_BROKEN_PIPE as i32
                || code == ERROR_NO_DATA as i32
                || code == ERROR_PIPE_NOT_CONNECTED as i32
    )
}

fn wide_null(value: &str) -> Vec<u16> {
    OsStr::new(value).encode_wide().chain(Some(0)).collect()
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use windows_sys::Win32::Foundation::ERROR_PIPE_CONNECTED;
    use windows_sys::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_TYPE_MESSAGE,
        PIPE_UNLIMITED_INSTANCES,
    };

    use super::*;

    #[test]
    fn named_pipe_backend_exchanges_frames() {
        let pipe_name = unique_pipe_name();
        let guest_to_helper = vec![0xde, 0xad, 0xbe, 0xef];
        let helper_to_guest = vec![0xca, 0xfe, 0xba, 0xbe];
        let (server_ready, server_done, server) = spawn_message_pipe_server(
            pipe_name.clone(),
            guest_to_helper.clone(),
            helper_to_guest.clone(),
        );

        server_ready
            .recv_timeout(Duration::from_secs(2))
            .expect("named-pipe server did not start")
            .expect("named-pipe server failed to start");

        let mut backend = NamedPipe::open(pipe_name).unwrap();
        let hdr_len = vnet_hdr_len();
        let mut tx_buf = vec![0u8; hdr_len + guest_to_helper.len()];
        tx_buf[hdr_len..].copy_from_slice(&guest_to_helper);

        backend.write_frame(hdr_len, &mut tx_buf).unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        let mut rx_buf = vec![0u8; MAX_BUFFER_SIZE];
        let rx_len = loop {
            match backend.read_frame(&mut rx_buf) {
                Ok(len) => break len,
                Err(ReadError::NothingRead) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(err) => panic!("failed to read named-pipe frame: {err:?}"),
            }
        };

        assert_eq!(&rx_buf[hdr_len..rx_len], helper_to_guest.as_slice());

        server_done.send(()).unwrap();
        server
            .join()
            .expect("named-pipe server thread panicked")
            .expect("named-pipe server failed");
    }

    fn spawn_message_pipe_server(
        name: String,
        expected_frame: Vec<u8>,
        reply_frame: Vec<u8>,
    ) -> (
        mpsc::Receiver<io::Result<()>>,
        mpsc::Sender<()>,
        thread::JoinHandle<io::Result<()>>,
    ) {
        let (ready_tx, ready_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let wide_name = wide_null(&name);
            let handle = unsafe {
                CreateNamedPipeW(
                    wide_name.as_ptr(),
                    PIPE_ACCESS_DUPLEX,
                    PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT,
                    PIPE_UNLIMITED_INSTANCES,
                    MAX_BUFFER_SIZE as u32,
                    MAX_BUFFER_SIZE as u32,
                    0,
                    ptr::null(),
                )
            };

            if handle == INVALID_HANDLE_VALUE {
                let err = io::Error::last_os_error();
                let _ = ready_tx.send(Err(io::Error::new(err.kind(), err.to_string())));
                return Err(err);
            }

            let handle = unsafe { OwnedHandle::from_raw_handle(handle as _) };
            ready_tx.send(Ok(())).unwrap();

            let connected =
                unsafe { ConnectNamedPipe(handle.as_raw_handle() as HANDLE, ptr::null_mut()) };
            if connected == 0 {
                let err = io::Error::last_os_error();
                if err.raw_os_error() != Some(ERROR_PIPE_CONNECTED as i32) {
                    return Err(err);
                }
            }

            let mut received_frame = vec![0u8; MAX_BUFFER_SIZE];
            let mut bytes_read = 0;
            let ok = unsafe {
                ReadFile(
                    handle.as_raw_handle() as HANDLE,
                    received_frame.as_mut_ptr(),
                    received_frame.len() as u32,
                    &mut bytes_read,
                    ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }

            received_frame.truncate(bytes_read as usize);
            if received_frame != expected_frame {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "named-pipe server received unexpected frame",
                ));
            }

            let mut bytes_written = 0;
            let ok = unsafe {
                WriteFile(
                    handle.as_raw_handle() as HANDLE,
                    reply_frame.as_ptr(),
                    reply_frame.len() as u32,
                    &mut bytes_written,
                    ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            if bytes_written as usize != reply_frame.len() {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "named-pipe server wrote a short frame",
                ));
            }

            let _ = done_rx.recv_timeout(Duration::from_secs(2));

            unsafe {
                DisconnectNamedPipe(handle.as_raw_handle() as HANDLE);
            }

            Ok(())
        });

        (ready_rx, done_tx, server)
    }

    fn unique_pipe_name() -> String {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();

        format!(r"\\.\pipe\libkrun-net-test-{timestamp}")
    }
}
