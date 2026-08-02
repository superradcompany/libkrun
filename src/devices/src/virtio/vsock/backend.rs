use std::io;
use std::os::fd::RawFd;

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

/// Metadata for a guest-initiated connection to a registered host port.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VsockConnectRequest {
    /// CID of the guest opening the connection.
    pub guest_cid: u64,
    /// Ephemeral source port selected by the guest.
    pub guest_port: u32,
    /// Host port on which the backend was registered.
    pub host_port: u32,
}

/// Direction requested by a guest shutdown packet.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VsockShutdown {
    Read,
    Write,
    Both,
}

/// Factory for custom, in-process services exposed on one host vsock port.
///
/// The factory is shared between connections and may be called concurrently.
/// It should return promptly; expensive setup belongs in backend-managed work.
pub trait VsockPortBackend: Send + Sync {
    /// Accept a guest connection and return its byte-stream endpoint.
    fn connect(&self, request: VsockConnectRequest) -> io::Result<Box<dyn VsockStreamBackend>>;
}

/// One nonblocking byte stream served by a custom vsock backend.
///
/// Implementations return [`io::ErrorKind::WouldBlock`] when progress is not
/// currently possible. `poll_fd` must become readable whenever a blocked read
/// or write may make progress; libkrun continues to own virtio-vsock framing,
/// credit flow, shutdown, and reset handling around this stream.
pub trait VsockStreamBackend: Send {
    /// Read bytes that should be delivered to the guest.
    fn read(&self, buf: &mut [u8]) -> io::Result<usize>;

    /// Consume bytes received from the guest.
    fn write(&self, buf: &[u8]) -> io::Result<usize>;

    /// Apply a guest-requested half-close or full shutdown.
    fn shutdown(&self, how: VsockShutdown) -> io::Result<()>;

    /// Readiness source used by the vsock muxer event loop.
    ///
    /// The descriptor must remain open for this stream's lifetime and must be
    /// independently registerable with epoll (do not share one descriptor
    /// between concurrently active streams).
    fn poll_fd(&self) -> RawFd;
}
