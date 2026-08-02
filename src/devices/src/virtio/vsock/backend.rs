use std::io;
use std::sync::Arc;

use utils::eventfd::{EventFd, EFD_NONBLOCK};

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

/// Cloneable wake handle for a custom vsock stream.
///
/// Call [`notify`](Self::notify) whenever a previously blocked stream read or
/// write may make progress. libkrun owns the platform event primitive and its
/// registration with the VMM event loop.
#[derive(Clone, Debug)]
pub struct VsockNotifier {
    event: Arc<EventFd>,
}

/// Factory for custom, in-process services exposed on one host vsock port.
///
/// The factory is shared between connections and may be called concurrently.
/// It should return promptly; expensive setup belongs in backend-managed work.
pub trait VsockPortBackend: Send + Sync {
    /// Accept a guest connection and return its byte-stream endpoint.
    fn connect(
        &self,
        request: VsockConnectRequest,
        notifier: VsockNotifier,
    ) -> io::Result<Box<dyn VsockStreamBackend>>;
}

/// One nonblocking byte stream served by a custom vsock backend.
///
/// Implementations return [`io::ErrorKind::WouldBlock`] when progress is not
/// currently possible. The [`VsockNotifier`] supplied at connection time must
/// be signaled whenever a blocked operation may make progress; libkrun
/// continues to own virtio-vsock framing, credit flow, shutdown, and reset
/// handling around this stream.
pub trait VsockStreamBackend: Send {
    /// Read bytes that should be delivered to the guest.
    fn read(&self, buf: &mut [u8]) -> io::Result<usize>;

    /// Consume bytes received from the guest.
    fn write(&self, buf: &[u8]) -> io::Result<usize>;

    /// Apply a guest-requested half-close or full shutdown.
    fn shutdown(&self, how: VsockShutdown) -> io::Result<()>;
}

//--------------------------------------------------------------------------------------------------
// Methods
//--------------------------------------------------------------------------------------------------

impl VsockNotifier {
    pub(crate) fn new() -> io::Result<Self> {
        Ok(Self {
            event: Arc::new(EventFd::new(EFD_NONBLOCK)?),
        })
    }

    /// Wake libkrun so it retries this stream's nonblocking operations.
    pub fn notify(&self) -> io::Result<()> {
        self.event.write(1)
    }

    pub(crate) fn event(&self) -> &EventFd {
        &self.event
    }

    pub(crate) fn clear(&self) -> io::Result<()> {
        match self.event.read() {
            Ok(_) => Ok(()),
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => Ok(()),
            Err(err) => Err(err),
        }
    }
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notifier_clone_wakes_shared_libkrun_event() {
        let notifier = VsockNotifier::new().unwrap();
        notifier.clone().notify().unwrap();

        notifier.clear().unwrap();
        assert_eq!(
            notifier.event().read().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    }
}
