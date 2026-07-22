//! Linux `sendmmsg` helpers for batch UDP send (P1.2b).
//!
//! Compiled only on Linux. Other platforms keep the try_send / sequential path.

#![cfg(target_os = "linux")]

use std::io;
use std::mem;
use std::os::fd::RawFd;
use std::ptr;

/// Try to send many datagrams with one `sendmmsg` syscall.
///
/// - Connected socket: pass `None` for `to` (uses connected peer).
/// - Unconnected: pass `Some(sockaddr_storage, socklen)`.
///
/// Returns number of messages successfully queued (may be partial).
/// `WouldBlock` is returned only if zero messages were sent.
pub fn sendmmsg_connected(fd: RawFd, bufs: &[&[u8]]) -> io::Result<usize> {
    sendmmsg_inner(fd, bufs, None)
}

pub fn sendmmsg_to(fd: RawFd, bufs: &[&[u8]], addr: &std::net::SocketAddr) -> io::Result<usize> {
    let (storage, len) = socket_addr_to_storage(addr);
    sendmmsg_inner(fd, bufs, Some((&storage, len)))
}

fn sendmmsg_inner(
    fd: RawFd,
    bufs: &[&[u8]],
    to: Option<(&libc::sockaddr_storage, libc::socklen_t)>,
) -> io::Result<usize> {
    if bufs.is_empty() {
        return Ok(0);
    }
    // Cap batch size to avoid huge stack/heap; callers can loop.
    const MAX_BATCH: usize = 64;
    let n = bufs.len().min(MAX_BATCH);

    // Heap-allocate iovec + mmsghdr to avoid large stack frames.
    let mut iov: Vec<libc::iovec> = Vec::with_capacity(n);
    let mut msgs: Vec<libc::mmsghdr> = Vec::with_capacity(n);

    for buf in bufs.iter().take(n) {
        iov.push(libc::iovec {
            iov_base: buf.as_ptr() as *mut _,
            iov_len: buf.len(),
        });
    }

    for i in 0..n {
        let mut hdr: libc::mmsghdr = unsafe { mem::zeroed() };
        hdr.msg_hdr.msg_iov = &mut iov[i] as *mut _;
        hdr.msg_hdr.msg_iovlen = 1;
        if let Some((storage, len)) = to {
            hdr.msg_hdr.msg_name = storage as *const _ as *mut _;
            hdr.msg_hdr.msg_namelen = len;
        }
        msgs.push(hdr);
    }

    let ret = unsafe { libc::sendmmsg(fd, msgs.as_mut_ptr(), n as libc::c_uint, 0) };
    if ret < 0 {
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::WouldBlock {
            return Err(err);
        }
        return Err(err);
    }
    Ok(ret as usize)
}

fn socket_addr_to_storage(
    addr: &std::net::SocketAddr,
) -> (libc::sockaddr_storage, libc::socklen_t) {
    let mut storage: libc::sockaddr_storage = unsafe { mem::zeroed() };
    let len = match addr {
        std::net::SocketAddr::V4(a) => {
            let sin = libc::sockaddr_in {
                sin_family: libc::AF_INET as libc::sa_family_t,
                sin_port: a.port().to_be(),
                sin_addr: libc::in_addr {
                    s_addr: u32::from_be_bytes(a.ip().octets()),
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

    let mut iov: Vec<libc::iovec> = Vec::with_capacity(n);
    let mut names: Vec<libc::sockaddr_storage> = vec![unsafe { mem::zeroed() }; n];
    let mut msgs: Vec<libc::mmsghdr> = Vec::with_capacity(n);

    for b in bufs.iter_mut().take(n) {
        if b.capacity() < 2048 {
            b.reserve(2048);
        }
        let cap = b.capacity();
        unsafe {
            b.set_len(cap);
        }
        iov.push(libc::iovec {
            iov_base: b.as_mut_ptr() as *mut _,
            iov_len: cap,
        });
    }

    for i in 0..n {
        let mut hdr: libc::mmsghdr = unsafe { mem::zeroed() };
        hdr.msg_hdr.msg_iov = &mut iov[i] as *mut _;
        hdr.msg_hdr.msg_iovlen = 1;
        hdr.msg_hdr.msg_name = &mut names[i] as *mut _ as *mut _;
        hdr.msg_hdr.msg_namelen = mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
        msgs.push(hdr);
    }

    let ret =
        unsafe { libc::recvmmsg(fd, msgs.as_mut_ptr(), n as libc::c_uint, 0, ptr::null_mut()) };
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
    for i in 0..got {
        let len = msgs[i].msg_len as usize;
        unsafe {
            bufs[i].set_len(len);
        }
        let addr = sockaddr_storage_to_addr(&names[i], msgs[i].msg_hdr.msg_namelen);
        out.push((len, addr));
    }
    for i in got..n {
        unsafe {
            bufs[i].set_len(0);
        }
    }
    Ok(out)
}

fn sockaddr_storage_to_addr(
    storage: &libc::sockaddr_storage,
    len: libc::socklen_t,
) -> Option<std::net::SocketAddr> {
    if len == 0 {
        return None;
    }
    match storage.ss_family as i32 {
        x if x == libc::AF_INET as i32 => {
            let sin = unsafe { &*(storage as *const _ as *const libc::sockaddr_in) };
            let ip = std::net::Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
            let port = u16::from_be(sin.sin_port);
            Some(std::net::SocketAddr::V4(std::net::SocketAddrV4::new(
                ip, port,
            )))
        }
        x if x == libc::AF_INET6 as i32 => {
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
