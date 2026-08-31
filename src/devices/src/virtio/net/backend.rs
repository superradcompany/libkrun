use std::io;
#[cfg(unix)]
use std::os::fd::RawFd;

use utils::event::{EventSource, EventToken};

#[cfg(unix)]
type BackendError = nix::Error;
#[cfg(windows)]
type BackendError = io::Error;

#[allow(dead_code)]
#[derive(Debug)]
pub enum ConnectError {
    WorkerEvent(io::Error),
    InvalidAddress(BackendError),
    CreateSocket(BackendError),
    Binding(BackendError),
    SendingMagic(BackendError),
    // Tap backend errors.
    OpenNetTun(BackendError),
    TunSetIff(io::Error),
    TunSetVnetHdrSz(io::Error),
    TunSetOffload(io::Error),
    RateLimitTimer(io::Error),
}

#[allow(dead_code)]
#[derive(Debug)]
pub enum ReadError {
    /// Nothing was written
    NothingRead,
    /// Another internal error occurred
    Internal(BackendError),
}

#[allow(dead_code)]
#[derive(Debug)]
pub enum WriteError {
    /// Nothing was written, you can drop the frame or try to resend it later
    NothingWritten,
    /// Part of the buffer was written, the write has to be finished using try_finish_write
    PartialWrite,
    /// Passt doesnt seem to be running (received EPIPE)
    ProcessNotRunning,
    /// Another internal error occurred
    Internal(BackendError),
}

pub trait NetBackend {
    fn read_frame(&mut self, buf: &mut [u8]) -> Result<usize, ReadError>;
    fn write_frame(&mut self, hdr_len: usize, buf: &mut [u8]) -> Result<(), WriteError>;
    fn has_unfinished_write(&self) -> bool;
    fn try_finish_write(&mut self, hdr_len: usize, buf: &[u8]) -> Result<(), WriteError>;

    /// Stops backend-owned helper work before the virtio worker returns its queues.
    ///
    /// Most Unix backends perform all work on the virtio worker and need no extra action. Backends
    /// with private helper threads override this hook so no host writer remains live across a
    /// quiesced device boundary.
    fn quiesce(&mut self) {}

    /// Restarts backend-owned helper work when a quiesced source device resumes.
    fn resume(&mut self) {}

    /// Reports whether every backend operation is cooperatively stoppable at a frame boundary.
    ///
    /// Custom backends opt in explicitly. Returning `false` keeps normal networking available but
    /// lets checkpoint admission reject the device before the VM is paused.
    fn supports_quiesce(&self) -> bool {
        false
    }

    #[cfg(unix)]
    fn raw_socket_fd(&self) -> RawFd;

    #[cfg(unix)]
    fn event_source(&self, token: EventToken) -> EventSource {
        EventSource::fd(self.raw_socket_fd(), token)
    }

    #[cfg(windows)]
    fn event_source(&self, token: EventToken) -> EventSource;
}
