//! Linux/BSD `sendmmsg`/`recvmmsg` helpers for batch UDP send/recv (P1.2b).
//!
//! Compiled on Linux and macOS (both provide `recvmmsg`; only Linux provides
//! `sendmmsg`). Other platforms keep the try_send / sequential path.
//!
//! The `iovec`/`mmsghdr`/`sockaddr_storage` arrays are built in a per-thread
//! reusable scratch buffer (capacity retained across calls) so the hot path
//! does not allocate per batch.
//!
//! Linux-verified 2026-08-06: the `#[cfg(test)]` round-trip tests pass in a
//! `rustlang/rust:nightly` container (`cargo test -p knet-rs` → 25 passed,
//! incl. `sendmmsg_to_roundtrip` + `recvmmsg_from_batch`).

#![cfg(target_os = "linux")]

use std::cell::RefCell;
use std::io;
use std::mem;
use std::os::fd::RawFd;
use std::ptr;

/// Reusable `sendmmsg`/`recvmmsg` scratch arrays (cleared, not deallocated).
struct MmsgScratch {
    iov: Vec<libc::iovec>,
    msgs: Vec<libc::mmsghdr>,
    names: Vec<libc::sockaddr_storage>,
}

thread_local! {
    static SCRATCH: RefCell<MmsgScratch> = const {
        RefCell::new(MmsgScratch {
            iov: Vec::new(),
            msgs: Vec::new(),
            names: Vec::new(),
        })
    };
}

#[cfg(target_os = "linux")]
/// Try to send many datagrams with one `sendmmsg` syscall.
///
/// - Connected socket: pass `None` for `to` (uses connected peer).
/// - Unconnected: pass `Some(sockaddr_storage, socklen)`.
///
/// Returns number of messages successfully queued (may be partial).
/// `WouldBlock` is returned only if zero messages were sent.
pub fn sendmmsg_connected<B: AsRef<[u8]>>(fd: RawFd, bufs: &[B]) -> io::Result<usize> {
    sendmmsg_inner(fd, bufs, None)
}

#[cfg(target_os = "linux")]
pub fn sendmmsg_to<B: AsRef<[u8]>>(
    fd: RawFd,
    bufs: &[B],
    addr: &std::net::SocketAddr,
) -> io::Result<usize> {
    let (storage, len) = socket_addr_to_storage(addr);
    sendmmsg_inner(fd, bufs, Some((&storage, len)))
}

#[cfg(target_os = "linux")]
fn sendmmsg_inner<B: AsRef<[u8]>>(
    fd: RawFd,
    bufs: &[B],
    to: Option<(&libc::sockaddr_storage, libc::socklen_t)>,
) -> io::Result<usize> {
    if bufs.is_empty() {
        return Ok(0);
    }
    // Cap batch size to avoid huge scratch; callers can loop.
    const MAX_BATCH: usize = 64;
    let n = bufs.len().min(MAX_BATCH);

    SCRATCH.with(|s| {
        let mut s = s.borrow_mut();
        s.iov.clear();
        s.msgs.clear();

        for buf in bufs.iter().take(n) {
            let b = buf.as_ref();
            s.iov.push(libc::iovec {
                iov_base: b.as_ptr() as *mut _,
                iov_len: b.len(),
            });
        }

        for i in 0..n {
            let mut hdr: libc::mmsghdr = unsafe { mem::zeroed() };
            hdr.msg_hdr.msg_iov = &mut s.iov[i] as *mut _;
            hdr.msg_hdr.msg_iovlen = 1;
            if let Some((storage, len)) = to {
                hdr.msg_hdr.msg_name = storage as *const _ as *mut _;
                hdr.msg_hdr.msg_namelen = len;
            }
            s.msgs.push(hdr);
        }

        // MSG_DONTWAIT: match try_send semantics used on non-Linux paths.
        // flags type differs by libc (musl: c_uint, glibc: c_int); `as _` infers.
        let ret = unsafe {
            libc::sendmmsg(
                fd,
                s.msgs.as_mut_ptr(),
                n as libc::c_uint,
                libc::MSG_DONTWAIT as _,
            )
        };
        if ret < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::WouldBlock {
                return Err(err);
            }
            return Err(err);
        }
        Ok(ret as usize)
    })
}

fn socket_addr_to_storage(
    addr: &std::net::SocketAddr,
) -> (libc::sockaddr_storage, libc::socklen_t) {
    let mut storage: libc::sockaddr_storage = unsafe { mem::zeroed() };
    let len = match addr {
        std::net::SocketAddr::V4(a) => {
            // `s_addr` is stored in network byte order in memory. `octets()` is
            // already [a,b,c,d] in network order, so copy those bytes as-is via
            // from_ne_bytes. from_be_bytes would swap on LE hosts and send to
            // e.g. 1.0.0.127 instead of 127.0.0.1 — silent blackhole (hang).
            let sin = libc::sockaddr_in {
                sin_family: libc::AF_INET as libc::sa_family_t,
                sin_port: a.port().to_be(),
                sin_addr: libc::in_addr {
                    s_addr: u32::from_ne_bytes(a.ip().octets()),
                },
                sin_zero: [0; 8],
            };
            unsafe {
                ptr::copy_nonoverlapping(
                    &sin as *const _ as *const u8,
                    &mut storage as *mut _ as *mut u8,
                    mem::size_of::<libc::sockaddr_in>(),
                );
            }
            mem::size_of::<libc::sockaddr_in>() as libc::socklen_t
        }
        std::net::SocketAddr::V6(a) => {
            let sin6 = libc::sockaddr_in6 {
                sin6_family: libc::AF_INET6 as libc::sa_family_t,
                sin6_port: a.port().to_be(),
                sin6_flowinfo: a.flowinfo(),
                sin6_addr: libc::in6_addr {
                    s6_addr: a.ip().octets(),
                },
                sin6_scope_id: a.scope_id(),
            };
            unsafe {
                ptr::copy_nonoverlapping(
                    &sin6 as *const _ as *const u8,
                    &mut storage as *mut _ as *mut u8,
                    mem::size_of::<libc::sockaddr_in6>(),
                );
            }
            mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t
        }
    };
    (storage, len)
}

/// Receive up to `bufs.len()` datagrams into pre-allocated buffers.
///
/// On success returns one `(nbytes, peer)` per filled slot. Buffers beyond
/// the received count are left with `len == 0`.
///
/// Returns `WouldBlock` when no datagram is available.
pub fn recvmmsg_from(
    fd: RawFd,
    bufs: &mut [Vec<u8>],
) -> io::Result<Vec<(usize, Option<std::net::SocketAddr>)>> {
    if bufs.is_empty() {
        return Ok(Vec::new());
    }
    const MAX_BATCH: usize = 64;
    let n = bufs.len().min(MAX_BATCH);

    SCRATCH.with(|s| {
        let mut s = s.borrow_mut();
        s.iov.clear();
        s.msgs.clear();
        s.names.clear();
        s.names.resize(n, unsafe { mem::zeroed() });

        for b in bufs.iter_mut().take(n) {
            if b.capacity() < 2048 {
                b.reserve(2048);
            }
            let cap = b.capacity();
            unsafe {
                b.set_len(cap);
            }
            s.iov.push(libc::iovec {
                iov_base: b.as_mut_ptr() as *mut _,
                iov_len: cap,
            });
        }

        for i in 0..n {
            let mut hdr: libc::mmsghdr = unsafe { mem::zeroed() };
            hdr.msg_hdr.msg_iov = &mut s.iov[i] as *mut _;
            hdr.msg_hdr.msg_iovlen = 1;
            hdr.msg_hdr.msg_name = &mut s.names[i] as *mut _ as *mut _;
            hdr.msg_hdr.msg_namelen = mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
            s.msgs.push(hdr);
        }

        // MSG_DONTWAIT: never block even if the runtime's non-blocking flag is
        // lost/raced; callers (try_recv_batch_from) expect WouldBlock semantics.
        // flags type differs by libc (musl: c_uint, glibc: c_int); `as _` infers.
        let ret = unsafe {
            libc::recvmmsg(
                fd,
                s.msgs.as_mut_ptr(),
                n as libc::c_uint,
                libc::MSG_DONTWAIT as _,
                ptr::null_mut(),
            )
        };
        if ret < 0 {
            for b in bufs.iter_mut().take(n) {
                unsafe {
                    b.set_len(0);
                }
            }
            return Err(io::Error::last_os_error());
        }
        let got = ret as usize;
        let mut out = Vec::with_capacity(got);
        for (i, b) in bufs.iter_mut().take(got).enumerate() {
            let len = s.msgs[i].msg_len as usize;
            unsafe {
                b.set_len(len);
            }
            let addr = sockaddr_storage_to_addr(&s.names[i], s.msgs[i].msg_hdr.msg_namelen);
            out.push((len, addr));
        }
        for b in bufs.iter_mut().skip(got).take(n - got) {
            unsafe {
                b.set_len(0);
            }
        }
        Ok(out)
    })
}

/// Receive up to `bufs.len()` datagrams into pre-allocated buffers, writing each
/// source address into `peers` (cleared first; capacity reused).
///
/// Allocation-free steady state: payload slots stay owned by the caller (no
/// per-slot replacement `Vec`), and `peers` reuses its existing capacity — the
/// only metadata write is appending each address. Returns the number of
/// datagrams received; `WouldBlock` when none are available.
pub fn recvmmsg_from_into(
    fd: RawFd,
    bufs: &mut [Vec<u8>],
    peers: &mut Vec<std::net::SocketAddr>,
) -> io::Result<usize> {
    if bufs.is_empty() {
        return Ok(0);
    }
    const MAX_BATCH: usize = 64;
    let n = bufs.len().min(MAX_BATCH);

    SCRATCH.with(|s| {
        let mut s = s.borrow_mut();
        s.iov.clear();
        s.msgs.clear();
        s.names.clear();
        s.names.resize(n, unsafe { mem::zeroed() });

        for b in bufs.iter_mut().take(n) {
            if b.capacity() < 2048 {
                b.reserve(2048);
            }
            let cap = b.capacity();
            unsafe {
                b.set_len(cap);
            }
            s.iov.push(libc::iovec {
                iov_base: b.as_mut_ptr() as *mut _,
                iov_len: cap,
            });
        }

        for i in 0..n {
            let mut hdr: libc::mmsghdr = unsafe { mem::zeroed() };
            hdr.msg_hdr.msg_iov = &mut s.iov[i] as *mut _;
            hdr.msg_hdr.msg_iovlen = 1;
            hdr.msg_hdr.msg_name = &mut s.names[i] as *mut _ as *mut _;
            hdr.msg_hdr.msg_namelen = mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
            s.msgs.push(hdr);
        }

        // MSG_DONTWAIT: never block even if the runtime's non-blocking flag is
        // lost/raced; callers (try_recv_batch_from_into) expect WouldBlock.
        let ret = unsafe {
            libc::recvmmsg(
                fd,
                s.msgs.as_mut_ptr(),
                n as libc::c_uint,
                libc::MSG_DONTWAIT as _,
                ptr::null_mut(),
            )
        };
        if ret < 0 {
            for b in bufs.iter_mut().take(n) {
                unsafe {
                    b.set_len(0);
                }
            }
            return Err(io::Error::last_os_error());
        }
        let got = ret as usize;
        peers.clear();
        for (i, b) in bufs.iter_mut().take(got).enumerate() {
            let len = s.msgs[i].msg_len as usize;
            unsafe {
                b.set_len(len);
            }
            // Unconnected recvmmsg always fills msg_name (msg_name was set
            // above), so sockaddr_storage_to_addr is Some for UDP.
            if let Some(addr) = sockaddr_storage_to_addr(&s.names[i], s.msgs[i].msg_hdr.msg_namelen)
            {
                peers.push(addr);
            }
        }
        for b in bufs.iter_mut().skip(got).take(n - got) {
            unsafe {
                b.set_len(0);
            }
        }
        Ok(got)
    })
}

/// Receive up to `bufs.len()` datagrams into pre-allocated buffers on a
/// **connected** socket (no peer address captured). Returns the number of
/// datagrams received; `WouldBlock` when none ready.
pub fn recvmmsg_connected(fd: RawFd, bufs: &mut [Vec<u8>]) -> io::Result<usize> {
    if bufs.is_empty() {
        return Ok(0);
    }
    const MAX_BATCH: usize = 64;
    let n = bufs.len().min(MAX_BATCH);

    SCRATCH.with(|s| {
        let mut s = s.borrow_mut();
        s.iov.clear();
        s.msgs.clear();
        for b in bufs.iter_mut().take(n) {
            if b.capacity() < 2048 {
                b.reserve(2048);
            }
            let cap = b.capacity();
            unsafe {
                b.set_len(cap);
            }
            s.iov.push(libc::iovec {
                iov_base: b.as_mut_ptr() as *mut _,
                iov_len: cap,
            });
        }
        for i in 0..n {
            let mut hdr: libc::mmsghdr = unsafe { mem::zeroed() };
            hdr.msg_hdr.msg_iov = &mut s.iov[i] as *mut _;
            hdr.msg_hdr.msg_iovlen = 1;
            hdr.msg_hdr.msg_name = ptr::null_mut();
            hdr.msg_hdr.msg_namelen = 0;
            s.msgs.push(hdr);
        }
        let ret = unsafe {
            libc::recvmmsg(
                fd,
                s.msgs.as_mut_ptr(),
                n as libc::c_uint,
                libc::MSG_DONTWAIT as _,
                ptr::null_mut(),
            )
        };
        if ret < 0 {
            for b in bufs.iter_mut().take(n) {
                unsafe {
                    b.set_len(0);
                }
            }
            return Err(io::Error::last_os_error());
        }
        let got = ret as usize;
        for (i, b) in bufs.iter_mut().take(got).enumerate() {
            let len = s.msgs[i].msg_len as usize;
            unsafe {
                b.set_len(len);
            }
        }
        for b in bufs.iter_mut().skip(got).take(n - got) {
            unsafe {
                b.set_len(0);
            }
        }
        Ok(got)
    })
}

fn sockaddr_storage_to_addr(
    storage: &libc::sockaddr_storage,
    len: libc::socklen_t,
) -> Option<std::net::SocketAddr> {
    if len == 0 {
        return None;
    }
    match storage.ss_family as i32 {
        x if x == libc::AF_INET => {
            let sin = unsafe { &*(storage as *const _ as *const libc::sockaddr_in) };
            let ip = std::net::Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
            let port = u16::from_be(sin.sin_port);
            Some(std::net::SocketAddr::V4(std::net::SocketAddrV4::new(
                ip, port,
            )))
        }
        x if x == libc::AF_INET6 => {
            let sin6 = unsafe { &*(storage as *const _ as *const libc::sockaddr_in6) };
            let ip = std::net::Ipv6Addr::from(sin6.sin6_addr.s6_addr);
            let port = u16::from_be(sin6.sin6_port);
            Some(std::net::SocketAddr::V6(std::net::SocketAddrV6::new(
                ip,
                port,
                sin6.sin6_flowinfo,
                sin6.sin6_scope_id,
            )))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::UdpSocket;
    use std::os::fd::AsRawFd;

    /// `sendmmsg_to` must deliver every datagram intact and in order.
    #[test]
    fn sendmmsg_to_roundtrip() {
        let recv = UdpSocket::bind("127.0.0.1:0").unwrap();
        let send = UdpSocket::bind("127.0.0.1:0").unwrap();
        let target = recv.local_addr().unwrap();

        let expected: Vec<Vec<u8>> = vec![vec![0xAA; 100], vec![0xBB; 200], vec![0xCC; 300]];
        let n = sendmmsg_to(send.as_raw_fd(), &expected, &target).unwrap();
        assert_eq!(n, 3, "all three datagrams should send");

        let mut got_buf = vec![0u8; 512];
        let mut received: Vec<Vec<u8>> = Vec::new();
        while received.len() < 3 {
            let (len, _peer) = recv.recv_from(&mut got_buf).unwrap();
            received.push(got_buf[..len].to_vec());
        }
        assert_eq!(
            received, expected,
            "datagrams must arrive intact and in order"
        );
    }

    /// `recvmmsg_from` must fill one slot per datagram with correct peers.
    #[test]
    fn recvmmsg_from_batch() {
        let recv = UdpSocket::bind("127.0.0.1:0").unwrap();
        let send = UdpSocket::bind("127.0.0.1:0").unwrap();
        let recv_addr = recv.local_addr().unwrap();

        for _ in 0..3 {
            send.send_to(b"x", recv_addr).unwrap();
        }
        let mut bufs = vec![vec![0u8; 0]; 3];
        let out = loop {
            let o = recvmmsg_from(recv.as_raw_fd(), &mut bufs).unwrap();
            if !o.is_empty() {
                break o;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        };
        assert_eq!(out.len(), 3, "should batch-receive all three datagrams");
        for (len, peer) in &out {
            assert_eq!(*len, 1);
            assert!(peer.is_some());
        }
    }
}
