use std::io::{ErrorKind, Read, Write};
use std::mem;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

fn expect_msg(stream: &mut TcpStream, expected: &[u8]) {
    let mut buf = vec![0; expected.len()];
    stream.read_exact(&mut buf[..]).unwrap();
    assert_eq!(&buf[..], expected);
}

fn expect_wouldblock(stream: &mut TcpStream) {
    stream.set_nonblocking(true).unwrap();
    let err = stream.read(&mut [0u8; 1]).unwrap_err();
    stream.set_nonblocking(false).unwrap();
    assert_eq!(err.kind(), ErrorKind::WouldBlock);
}

fn set_timeouts(stream: &mut TcpStream) {
    stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_millis(500)))
        .unwrap();
}

fn connect(port: u16) -> TcpStream {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), port);
    connect_with_timeout(addr, Duration::from_secs(60))
        .unwrap_or_else(|err| panic!("Couldn't connect to server within 60 seconds: {err}"))
}

fn connect_with_timeout(addr: SocketAddr, timeout: Duration) -> std::io::Result<TcpStream> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(std::io::Error::new(
                ErrorKind::TimedOut,
                "server did not become ready",
            ));
        }
        match TcpStream::connect_timeout(&addr, remaining.min(Duration::from_millis(250))) {
            Ok(stream) => return Ok(stream),
            Err(err)
                if matches!(
                    err.kind(),
                    ErrorKind::ConnectionRefused | ErrorKind::TimedOut
                ) =>
            {
                thread::sleep(
                    deadline
                        .saturating_duration_since(Instant::now())
                        .min(Duration::from_millis(100)),
                );
            }
            Err(err) => return Err(err),
        }
    }
}

#[derive(Debug, Copy, Clone)]
pub struct TcpTester {
    port: u16,
}

impl TcpTester {
    pub const fn new(port: u16) -> Self {
        Self { port }
    }

    pub fn create_server_socket(&self) -> TcpListener {
        TcpListener::bind(SocketAddrV4::new(Ipv4Addr::new(0, 0, 0, 0), self.port)).unwrap()
    }

    pub fn run_server(&self, listener: TcpListener) {
        let (mut stream, _addr) = listener.accept().unwrap();
        set_timeouts(&mut stream);
        stream.write_all(b"ping!").unwrap();
        expect_msg(&mut stream, b"pong!");
        expect_wouldblock(&mut stream);
        stream.write_all(b"bye!").unwrap();
        // We leak the file descriptor for now, since there is no easy way to close it on libkrun exit
        mem::forget(listener);
    }

    pub fn run_client(&self) {
        let mut stream = connect(self.port);
        set_timeouts(&mut stream);
        expect_msg(&mut stream, b"ping!");
        expect_wouldblock(&mut stream);
        stream.write_all(b"pong!").unwrap();
        expect_msg(&mut stream, b"bye!");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connects_after_slow_server_startup() {
        let reservation = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let addr = reservation.local_addr().unwrap();
        drop(reservation);
        let server = thread::spawn(move || {
            // Exceed the old five-second retry budget.
            thread::sleep(Duration::from_secs(7));
            let listener = TcpListener::bind(addr).unwrap();
            listener.set_nonblocking(true).unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                match listener.accept() {
                    Ok(_) => return,
                    Err(err)
                        if err.kind() == ErrorKind::WouldBlock && Instant::now() < deadline =>
                    {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(err) => panic!("server did not receive connection: {err}"),
                }
            }
        });
        let result = connect_with_timeout(addr, Duration::from_secs(15));
        server.join().unwrap();
        assert!(result.is_ok(), "{result:?}");
    }

    #[test]
    fn unavailable_server_has_a_bounded_wait() {
        let reservation = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let addr = reservation.local_addr().unwrap();
        drop(reservation);
        let start = Instant::now();
        let err = connect_with_timeout(addr, Duration::from_millis(100)).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::TimedOut);
        assert!(start.elapsed() < Duration::from_secs(2));
    }
}
