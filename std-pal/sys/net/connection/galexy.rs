//! `std::net` on Galexy dials through `net`.
//!
//! `TcpStream::connect` takes a granted name, one stored address, or
//! one stored `address:port`. The kernel asks the seat when the card
//! is missing. The value is a lane Cap. `TcpListener::bind` binds the
//! stored local address when the seat holds write on that card.
//! `accept` on that listener, and `listen_port`, accept on the stored
//! port with write on that card. `UdpSocket::send_card` sends one
//! datagram to the stored peer. `UdpSocket::recv_card` receives one
//! datagram from that same peer. This file does not parse TCP. rustls
//! stays in the `net` service. Timeouts and socket options stay
//! `unsupported`.

use crate::fmt;
use crate::io::{self, BorrowedCursor, IoSlice, IoSliceMut};
use crate::net::{Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, SocketAddrV4, ToSocketAddrs};
use crate::sys::pal::abi::{self, Cap, CapRights, SysError, Syscall};
use crate::sys::pal::{syscall, unsupported};
use crate::time::Duration;

pub struct TcpStream {
    lane: Cap,
}

impl TcpStream {
    /// Resolve `name` over DNS-over-TLS. The reply is the address text.
    /// A missing card asks the seat. This does not open the dial.
    pub fn lookup(name: &str) -> io::Result<String> {
        if !is_name(name) || is_address(name) {
            return Err(io::const_error!(io::ErrorKind::InvalidInput, "bad name"));
        }
        let text = net_text(abi::NET_OP_RESOLVE, name)?;
        Ok(text)
    }

    /// Dial `name`, one stored address, or `address:port`.
    pub fn connect(name: &str) -> io::Result<TcpStream> {
        if is_address(name) && !is_passed_endpoint(name) {
            return unsupported();
        }
        if !is_name(name) {
            return Err(io::const_error!(io::ErrorKind::InvalidInput, "bad name"));
        }
        let lane = dial(name)?;
        Ok(TcpStream { lane })
    }

    pub fn connect_timeout(_: &SocketAddr, _: Duration) -> io::Result<TcpStream> {
        unsupported()
    }

    pub fn set_read_timeout(&self, _: Option<Duration>) -> io::Result<()> {
        unsupported()
    }

    pub fn set_write_timeout(&self, _: Option<Duration>) -> io::Result<()> {
        unsupported()
    }

    pub fn read_timeout(&self) -> io::Result<Option<Duration>> {
        unsupported()
    }

    pub fn write_timeout(&self) -> io::Result<Option<Duration>> {
        unsupported()
    }

    pub fn peek(&self, _: &mut [u8]) -> io::Result<usize> {
        unsupported()
    }

    pub fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        syscall::read(self.lane, buf).map_err(syscall::io_error)
    }

    pub fn read_buf(&self, mut buf: BorrowedCursor<'_, u8>) -> io::Result<()> {
        let mut tmp = [0u8; 256];
        let n = tmp.len().min(buf.capacity());
        let got = self.read(&mut tmp[..n])?;
        buf.append(&tmp[..got]);
        Ok(())
    }

    pub fn read_vectored(&self, bufs: &mut [IoSliceMut<'_>]) -> io::Result<usize> {
        let Some(buf) = bufs.iter_mut().find(|b| !b.is_empty()) else {
            return Ok(0);
        };
        self.read(buf)
    }

    pub fn is_read_vectored(&self) -> bool {
        false
    }

    pub fn write(&self, buf: &[u8]) -> io::Result<usize> {
        syscall::write(self.lane, buf).map_err(syscall::io_error)
    }

    pub fn write_vectored(&self, bufs: &[IoSlice<'_>]) -> io::Result<usize> {
        let Some(buf) = bufs.iter().find(|b| !b.is_empty()) else {
            return Ok(0);
        };
        self.write(buf)
    }

    pub fn is_write_vectored(&self) -> bool {
        false
    }

    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        unsupported()
    }

    pub fn socket_addr(&self) -> io::Result<SocketAddr> {
        unsupported()
    }

    pub fn shutdown(&self, _: Shutdown) -> io::Result<()> {
        unsupported()
    }

    pub fn duplicate(&self) -> io::Result<TcpStream> {
        unsupported()
    }

    pub fn set_linger(&self, _: Option<Duration>) -> io::Result<()> {
        unsupported()
    }

    pub fn linger(&self) -> io::Result<Option<Duration>> {
        unsupported()
    }

    pub fn set_keepalive(&self, _: bool) -> io::Result<()> {
        unsupported()
    }

    pub fn keepalive(&self) -> io::Result<bool> {
        unsupported()
    }

    pub fn set_nodelay(&self, _: bool) -> io::Result<()> {
        unsupported()
    }

    pub fn nodelay(&self) -> io::Result<bool> {
        unsupported()
    }

    pub fn set_ttl(&self, _: u32) -> io::Result<()> {
        unsupported()
    }

    pub fn ttl(&self) -> io::Result<u32> {
        unsupported()
    }

    pub fn take_error(&self) -> io::Result<Option<io::Error>> {
        unsupported()
    }

    pub fn set_nonblocking(&self, _: bool) -> io::Result<()> {
        unsupported()
    }
}

impl Drop for TcpStream {
    fn drop(&mut self) {
        let _ = syscall::close(self.lane);
    }
}

impl fmt::Debug for TcpStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TcpStream")
    }
}

/// A dotted IPv4 address, or that address plus `:port`.
fn is_passed_endpoint(name: &str) -> bool {
    let (host, port) = match name.split_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (name, None),
    };
    if !dotted_v4(host) {
        return false;
    }
    match port {
        None => true,
        Some(port) => {
            !port.is_empty()
                && port.bytes().all(|b| b.is_ascii_digit())
                && !(port.len() > 1 && port.starts_with('0'))
        }
    }
}

fn dotted_v4(text: &str) -> bool {
    let mut parts = 0usize;
    for part in text.split('.') {
        if part.is_empty() || part.len() > 3 || !part.bytes().all(|b| b.is_ascii_digit()) {
            return false;
        }
        let Ok(n) = part.parse::<u16>() else {
            return false;
        };
        if n > 255 {
            return false;
        }
        parts += 1;
    }
    parts == 4
}

/// A colon is a port. Four numeric pieces are an IPv4 address. A
/// string of only digits is a port.
fn is_address(name: &str) -> bool {
    if name.contains(':') || name.contains('/') {
        return true;
    }
    if !name.is_empty() && name.bytes().all(|b| b.is_ascii_digit()) {
        return true;
    }
    let mut parts = 0usize;
    for part in name.split('.') {
        if part.is_empty() || part.len() > 3 || !part.bytes().all(|b| b.is_ascii_digit()) {
            return false;
        }
        parts += 1;
    }
    parts == 4
}

fn is_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && name.bytes().all(|b| (0x21..=0x7e).contains(&b))
}

/// Resolve or another name opcode. The reply text has no lane.
fn net_text(op: u8, name: &str) -> io::Result<String> {
    let (reply, got, lane) = net_rpc(op, name)?;
    if lane != 0 {
        return Err(io::const_error!(
            io::ErrorKind::InvalidData,
            "unexpected lane"
        ));
    }
    let text = core::str::from_utf8(&reply[8..got.min(reply.len())]).unwrap_or("");
    if text.contains("denied") {
        return Err(syscall::io_error(SysError::AccessDenied));
    }
    if text.contains("timed") {
        return Err(syscall::io_error(SysError::TimedOut));
    }
    if text.is_empty() {
        return Err(io::const_error!(io::ErrorKind::InvalidData, "bad reply"));
    }
    Ok(text.to_string())
}

/// Same payload `fetch` sends: opcode, the name, then `whoami`.
fn dial(name: &str) -> io::Result<Cap> {
    let (reply, got, lane) = net_rpc(abi::NET_OP_DIAL, name)?;
    if lane == 0 {
        let text = core::str::from_utf8(&reply[8..got.min(reply.len())]).unwrap_or("");
        let err = if text.contains("denied") {
            SysError::AccessDenied
        } else if text.contains("timed") {
            SysError::TimedOut
        } else {
            SysError::Unsupported
        };
        return Err(syscall::io_error(err));
    }
    Ok(Cap::from_bits(lane))
}

fn net_rpc(op: u8, name: &str) -> io::Result<([u8; abi::CHAN_MSG_MAX], usize, u64)> {
    let mut who = [0u8; 32];
    let who_n = syscall::syscall(
        Syscall::User,
        who.as_mut_ptr().addr() as u64,
        who.len() as u64,
        abi::USER_WHOAMI,
    )
    .map(|n| n as usize)
    .unwrap_or(0)
    .min(who.len());
    let mut payload = [0u8; 96];
    let name_b = name.as_bytes();
    if name_b.len() + who_n + 3 > payload.len() {
        return Err(io::const_error!(io::ErrorKind::InvalidInput, "bad name"));
    }
    payload[0] = op;
    payload[1] = name_b.len() as u8;
    payload[2..2 + name_b.len()].copy_from_slice(name_b);
    let at = 2 + name_b.len();
    payload[at] = who_n as u8;
    payload[at + 1..at + 1 + who_n].copy_from_slice(&who[..who_n]);
    let n = at + 1 + who_n;
    let mut reply = [0u8; abi::CHAN_MSG_MAX];
    let got = syscall::syscall6(
        Syscall::Send,
        abi::reserved::net(CapRights::WRITE).bits(),
        payload.as_ptr().addr() as u64,
        n as u64,
        reply.as_mut_ptr().addr() as u64,
        reply.len() as u64,
    )
    .map_err(syscall::io_error)?;
    let got = got as usize;
    if got < 8 {
        return Err(io::const_error!(io::ErrorKind::InvalidData, "bad reply"));
    }
    let lane = u64::from_le_bytes(reply[..8].try_into().unwrap_or([0; 8]));
    Ok((reply, got, lane))
}

fn bind_local(addr: &str) -> io::Result<()> {
    let bytes = addr.as_bytes();
    let mut payload = [0u8; 32];
    if bytes.len() + 2 > payload.len() {
        return Err(io::const_error!(io::ErrorKind::InvalidInput, "bad address"));
    }
    payload[0] = abi::NET_OP_BIND;
    payload[1] = bytes.len() as u8;
    payload[2..2 + bytes.len()].copy_from_slice(bytes);
    let n = 2 + bytes.len();
    let mut reply = [0u8; abi::CHAN_MSG_MAX];
    let got = syscall::syscall6(
        Syscall::Send,
        abi::reserved::net(CapRights::WRITE).bits(),
        payload.as_ptr().addr() as u64,
        n as u64,
        reply.as_mut_ptr().addr() as u64,
        reply.len() as u64,
    )
    .map_err(syscall::io_error)?;
    let got = got as usize;
    if got < 8 {
        return Err(io::const_error!(io::ErrorKind::InvalidData, "bad reply"));
    }
    let text = core::str::from_utf8(&reply[8..got.min(reply.len())]).unwrap_or("");
    if text.contains("ok") {
        return Ok(());
    }
    let err = if text.contains("denied") {
        SysError::AccessDenied
    } else if text.contains("timed") {
        SysError::TimedOut
    } else {
        SysError::Unsupported
    };
    Err(syscall::io_error(err))
}

pub struct TcpListener {
    port: Option<u16>,
}

impl TcpListener {
    /// Bind the stored local address. A non-zero port is remembered
    /// for `accept`, which uses the listen card.
    pub fn bind<A: ToSocketAddrs>(addr: A) -> io::Result<TcpListener> {
        let sock = addr
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| io::const_error!(io::ErrorKind::InvalidInput, "bad address"))?;
        let SocketAddr::V4(v4) = sock else {
            return unsupported();
        };
        let ip = v4.ip().octets();
        let text = format!("{}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3]);
        bind_local(&text)?;
        let port = v4.port();
        Ok(TcpListener {
            port: if port == 0 { None } else { Some(port) },
        })
    }

    /// Listen on the stored port. `Ok(None)` means the gate is attached
    /// and no lane has arrived. `Ok(Some)` is the inbound body.
    pub fn listen_port(port: &str) -> io::Result<Option<TcpStream>> {
        if port.is_empty()
            || port.len() > 5
            || !port.bytes().all(|b| b.is_ascii_digit())
            || (port.len() > 1 && port.starts_with('0'))
        {
            return Err(io::const_error!(io::ErrorKind::InvalidInput, "bad port"));
        }
        let (reply, got, lane) = net_rpc(abi::NET_OP_LISTEN, port)?;
        if lane != 0 {
            return Ok(Some(TcpStream {
                lane: Cap::from_bits(lane),
            }));
        }
        let text = core::str::from_utf8(&reply[8..got.min(reply.len())]).unwrap_or("");
        if text.contains("attached") {
            return Ok(None);
        }
        let err = if text.contains("held") {
            io::const_error!(io::ErrorKind::AddrInUse, "held")
        } else if text.contains("denied") {
            syscall::io_error(SysError::AccessDenied)
        } else if text.contains("timed") {
            syscall::io_error(SysError::TimedOut)
        } else {
            io::const_error!(io::ErrorKind::InvalidData, "bad reply")
        };
        Err(err)
    }

    pub fn socket_addr(&self) -> io::Result<SocketAddr> {
        unsupported()
    }

    /// Accept one inbound lane on the port `bind` stored. The first
    /// call attaches the gate. The next waits for the body. The value
    /// is a lane.
    pub fn accept(&self) -> io::Result<(TcpStream, SocketAddr)> {
        let Some(port) = self.port else {
            return unsupported();
        };
        let text = port.to_string();
        loop {
            if let Some(stream) = Self::listen_port(&text)? {
                let addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port));
                return Ok((stream, addr));
            }
        }
    }

    pub fn duplicate(&self) -> io::Result<TcpListener> {
        unsupported()
    }

    pub fn set_ttl(&self, _: u32) -> io::Result<()> {
        unsupported()
    }

    pub fn ttl(&self) -> io::Result<u32> {
        unsupported()
    }

    pub fn set_only_v6(&self, _: bool) -> io::Result<()> {
        unsupported()
    }

    pub fn only_v6(&self) -> io::Result<bool> {
        unsupported()
    }

    pub fn take_error(&self) -> io::Result<Option<io::Error>> {
        unsupported()
    }

    pub fn set_nonblocking(&self, _: bool) -> io::Result<()> {
        unsupported()
    }
}

impl fmt::Debug for TcpListener {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TcpListener")
    }
}

pub struct UdpSocket(!);

impl UdpSocket {
    pub fn bind<A: ToSocketAddrs>(_: A) -> io::Result<UdpSocket> {
        unsupported()
    }

    /// Send `payload` to the peer stored on `name`. Read on that card.
    /// This does not receive, and it does not take an address.
    pub fn send_card(name: &str, payload: &[u8]) -> io::Result<()> {
        if !is_name(name) || is_address(name) || payload.is_empty() || payload.len() > 64 {
            return Err(io::const_error!(
                io::ErrorKind::InvalidInput,
                "bad datagram"
            ));
        }
        let mut body = [0u8; 96];
        let name_b = name.as_bytes();
        if name_b.len() + payload.len() + 3 > body.len() {
            return Err(io::const_error!(
                io::ErrorKind::InvalidInput,
                "bad datagram"
            ));
        }
        body[0] = abi::NET_OP_DGRAM;
        body[1] = name_b.len() as u8;
        body[2..2 + name_b.len()].copy_from_slice(name_b);
        let at = 2 + name_b.len();
        body[at] = payload.len() as u8;
        body[at + 1..at + 1 + payload.len()].copy_from_slice(payload);
        let n = at + 1 + payload.len();
        let mut reply = [0u8; abi::CHAN_MSG_MAX];
        let got = syscall::syscall6(
            Syscall::Send,
            abi::reserved::net(CapRights::WRITE).bits(),
            body.as_ptr().addr() as u64,
            n as u64,
            reply.as_mut_ptr().addr() as u64,
            reply.len() as u64,
        )
        .map_err(syscall::io_error)?;
        let got = got as usize;
        if got < 8 {
            return Err(io::const_error!(io::ErrorKind::InvalidData, "bad reply"));
        }
        let text = core::str::from_utf8(&reply[8..got.min(reply.len())]).unwrap_or("");
        if text.contains("ok") {
            return Ok(());
        }
        let err = if text.contains("denied") {
            SysError::AccessDenied
        } else if text.contains("timed") {
            SysError::TimedOut
        } else {
            SysError::Unsupported
        };
        Err(syscall::io_error(err))
    }

    /// Receive one datagram from the peer stored on `name`. Read on
    /// that card. This does not take an address, and port 53 is refused.
    pub fn recv_card(name: &str) -> io::Result<Vec<u8>> {
        if !is_name(name) || is_address(name) {
            return Err(io::const_error!(
                io::ErrorKind::InvalidInput,
                "bad datagram"
            ));
        }
        let (reply, got, lane) = net_rpc(abi::NET_OP_RECV, name)?;
        if lane != 0 {
            return Err(io::const_error!(
                io::ErrorKind::InvalidData,
                "unexpected lane"
            ));
        }
        let bytes = &reply[8..got.min(reply.len())];
        let text = core::str::from_utf8(bytes).unwrap_or("");
        if text.starts_with("access denied") {
            return Err(syscall::io_error(SysError::AccessDenied));
        }
        if text.starts_with("timed") {
            return Err(syscall::io_error(SysError::TimedOut));
        }
        if text.starts_with("bad") || text.starts_with("unavailable") || bytes.is_empty() {
            return Err(io::const_error!(io::ErrorKind::InvalidData, "bad reply"));
        }
        Ok(bytes.to_vec())
    }

    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        self.0
    }

    pub fn socket_addr(&self) -> io::Result<SocketAddr> {
        self.0
    }

    pub fn recv_from(&self, _: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        self.0
    }

    pub fn peek_from(&self, _: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        self.0
    }

    pub fn send_to(&self, _: &[u8], _: &SocketAddr) -> io::Result<usize> {
        self.0
    }

    pub fn duplicate(&self) -> io::Result<UdpSocket> {
        self.0
    }

    pub fn set_read_timeout(&self, _: Option<Duration>) -> io::Result<()> {
        self.0
    }

    pub fn set_write_timeout(&self, _: Option<Duration>) -> io::Result<()> {
        self.0
    }

    pub fn read_timeout(&self) -> io::Result<Option<Duration>> {
        self.0
    }

    pub fn write_timeout(&self) -> io::Result<Option<Duration>> {
        self.0
    }

    pub fn set_broadcast(&self, _: bool) -> io::Result<()> {
        self.0
    }

    pub fn broadcast(&self) -> io::Result<bool> {
        self.0
    }

    pub fn set_multicast_loop_v4(&self, _: bool) -> io::Result<()> {
        self.0
    }

    pub fn multicast_loop_v4(&self) -> io::Result<bool> {
        self.0
    }

    pub fn set_multicast_ttl_v4(&self, _: u32) -> io::Result<()> {
        self.0
    }

    pub fn multicast_ttl_v4(&self) -> io::Result<u32> {
        self.0
    }

    pub fn set_multicast_loop_v6(&self, _: bool) -> io::Result<()> {
        self.0
    }

    pub fn multicast_loop_v6(&self) -> io::Result<bool> {
        self.0
    }

    pub fn join_multicast_v4(&self, _: &Ipv4Addr, _: &Ipv4Addr) -> io::Result<()> {
        self.0
    }

    pub fn join_multicast_v6(&self, _: &Ipv6Addr, _: u32) -> io::Result<()> {
        self.0
    }

    pub fn leave_multicast_v4(&self, _: &Ipv4Addr, _: &Ipv4Addr) -> io::Result<()> {
        self.0
    }

    pub fn leave_multicast_v6(&self, _: &Ipv6Addr, _: u32) -> io::Result<()> {
        self.0
    }

    pub fn set_ttl(&self, _: u32) -> io::Result<()> {
        self.0
    }

    pub fn ttl(&self) -> io::Result<u32> {
        self.0
    }

    pub fn take_error(&self) -> io::Result<Option<io::Error>> {
        self.0
    }

    pub fn set_nonblocking(&self, _: bool) -> io::Result<()> {
        self.0
    }

    pub fn recv(&self, _: &mut [u8]) -> io::Result<usize> {
        self.0
    }

    pub fn peek(&self, _: &mut [u8]) -> io::Result<usize> {
        self.0
    }

    pub fn send(&self, _: &[u8]) -> io::Result<usize> {
        self.0
    }

    pub fn connect<A: ToSocketAddrs>(&self, _: A) -> io::Result<()> {
        self.0
    }
}

impl fmt::Debug for UdpSocket {
    fn fmt(&self, _f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0
    }
}

pub struct LookupHost(!);

impl Iterator for LookupHost {
    type Item = SocketAddr;
    fn next(&mut self) -> Option<SocketAddr> {
        self.0
    }
}

pub fn lookup_host(_host: &str, _port: u16) -> io::Result<LookupHost> {
    unsupported()
}
