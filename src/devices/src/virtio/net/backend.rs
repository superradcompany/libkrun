use std::io;
#[cfg(unix)]
use std::os::fd::RawFd;

use utils::event::{EventSource, EventToken};

//--------------------------------------------------------------------------------------------------
// Constants
//--------------------------------------------------------------------------------------------------

/// Device completes checksums for packets transmitted by the guest.
pub const NET_F_CSUM: u64 = 1 << virtio_bindings::virtio_net::VIRTIO_NET_F_CSUM;

/// Device segments IPv4 TCP packets transmitted by the guest.
pub const NET_F_HOST_TSO4: u64 = 1 << virtio_bindings::virtio_net::VIRTIO_NET_F_HOST_TSO4;

/// Device segments IPv6 TCP packets transmitted by the guest.
pub const NET_F_HOST_TSO6: u64 = 1 << virtio_bindings::virtio_net::VIRTIO_NET_F_HOST_TSO6;

#[cfg(unix)]
type BackendError = nix::Error;
#[cfg(windows)]
type BackendError = io::Error;

#[allow(dead_code)]
#[derive(Debug)]
pub enum ConnectError {
    InvalidAddress(BackendError),
    CreateSocket(BackendError),
    Binding(BackendError),
    SendingMagic(BackendError),
    // Tap backend errors.
    OpenNetTun(BackendError),
    TunSetIff(io::Error),
    TunSetVnetHdrSz(io::Error),
    TunSetOffload(io::Error),
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
    /// Return the maximum RX/TX queue pairs this backend can service.
    ///
    /// A value greater than one opts the device into virtio-net MQ negotiation. Implementations
    /// must still preserve packet ordering within each flow and bound aggregate queue resources.
    fn max_queue_pairs(&self) -> u16 {
        1
    }

    /// Return virtio-net features this backend can honor end to end.
    ///
    /// The default is deliberately empty: advertising an offload without consuming the associated
    /// virtio header would silently corrupt packets produced with partial checksums or GSO.
    fn supported_features(&self) -> u64 {
        0
    }

    fn read_frame(&mut self, buf: &mut [u8]) -> Result<usize, ReadError>;
    fn write_frame(&mut self, hdr_len: usize, buf: &mut [u8]) -> Result<(), WriteError>;
    fn has_unfinished_write(&self) -> bool;
    fn try_finish_write(&mut self, hdr_len: usize, buf: &[u8]) -> Result<(), WriteError>;

    #[cfg(unix)]
    fn raw_socket_fd(&self) -> RawFd;

    #[cfg(unix)]
    fn event_source(&self, token: EventToken) -> EventSource {
        EventSource::fd(self.raw_socket_fd(), token)
    }

    #[cfg(windows)]
    fn event_source(&self, token: EventToken) -> EventSource;
}
