//! The handful of libc declarations loot-ledger needs.
//!
//! loot-ledger has no crate dependencies, so the C calls that a crate like `libc`
//! would normally provide are declared here directly. The program links against
//! the system libc it already depends on, so this adds nothing to install and
//! nothing to the dependency graph — it just means `cargo build` works with no
//! registry access at all.

#![allow(non_camel_case_types)]

use std::ffi::c_int;

pub type c_uint = u32;
pub type c_ushort = u16;
pub type c_void = std::ffi::c_void;
pub type ssize_t = isize;

pub const AF_PACKET: c_int = 17;
pub const SOCK_RAW: c_int = 3;
pub const SOCK_NONBLOCK: c_int = 0o4000;

/// `ETH_P_ALL`, in host order; use [`htons`] before handing it to the kernel.
pub const ETH_P_ALL: c_int = 0x0003;

pub const SOL_SOCKET: c_int = 1;
pub const SO_RCVBUF: c_int = 8;
pub const SO_RCVTIMEO: c_int = 20;
pub const SO_ATTACH_FILTER: c_int = 26;

pub const SIGINT: c_int = 2;
pub const SIGTERM: c_int = 15;

/// `sockaddr_ll` — the address an AF_PACKET socket is bound to, and the source
/// address `recvfrom` reports.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SockAddrLl {
    pub sll_family: u16,
    pub sll_protocol: u16,
    /// Interface index, or 0 to receive from every interface.
    pub sll_ifindex: c_int,
    pub sll_hatype: u16,
    pub sll_pkttype: u8,
    pub sll_halen: u8,
    pub sll_addr: [u8; 8],
}

impl Default for SockAddrLl {
    fn default() -> Self {
        SockAddrLl {
            sll_family: AF_PACKET as u16,
            sll_protocol: 0,
            sll_ifindex: 0,
            sll_hatype: 0,
            sll_pkttype: 0,
            sll_halen: 0,
            sll_addr: [0; 8],
        }
    }
}

/// `struct sock_fprog` — the shape `SO_ATTACH_FILTER` expects.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SockFprog {
    pub len: c_ushort,
    pub filter: *const crate::capture::bpf::Instruction,
}

/// `struct timeval`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Timeval {
    pub tv_sec: i64,
    pub tv_usec: i64,
}

extern "C" {
    fn socket(domain: c_int, ty: c_int, protocol: c_int) -> c_int;
    fn bind(fd: c_int, addr: *const c_void, addrlen: u32) -> c_int;
    fn setsockopt(fd: c_int, level: c_int, name: c_int, value: *const c_void, optlen: u32)
        -> c_int;
    fn recvfrom(
        fd: c_int,
        buf: *mut c_void,
        len: usize,
        flags: c_int,
        src: *mut SockAddrLl,
        addrlen: *mut u32,
    ) -> ssize_t;
    fn close(fd: c_int) -> c_int;
    fn signal(signum: c_int, handler: extern "C" fn(c_int)) -> usize;
}

/// Convert a port or protocol constant to network byte order.
///
/// Equivalent to C's `htons`: the value is left in the numeric form the
/// kernel expects to find in memory.
pub fn htons(v: u16) -> u16 {
    v.to_be()
}

/// Open a raw packet socket and attach a filter.
pub fn open_packet_socket(
    ifindex: c_int,
    filter: &[crate::capture::bpf::Instruction],
) -> std::io::Result<c_int> {
    // ETH_P_ALL so the kernel delivers every link-layer type; the attached BPF
    // filter is what actually narrows the stream.
    let fd = unsafe {
        socket(
            AF_PACKET,
            SOCK_RAW | SOCK_NONBLOCK,
            htons(ETH_P_ALL as u16) as c_int,
        )
    };

    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }

    if let Err(e) = bind_socket(fd, ifindex) {
        let _ = unsafe { close(fd) };
        return Err(e);
    }

    // A roomy receive buffer keeps bursts from being dropped between reads.
    let bufsize: c_int = 4 * 1024 * 1024;
    unsafe {
        setsockopt(
            fd,
            SOL_SOCKET,
            SO_RCVBUF,
            &bufsize as *const c_int as *const c_void,
            std::mem::size_of::<c_int>() as u32,
        );
    }

    if !filter.is_empty() {
        let fprog = SockFprog {
            len: filter.len() as c_ushort,
            filter: filter.as_ptr(),
        };

        let rc = unsafe {
            setsockopt(
                fd,
                SOL_SOCKET,
                SO_ATTACH_FILTER,
                &fprog as *const SockFprog as *const c_void,
                std::mem::size_of::<SockFprog>() as u32,
            )
        };

        if rc < 0 {
            // Not fatal: without the filter we still work, just with more
            // wakeups. The caller decides how loudly to complain.
            let _ = unsafe { close(fd) };
            return Err(std::io::Error::last_os_error());
        }
    }

    Ok(fd)
}

fn bind_socket(fd: c_int, ifindex: c_int) -> std::io::Result<()> {
    let addr = SockAddrLl {
        sll_ifindex: ifindex,
        ..Default::default()
    };

    let rc = unsafe {
        bind(
            fd,
            &addr as *const SockAddrLl as *const c_void,
            std::mem::size_of::<SockAddrLl>() as u32,
        )
    };

    if rc < 0 {
        return Err(std::io::Error::last_os_error());
    }

    Ok(())
}

/// Read one frame, returning its length and the interface it arrived on.
///
/// Returns `Ok(None)` when the socket has no data within the read timeout,
/// which is the signal to re-check whether the application should stop.
pub fn recv_frame(
    fd: c_int,
    buf: &mut [u8],
    timeout: std::time::Duration,
) -> std::io::Result<Option<(usize, c_int)>> {
    let tv = Timeval {
        tv_sec: timeout.as_secs() as i64,
        tv_usec: timeout.subsec_micros() as i64,
    };

    unsafe {
        setsockopt(
            fd,
            SOL_SOCKET,
            SO_RCVTIMEO,
            &tv as *const Timeval as *const c_void,
            std::mem::size_of::<Timeval>() as u32,
        );
    }

    let mut src = SockAddrLl::default();
    let mut addrlen = std::mem::size_of::<SockAddrLl>() as u32;

    let n = unsafe {
        recvfrom(
            fd,
            buf.as_mut_ptr() as *mut c_void,
            buf.len(),
            0,
            &mut src as *mut SockAddrLl,
            &mut addrlen,
        )
    };

    if n < 0 {
        let err = std::io::Error::last_os_error();
        return match err.kind() {
            // The socket is non-blocking, so this is the normal "nothing yet".
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => Ok(None),
            _ => Err(err),
        };
    }

    Ok(Some((n as usize, src.sll_ifindex)))
}

/// Close a file descriptor.
pub fn close_fd(fd: c_int) {
    unsafe {
        close(fd);
    }
}

/// Install a signal handler.
///
/// loot-ledger's own handler only flips a flag; the main loop notices and shuts
/// down cleanly, so the journal is flushed rather than truncated.
pub fn install_signal_handler(handler: extern "C" fn(c_int)) {
    unsafe {
        signal(SIGINT, handler);
        signal(SIGTERM, handler);
    }
}
