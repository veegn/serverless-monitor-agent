use std::io;
use std::net::{TcpStream, UdpSocket};
use std::time::Duration;

/// A connected loopback pair lets producers wake the socket-owning thread
/// without sharing TLS state or repeatedly polling an idle connection.
pub(crate) struct SocketWake {
    sender: UdpSocket,
    receiver: UdpSocket,
}

impl SocketWake {
    pub(crate) fn new() -> io::Result<Self> {
        let receiver = UdpSocket::bind("127.0.0.1:0")?;
        let sender = UdpSocket::bind("127.0.0.1:0")?;
        receiver.connect(sender.local_addr()?)?;
        sender.connect(receiver.local_addr()?)?;
        sender.set_nonblocking(true)?;
        receiver.set_nonblocking(true)?;
        Ok(Self { sender, receiver })
    }

    pub(crate) fn notify(&self) {
        // Notifications may coalesce. The queue and lifecycle flags are the
        // source of truth, and the bounded wait also covers a failed signal.
        let _ = self.sender.send(&[1]);
    }

    pub(crate) fn wait(&self, socket: &TcpStream, timeout: Duration) -> io::Result<bool> {
        let (readable, notified) = self.poll(socket, timeout)?;
        if notified {
            let mut bytes = [0; 128];
            while self.receiver.recv(&mut bytes).is_ok() {}
        }
        Ok(readable)
    }

    #[cfg(unix)]
    fn poll(&self, socket: &TcpStream, timeout: Duration) -> io::Result<(bool, bool)> {
        use std::os::fd::AsRawFd;
        let mut fds = [
            libc::pollfd {
                fd: socket.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: self.receiver.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let result = unsafe {
            libc::poll(
                fds.as_mut_ptr(),
                fds.len() as libc::nfds_t,
                timeout_ms(timeout),
            )
        };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((fds[0].revents != 0, fds[1].revents != 0))
    }

    #[cfg(windows)]
    fn poll(&self, socket: &TcpStream, timeout: Duration) -> io::Result<(bool, bool)> {
        use std::os::windows::io::AsRawSocket;
        use windows_sys::Win32::Networking::WinSock::{
            WSAGetLastError, WSAPoll, POLLRDNORM, WSAPOLLFD,
        };
        let mut fds = [
            WSAPOLLFD {
                fd: socket.as_raw_socket() as _,
                events: POLLRDNORM,
                revents: 0,
            },
            WSAPOLLFD {
                fd: self.receiver.as_raw_socket() as _,
                events: POLLRDNORM,
                revents: 0,
            },
        ];
        let result = unsafe { WSAPoll(fds.as_mut_ptr(), fds.len() as u32, timeout_ms(timeout)) };
        if result < 0 {
            return Err(io::Error::from_raw_os_error(unsafe { WSAGetLastError() }));
        }
        Ok((fds[0].revents != 0, fds[1].revents != 0))
    }

    #[cfg(not(any(unix, windows)))]
    fn poll(&self, _socket: &TcpStream, _timeout: Duration) -> io::Result<(bool, bool)> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "socket readiness unavailable",
        ))
    }
}

fn timeout_ms(timeout: Duration) -> i32 {
    timeout.as_nanos().div_ceil(1_000_000).min(i32::MAX as u128) as i32
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::time::Instant;

    #[test]
    fn producer_interrupts_wait_and_signals_are_drained() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let socket = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (_peer, _) = listener.accept().unwrap();
        let wake = Arc::new(SocketWake::new().unwrap());
        let producer = wake.clone();
        let thread = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(40));
            producer.notify();
            producer.notify();
        });
        let start = Instant::now();
        assert!(!wake.wait(&socket, Duration::from_secs(2)).unwrap());
        assert!(start.elapsed() < Duration::from_secs(1));
        thread.join().unwrap();
        // Drain a signal racing the first drain, then verify that an idle wait
        // really blocks instead of repeatedly waking on a stale notification.
        assert!(!wake.wait(&socket, Duration::from_millis(1)).unwrap());
        let start = Instant::now();
        assert!(!wake.wait(&socket, Duration::from_millis(40)).unwrap());
        assert!(start.elapsed() >= Duration::from_millis(30));
    }

    #[test]
    fn incoming_data_and_peer_close_are_readable() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let socket = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut peer, _) = listener.accept().unwrap();
        let wake = SocketWake::new().unwrap();
        peer.write_all(b"data").unwrap();
        assert!(wake.wait(&socket, Duration::from_secs(1)).unwrap());
        use std::io::Read;
        (&socket).read_exact(&mut [0; 4]).unwrap();
        drop(peer);
        assert!(wake.wait(&socket, Duration::from_secs(1)).unwrap());
    }
}
