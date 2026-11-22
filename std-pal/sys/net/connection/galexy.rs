//! `std::net` on Galexy is one dial (Milestone 108).
//!
//! `TcpStream::connect` takes a granted name. The kernel asks the seat
//! when the card is missing, then `net` dials. The value is a lane Cap.
//! This file does not parse TCP. rustls stays in the `net` service.
//! `bind`, `listen`, UDP, socket options, and any call that passes an
//! IP or a port return `unsupported`.

use crate::fmt;
use crate::io::{self, BorrowedCursor, IoSlice, IoSliceMut};
use crate::net::{Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, ToSocketAddrs};
use crate::sys::pal::abi::{self, Cap, CapRights, SysError, Syscall};
use crate::sys::pal::{syscall, unsupported};
use crate::time::Duration;

pub struct TcpStream {
    lane: Cap,
}

impl TcpStream {
    /// Dial `name`. An address or a port never reaches `net`.
    pub fn connect(name: &str) -> io::Result<TcpStream> {
        if is_address(name) {
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

/// A colon is a port. Four numeric pieces are an IPv4 address. A
/// string of only digits is a port. None of those is a granted name.
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

/// Same payload `fetch` sends: opcode 2, the name, then `whoami`.
fn dial(name: &str) -> io::Result<Cap> {
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
    payload[0] = abi::NET_OP_DIAL;
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

pub struct TcpListener(!);

impl TcpListener {
    pub fn bind<A: ToSocketAddrs>(_: A) -> io::Result<TcpListener> {
        unsupported()
    }

    pub fn socket_addr(&self) -> io::Result<SocketAddr> {
        self.0
    }

    pub fn accept(&self) -> io::Result<(TcpStream, SocketAddr)> {
        self.0
    }

    pub fn duplicate(&self) -> io::Result<TcpListener> {
        self.0
    }

    pub fn set_ttl(&self, _: u32) -> io::Result<()> {
        self.0
    }

    pub fn ttl(&self) -> io::Result<u32> {
        self.0
    }

    pub fn set_only_v6(&self, _: bool) -> io::Result<()> {
        self.0
    }

    pub fn only_v6(&self) -> io::Result<bool> {
        self.0
    }

    pub fn take_error(&self) -> io::Result<Option<io::Error>> {
        self.0
    }

    pub fn set_nonblocking(&self, _: bool) -> io::Result<()> {
        self.0
    }
}

impl fmt::Debug for TcpListener {
    fn fmt(&self, _f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0
    }
}

pub struct UdpSocket(!);

impl UdpSocket {
    pub fn bind<A: ToSocketAddrs>(_: A) -> io::Result<UdpSocket> {
        unsupported()
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
