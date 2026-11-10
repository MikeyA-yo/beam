//! Listening sockets that never share their port with another process.
//!
//! **Windows.** A plain `bind` can succeed on a port another program is
//! already serving. Both programs then listen side by side, and the phone
//! may reach the wrong one. We saw two ways this happens:
//!
//! 1. **Dual-stack listeners.** Go's `net.Listen(":8080")`, and many other
//!    runtimes, open one IPv6 socket on `[::]:8080` that also accepts IPv4.
//!    Windows still lets an IPv4 `0.0.0.0:8080` bind through next to it,
//!    with or without `SO_EXCLUSIVEADDRUSE`, and so does an IPv6-only one.
//!    The one bind it refuses is another dual-stack socket. So before
//!    binding we try a dual-stack exclusive bind on `[::]:port` and close
//!    it at once. It is refused if anything at all holds the port: IPv4,
//!    IPv6, or dual-stack, listening or just bound. We checked each case
//!    on Windows 11.
//! 2. **Port hijacking.** Without `SO_EXCLUSIVEADDRUSE` on our listener, a
//!    later program with `SO_REUSEADDR` can bind our port on a specific
//!    address (the LAN IP or loopback) and take that traffic.
//!
//! **Linux and macOS.** The kernel already refuses a port with an active
//! listener. We only set `SO_REUSEADDR`, as tokio's `TcpListener::bind`
//! does, so a restart can reuse the port while the old run's connections
//! sit in TIME_WAIT.

use std::io;
use std::net::SocketAddrV4;

use tokio::net::{TcpListener, TcpSocket};

/// Same backlog tokio's `TcpListener::bind` uses.
const BACKLOG: u32 = 1024;

/// Binds and listens on `addr`. Fails with `AddrInUse` if another socket
/// holds the port, or `PermissionDenied` for ports Windows has reserved.
///
/// Must be called inside a tokio runtime.
pub fn listen_exclusive(addr: SocketAddrV4) -> io::Result<TcpListener> {
    #[cfg(windows)]
    win::ensure_port_free(addr.port())?;

    let socket = TcpSocket::new_v4()?;
    #[cfg(windows)]
    win::set_exclusive(&socket)?;
    #[cfg(not(windows))]
    socket.set_reuseaddr(true)?;
    socket.bind(addr.into())?;
    socket.listen(BACKLOG)
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod win {
    use std::io;
    use std::net::{Ipv6Addr, SocketAddrV6};
    use std::os::windows::io::AsRawSocket;

    use tokio::net::TcpSocket;
    use windows_sys::Win32::Networking::WinSock::{
        IPPROTO_IPV6, IPV6_V6ONLY, SO_EXCLUSIVEADDRUSE, SOCKET, SOCKET_ERROR, SOL_SOCKET,
        WSAGetLastError, setsockopt,
    };

    /// Sets `SO_EXCLUSIVEADDRUSE`. Only works before `bind`.
    pub fn set_exclusive(socket: &TcpSocket) -> io::Result<()> {
        set_bool_opt(socket, SOL_SOCKET, SO_EXCLUSIVEADDRUSE, true)
    }

    /// Lets an IPv6 socket also cover IPv4. Windows defaults to IPv6-only.
    pub fn set_dual_stack(socket: &TcpSocket) -> io::Result<()> {
        set_bool_opt(socket, IPPROTO_IPV6, IPV6_V6ONLY, false)
    }

    /// Fails with `AddrInUse` or `PermissionDenied` if any socket on this
    /// machine holds `port` (see the module docs for why this test bind
    /// is needed). Never listens, and closes the test socket on return.
    pub fn ensure_port_free(port: u16) -> io::Result<()> {
        // Port 0 means "any free port", and the OS picks it during the real bind.
        if port == 0 {
            return Ok(());
        }
        // No IPv6 stack means there can't be a dual-stack holder, and the
        // exclusive IPv4 bind that follows still catches IPv4 holders.
        let Ok(probe) = TcpSocket::new_v6() else {
            return Ok(());
        };
        if let Err(e) = set_dual_stack(&probe) {
            tracing::debug!("port check skipped, dual-stack unavailable: {e}");
            return Ok(());
        }
        set_exclusive(&probe)?;
        match probe.bind(SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, port, 0, 0).into()) {
            Ok(()) => Ok(()),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::AddrInUse | io::ErrorKind::PermissionDenied
                ) =>
            {
                Err(e)
            }
            // Anything else is a broken IPv6 setup, not a conflict.
            Err(e) => {
                tracing::debug!("port check skipped: {e}");
                Ok(())
            }
        }
    }

    fn set_bool_opt(socket: &TcpSocket, level: i32, name: i32, on: bool) -> io::Result<()> {
        // Win32 BOOL, which is a 4-byte int.
        let value = i32::from(on);
        // SAFETY: we borrow `socket` for the whole call, so the handle is an
        // open socket owned by this process. `optval` points to a local that
        // outlives the call, and `optlen` is exactly its size. setsockopt only
        // reads that buffer.
        let rc = unsafe {
            setsockopt(
                socket.as_raw_socket() as SOCKET,
                level,
                name,
                (&raw const value).cast::<u8>(),
                size_of::<i32>() as i32,
            )
        };
        if rc == SOCKET_ERROR {
            // SAFETY: no preconditions. It reads this thread's last Winsock
            // error, which the failed call above just set.
            let code = unsafe { WSAGetLastError() };
            return Err(io::Error::from_raw_os_error(code));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn loopback(port: u16) -> SocketAddrV4 {
        SocketAddrV4::new(Ipv4Addr::LOCALHOST, port)
    }

    fn assert_conflict(result: io::Result<TcpListener>) {
        match result {
            Ok(_) => panic!("bound a port that another socket holds"),
            Err(e) => assert!(
                matches!(
                    e.kind(),
                    io::ErrorKind::AddrInUse | io::ErrorKind::PermissionDenied
                ),
                "unexpected error: {e:?}"
            ),
        }
    }

    #[tokio::test]
    async fn refuses_port_held_by_another_ipv4_listener() {
        let other = std::net::TcpListener::bind(loopback(0)).unwrap();
        let port = other.local_addr().unwrap().port();
        assert_conflict(listen_exclusive(loopback(port)));
    }

    #[tokio::test]
    async fn listener_accepts_connections() {
        let ours = listen_exclusive(loopback(0)).unwrap();
        let addr = ours.local_addr().unwrap();
        let (client, server) = tokio::join!(tokio::net::TcpStream::connect(addr), ours.accept());
        assert!(client.is_ok());
        assert!(server.is_ok());
    }

    /// The Go-server case. The other socket is only bound, never listening,
    /// so the test doesn't trigger a Windows Firewall prompt. The port check
    /// treats bound and listening the same, as verified against a real Go server.
    #[cfg(windows)]
    #[tokio::test]
    async fn refuses_port_held_by_a_dual_stack_socket() {
        use std::net::{Ipv6Addr, SocketAddrV6};
        let Ok(go_like) = TcpSocket::new_v6() else {
            return; // no IPv6 on this machine
        };
        win::set_dual_stack(&go_like).unwrap();
        go_like
            .bind(SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, 0, 0, 0).into())
            .unwrap();
        let port = go_like.local_addr().unwrap().port();
        assert_conflict(listen_exclusive(loopback(port)));
    }

    /// Without `SO_EXCLUSIVEADDRUSE`, Windows lets a reuse-address socket
    /// bind a more specific address on our port. Our listener is on
    /// loopback here, so the intruder takes the same address, which
    /// Windows refuses even without the flag. This test therefore guards
    /// the plain behavior. The specific-address case was checked by hand
    /// against the real binary on `0.0.0.0`.
    #[tokio::test]
    async fn reuseaddr_socket_cannot_join_our_port() {
        let ours = listen_exclusive(loopback(0)).unwrap();
        let port = ours.local_addr().unwrap().port();
        let intruder = TcpSocket::new_v4().unwrap();
        intruder.set_reuseaddr(true).unwrap();
        assert!(intruder.bind(loopback(port).into()).is_err());
    }
}
