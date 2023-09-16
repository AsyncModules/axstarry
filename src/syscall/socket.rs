extern crate alloc;
use alloc::vec::Vec;
use core::{
    mem::size_of,
    ptr::copy_nonoverlapping,
    slice::{from_raw_parts, from_raw_parts_mut},
    sync::atomic::{AtomicBool, AtomicU64},
};

use alloc::{string::String, sync::Arc};
use axerrno::{AxError, AxResult};
use axfs::api::{FileIO, FileIOType, OpenFlags};
use axio::{Read, Write};
use axlog::{debug, error, info, warn};
use axnet::{
    from_core_sockaddr, into_core_sockaddr, poll_interfaces, IpAddr, SocketAddr, TcpSocket,
    UdpSocket,
};
use axprocess::current_process;
use axsync::Mutex;
use num_enum::TryFromPrimitive;

use crate::syscall::ErrorNo;

use super::flags::TimeVal;

pub const SOCKET_TYPE_MASK: usize = 0xFF;

#[derive(TryFromPrimitive, Clone)]
#[repr(usize)]
#[allow(non_camel_case_types)]
pub enum Domain {
    AF_UNIX = 1,
    AF_INET = 2,
}

#[derive(TryFromPrimitive, PartialEq, Eq, Clone, Debug)]
#[repr(usize)]
#[allow(non_camel_case_types)]
pub enum SocketType {
    /// Provides sequenced, reliable, two-way, connection-based byte streams.
    /// An out-of-band data transmission mechanism may be supported.
    SOCK_STREAM = 1,
    /// Supports datagrams (connectionless, unreliable messages of a fixed maximum length).
    SOCK_DGRAM = 2,
    /// Provides raw network protocol access.
    SOCK_RAW = 3,
    /// Provides a reliable datagram layer that does not guarantee ordering.
    SOCK_RDM = 4,
    /// Provides a sequenced, reliable, two-way connection-based data
    /// transmission path for datagrams of fixed maximum length;
    /// a consumer is required to read an entire packet with each input system call.
    SOCK_SEQPACKET = 5,
    /// Datagram Congestion Control Protocol socket
    SOCK_DCCP = 6,
    /// Obsolete and should not be used in new programs.
    SOCK_PACKET = 10,
}

/// Set O_NONBLOCK flag on the open fd
pub const SOCK_NONBLOCK: usize = 0x800;
/// Set FD_CLOEXEC flag on the new fd
pub const SOCK_CLOEXEC: usize = 0x80000;

#[derive(TryFromPrimitive, Debug)]
#[repr(usize)]
pub enum SocketOptionLevel {
    IP = 0,
    SOCKET = 1,
    TCP = 6,
}

#[derive(TryFromPrimitive, Debug)]
#[repr(usize)]
#[allow(non_camel_case_types)]
pub enum SocketOption {
    SO_REUSEADDR = 2,
    SO_ERROR = 4,
    SO_DONTROUTE = 5,
    SO_SNDBUF = 7,
    SO_RCVBUF = 8,
    SO_KEEPALIVE = 9,
    SO_RCVTIMEO = 20,
}

#[derive(TryFromPrimitive, PartialEq)]
#[repr(usize)]
#[allow(non_camel_case_types)]
pub enum TcpSocketOption {
    TCP_NODELAY = 1, // disable nagle algorithm and flush
    TCP_MAXSEG = 2,
    TCP_INFO = 11,
    TCP_CONGESTION = 13,
}

impl SocketOption {
    fn set(&self, socket: &Socket, opt: &[u8]) {
        match self {
            SocketOption::SO_REUSEADDR => {
                if opt.len() < 4 {
                    panic!("can't read a int from socket opt value");
                }

                let opt_value = i32::from_ne_bytes(<[u8; 4]>::try_from(&opt[0..4]).unwrap());

                socket.set_reuse_addr(opt_value != 0);
                // socket.reuse_addr = opt_value != 0;
            }
            SocketOption::SO_DONTROUTE => {
                if opt.len() < 4 {
                    panic!("can't read a int from socket opt value");
                }

                let opt_value = i32::from_ne_bytes(<[u8; 4]>::try_from(&opt[0..4]).unwrap());

                socket.set_reuse_addr(opt_value != 0);
                // socket.reuse_addr = opt_value != 0;
            }
            SocketOption::SO_SNDBUF => {
                if opt.len() < 4 {
                    panic!("can't read a int from socket opt value");
                }

                let opt_value = i32::from_ne_bytes(<[u8; 4]>::try_from(&opt[0..4]).unwrap());

                socket.set_send_buf_size(opt_value as u64);
                // socket.send_buf_size = opt_value as usize;
            }
            SocketOption::SO_RCVBUF => {
                if opt.len() < 4 {
                    panic!("can't read a int from socket opt value");
                }

                let opt_value = i32::from_ne_bytes(<[u8; 4]>::try_from(&opt[0..4]).unwrap());

                socket.set_recv_buf_size(opt_value as u64);
                // socket.recv_buf_size = opt_value as usize;
            }
            SocketOption::SO_KEEPALIVE => {
                if opt.len() < 4 {
                    panic!("can't read a int from socket opt value");
                }

                let opt_value = i32::from_ne_bytes(<[u8; 4]>::try_from(&opt[0..4]).unwrap());

                let interval = if opt_value != 0 {
                    Some(axnet::Duration::from_secs(45))
                } else {
                    None
                };

                let mut inner = socket.inner.lock();

                match &mut (*inner) {
                    SocketInner::Udp(_) => {
                        warn!("[setsockopt()] set SO_KEEPALIVE on udp socket, ignored")
                    }
                    SocketInner::Tcp(s) => s.with_socket_mut(|s| match s {
                        Some(s) => s.set_keep_alive(interval),
                        None => warn!(
                            "[setsockopt()] set keep-alive for tcp socket not created, ignored"
                        ),
                    }),
                };
                drop(inner);
                socket.set_recv_buf_size(opt_value as u64);
                // socket.recv_buf_size = opt_value as usize;
            }
            SocketOption::SO_RCVTIMEO => {
                if opt.len() < size_of::<TimeVal>() {
                    panic!("can't read a timeval from socket opt value");
                }

                let timeout = unsafe { *(opt.as_ptr() as *const TimeVal) };
                socket.set_recv_timeout(if timeout.sec == 0 && timeout.usec == 0 {
                    None
                } else {
                    Some(timeout)
                });
            }
            SocketOption::SO_ERROR => {
                panic!("can't set SO_ERROR");
            }
        }
    }

    fn get(&self, socket: &Socket, opt_value: *mut u8, opt_len: *mut u32) {
        let buf_len = unsafe { *opt_len } as usize;

        match self {
            SocketOption::SO_REUSEADDR => {
                let value: i32 = if socket.get_reuse_addr() { 1 } else { 0 };

                if buf_len < 4 {
                    panic!("can't write a int to socket opt value");
                }

                unsafe {
                    copy_nonoverlapping(&value.to_ne_bytes() as *const u8, opt_value, 4);
                    *opt_len = 4;
                }
            }
            SocketOption::SO_DONTROUTE => {
                if buf_len < 4 {
                    panic!("can't write a int to socket opt value");
                }

                let size: i32 = if socket.dont_route { 1 } else { 0 };

                unsafe {
                    copy_nonoverlapping(&size.to_ne_bytes() as *const u8, opt_value, 4);
                    *opt_len = 4;
                }
            }
            SocketOption::SO_SNDBUF => {
                if buf_len < 4 {
                    panic!("can't write a int to socket opt value");
                }

                let size: i32 = socket.get_send_buf_size() as i32;

                unsafe {
                    copy_nonoverlapping(&size.to_ne_bytes() as *const u8, opt_value, 4);
                    *opt_len = 4;
                }
            }
            SocketOption::SO_RCVBUF => {
                if buf_len < 4 {
                    panic!("can't write a int to socket opt value");
                }

                let size: i32 = socket.get_recv_buf_size() as i32;

                unsafe {
                    copy_nonoverlapping(&size.to_ne_bytes() as *const u8, opt_value, 4);
                    *opt_len = 4;
                }
            }
            SocketOption::SO_KEEPALIVE => {
                if buf_len < 4 {
                    panic!("can't write a int to socket opt value");
                }

                let mut inner = socket.inner.lock();
                let keep_alive: i32 = match &mut *inner {
                    SocketInner::Udp(_) => {
                        warn!("[getsockopt()] get SO_KEEPALIVE on udp socket, returning false");
                        0
                    }
                    SocketInner::Tcp(s) => s.with_socket(|s| match s {
                        Some(s) => if s.keep_alive().is_some() { 1 } else { 0 },
                        None => {warn!(
                            "[setsockopt()] set keep-alive for tcp socket not created, returning false"
                        );
                            0},
                    }),
                };
                drop(inner);

                unsafe {
                    copy_nonoverlapping(&keep_alive.to_ne_bytes() as *const u8, opt_value, 4);
                    *opt_len = 4;
                }
            }
            SocketOption::SO_RCVTIMEO => {
                if buf_len < size_of::<TimeVal>() {
                    panic!("can't write a timeval to socket opt value");
                }

                unsafe {
                    match socket.get_recv_timeout() {
                        Some(time) => copy_nonoverlapping(
                            (&time) as *const TimeVal,
                            opt_value as *mut TimeVal,
                            1,
                        ),
                        None => {
                            copy_nonoverlapping(&0u8 as *const u8, opt_value, size_of::<TimeVal>())
                        }
                    }

                    *opt_len = size_of::<TimeVal>() as u32;
                }
            }
            SocketOption::SO_ERROR => {
                // 当前没有存储错误列表，因此不做处理
            }
        }
    }
}

impl TcpSocketOption {
    fn set(&self, raw_socket: &Socket, opt: &[u8]) {
        let mut inner = raw_socket.inner.lock();
        let socket = match &mut *inner {
            SocketInner::Tcp(ref mut s) => s,
            _ => panic!("calling tcp option on a wrong type of socket"),
        };

        match self {
            TcpSocketOption::TCP_NODELAY => {
                if opt.len() < 4 {
                    panic!("can't read a int from socket opt value");
                }
                let opt_value = i32::from_ne_bytes(<[u8; 4]>::try_from(&opt[0..4]).unwrap());

                let _ = socket.set_nagle_enabled(opt_value == 0);
                let _ = socket.flush();
            }
            TcpSocketOption::TCP_INFO => panic!("[setsockopt()] try to set TCP_INFO"),
            TcpSocketOption::TCP_CONGESTION => {
                raw_socket.set_congestion(String::from_utf8(Vec::from(opt)).unwrap())
            }
            _ => {
                unimplemented!()
            }
        }
    }

    fn get(&self, raw_socket: &Socket, opt_value: *mut u8, opt_len: *mut u32) {
        let inner = raw_socket.inner.lock();
        let socket = match &*inner {
            SocketInner::Tcp(ref s) => s,
            _ => panic!("calling tcp option on a wrong type of socket"),
        };

        let buf_len = unsafe { *opt_len };

        match self {
            TcpSocketOption::TCP_NODELAY => {
                if buf_len < 4 {
                    panic!("can't write a int to socket opt value");
                }

                let value: i32 = if socket.nagle_enabled() { 0 } else { 1 };

                let value = value.to_ne_bytes();

                unsafe {
                    copy_nonoverlapping(&value as *const u8, opt_value, 4);
                    *opt_len = 4;
                }
            }
            TcpSocketOption::TCP_MAXSEG => {
                let len = size_of::<usize>();

                let value: usize = 1500;

                unsafe {
                    copy_nonoverlapping(&value as *const usize as *const u8, opt_value, len);
                    *opt_len = len as u32;
                };
            }
            TcpSocketOption::TCP_INFO => {}
            TcpSocketOption::TCP_CONGESTION => {
                let bytes = raw_socket.get_congestion();
                let bytes = bytes.as_bytes();

                unsafe {
                    copy_nonoverlapping(bytes.as_ptr(), opt_value, bytes.len());
                    *opt_len = bytes.len() as u32;
                };
            }
        }
    }
}

/// 包装内部的不同协议 Socket
/// 类似 FileDesc，impl FileIO 后加入fd_list
pub struct Socket {
    #[allow(dead_code)]
    domain: Domain,
    socket_type: SocketType,
    inner: Mutex<SocketInner>,
    close_exec: bool,
    recv_timeout: Mutex<Option<TimeVal>>,

    // fake options
    reuse_addr: AtomicBool,
    dont_route: bool,
    send_buf_size: AtomicU64,
    recv_buf_size: AtomicU64,
    congestion: Mutex<String>,
}

pub enum SocketInner {
    Tcp(TcpSocket),
    Udp(UdpSocket),
}

impl Socket {
    fn get_recv_timeout(&self) -> Option<TimeVal> {
        *self.recv_timeout.lock()
    }
    fn get_reuse_addr(&self) -> bool {
        self.reuse_addr.load(core::sync::atomic::Ordering::Acquire)
    }

    fn get_send_buf_size(&self) -> u64 {
        self.send_buf_size
            .load(core::sync::atomic::Ordering::Acquire)
    }

    fn get_recv_buf_size(&self) -> u64 {
        self.recv_buf_size
            .load(core::sync::atomic::Ordering::Acquire)
    }

    fn get_congestion(&self) -> String {
        self.congestion.lock().clone()
    }

    fn set_recv_timeout(&self, val: Option<TimeVal>) {
        *self.recv_timeout.lock() = val;
    }

    fn set_reuse_addr(&self, flag: bool) {
        self.reuse_addr
            .store(flag, core::sync::atomic::Ordering::Release)
    }

    fn set_send_buf_size(&self, size: u64) {
        self.send_buf_size
            .store(size, core::sync::atomic::Ordering::Release)
    }

    fn set_recv_buf_size(&self, size: u64) {
        self.recv_buf_size
            .store(size, core::sync::atomic::Ordering::Release)
    }

    fn set_congestion(&self, congestion: String) {
        *self.congestion.lock() = congestion;
    }

    fn new(domain: Domain, socket_type: SocketType) -> Self {
        let inner = match socket_type {
            SocketType::SOCK_STREAM | SocketType::SOCK_SEQPACKET => {
                SocketInner::Tcp(TcpSocket::new())
            }
            SocketType::SOCK_DGRAM => SocketInner::Udp(UdpSocket::new()),
            _ => unimplemented!(),
        };
        Self {
            domain,
            socket_type,
            inner: Mutex::new(inner),
            close_exec: false,
            recv_timeout: Mutex::new(None),
            reuse_addr: AtomicBool::new(false),
            dont_route: false,
            send_buf_size: AtomicU64::new(64 * 1024),
            recv_buf_size: AtomicU64::new(64 * 1024),
            congestion: Mutex::new(String::from("reno")),
        }
    }

    pub fn set_nonblocking(&self, nonblocking: bool) {
        let inner = self.inner.lock();

        match &*inner {
            SocketInner::Tcp(s) => s.set_nonblocking(nonblocking),
            SocketInner::Udp(s) => s.set_nonblocking(nonblocking),
        }
    }

    pub fn is_nonblocking(&self) -> bool {
        let inner = self.inner.lock();
        match &*inner {
            SocketInner::Tcp(s) => s.is_nonblocking(),
            SocketInner::Udp(s) => s.is_nonblocking(),
        }
    }

    /// Socket may send or recv.
    pub fn is_connected(&self) -> bool {
        let inner = self.inner.lock();
        match &*inner {
            SocketInner::Tcp(s) => s.is_connected(),
            SocketInner::Udp(s) => s.with_socket(|s| s.is_open()),
        }
    }

    /// Return bound address.
    pub fn name(&self) -> AxResult<SocketAddr> {
        let inner = self.inner.lock();
        match &*inner {
            SocketInner::Tcp(s) => s.local_addr(),
            SocketInner::Udp(s) => s.local_addr(),
        }
        .map(|addr| from_core_sockaddr(addr))
    }

    /// Return peer address.
    pub fn peer_name(&self) -> AxResult<SocketAddr> {
        let inner = self.inner.lock();
        match &*inner {
            SocketInner::Tcp(s) => s.peer_addr(),
            SocketInner::Udp(s) => s.peer_addr(),
        }
        .map(|addr| from_core_sockaddr(addr))
    }

    pub fn bind(&self, addr: SocketAddr) -> AxResult {
        let inner = self.inner.lock();
        match &*inner {
            SocketInner::Tcp(s) => s.bind(into_core_sockaddr(addr)),
            SocketInner::Udp(s) => s.bind(into_core_sockaddr(addr)),
        }
    }

    /// Listen to the bound address.
    ///
    /// Only support socket with type SOCK_STREAM or SOCK_SEQPACKET
    ///
    /// Err(Unsupported): EOPNOTSUPP
    pub fn listen(&self) -> AxResult {
        if self.socket_type != SocketType::SOCK_STREAM
            && self.socket_type != SocketType::SOCK_SEQPACKET
        {
            return Err(AxError::Unsupported);
        }
        let inner = self.inner.lock();
        match &*inner {
            SocketInner::Tcp(s) => s.listen(),
            SocketInner::Udp(_) => Err(AxError::Unsupported),
        }
    }

    pub fn accept(&self) -> AxResult<(Self, SocketAddr)> {
        if self.socket_type != SocketType::SOCK_STREAM
            && self.socket_type != SocketType::SOCK_SEQPACKET
        {
            return Err(AxError::Unsupported);
        }
        let inner = self.inner.lock();
        let new_socket = match &*inner {
            SocketInner::Tcp(s) => s.accept()?,
            SocketInner::Udp(_) => Err(AxError::Unsupported)?,
        };
        let addr = new_socket.peer_addr()?;

        Ok((
            Self {
                domain: self.domain.clone(),
                socket_type: self.socket_type.clone(),
                inner: Mutex::new(SocketInner::Tcp(new_socket)),
                close_exec: false,
                recv_timeout: Mutex::new(None),
                reuse_addr: AtomicBool::new(false),
                dont_route: false,
                send_buf_size: AtomicU64::new(64 * 1024),
                recv_buf_size: AtomicU64::new(64 * 1024),
                congestion: Mutex::new(String::from("reno")),
            },
            from_core_sockaddr(addr),
        ))
    }

    pub fn connect(&self, addr: SocketAddr) -> AxResult {
        let inner = self.inner.lock();
        match &*inner {
            SocketInner::Tcp(s) => s.connect(into_core_sockaddr(addr)),
            SocketInner::Udp(s) => s.connect(into_core_sockaddr(addr)),
        }
    }
    #[allow(unused)]
    pub fn is_bound(&self) -> bool {
        let inner = self.inner.lock();
        match &*inner {
            SocketInner::Tcp(s) => s.local_addr().is_ok(),
            SocketInner::Udp(s) => s.local_addr().is_ok(),
        }
    }
    #[allow(unused)]
    pub fn sendto(&self, buf: &[u8], addr: SocketAddr) -> AxResult<usize> {
        let inner = self.inner.lock();
        match &*inner {
            SocketInner::Tcp(s) => s.send(buf),
            SocketInner::Udp(s) => s.send_to(buf, into_core_sockaddr(addr)),
        }
    }

    pub fn recv_from(&self, buf: &mut [u8]) -> AxResult<(usize, SocketAddr)> {
        let inner = self.inner.lock();
        match &*inner {
            SocketInner::Tcp(s) => {
                let addr = s.peer_addr()?;

                match self.get_recv_timeout() {
                    Some(time) => s.recv_timeout(buf, time.to_ticks()),
                    None => s.recv(buf),
                }
                .map(|len| (len, from_core_sockaddr(addr)))
            }
            SocketInner::Udp(s) => match self.get_recv_timeout() {
                Some(time) => s
                    .recv_from_timeout(buf, time.to_ticks())
                    .map(|(val, addr)| (val, from_core_sockaddr(addr))),
                None => s
                    .recv_from(buf)
                    .map(|(val, addr)| (val, from_core_sockaddr(addr))),
            },
        }
    }

    /// For shutdown(fd, SHUT_WR)
    pub fn shutdown(&self) {
        let mut inner = self.inner.lock();
        let _ = match &mut *inner {
            SocketInner::Udp(s) => {
                let _ = s.shutdown();
            }
            SocketInner::Tcp(s) => s.close(),
        };
    }

    /// For shutdown(fd, SHUT_RDWR)
    pub fn abort(&self) {
        let mut inner = self.inner.lock();
        match &mut *inner {
            SocketInner::Udp(s) => {
                let _ = s.shutdown();
            }
            SocketInner::Tcp(s) => s.with_socket_mut(|s| {
                if let Some(s) = s {
                    s.abort();
                }
            }),
        }
    }
}

impl FileIO for Socket {
    fn read(&self, buf: &mut [u8]) -> AxResult<usize> {
        let mut inner = self.inner.lock();
        match &mut *inner {
            SocketInner::Tcp(s) => s.read(buf),
            SocketInner::Udp(s) => s.read(buf),
        }
    }

    fn write(&self, buf: &[u8]) -> AxResult<usize> {
        let mut inner = self.inner.lock();
        match &mut *inner {
            SocketInner::Tcp(s) => s.write(buf),
            SocketInner::Udp(s) => s.write(buf),
        }
    }

    fn flush(&self) -> AxResult {
        Err(AxError::Unsupported)
    }

    fn readable(&self) -> bool {
        poll_interfaces();
        let inner = self.inner.lock();
        match &*inner {
            SocketInner::Tcp(s) => s.poll().map_or(false, |p| p.readable),
            SocketInner::Udp(s) => s.poll().map_or(false, |p| p.readable),
        }
    }

    fn writable(&self) -> bool {
        poll_interfaces();
        let inner = self.inner.lock();
        match &*inner {
            SocketInner::Tcp(s) => s.poll().map_or(false, |p| p.writable),
            SocketInner::Udp(s) => s.poll().map_or(false, |p| p.writable),
        }
    }

    fn executable(&self) -> bool {
        false
    }

    fn get_type(&self) -> FileIOType {
        FileIOType::Socket
    }

    fn get_status(&self) -> OpenFlags {
        let mut flags = OpenFlags::default();

        if self.close_exec {
            flags = flags | OpenFlags::CLOEXEC;
        }

        if self.is_nonblocking() {
            flags = flags | OpenFlags::NON_BLOCK;
        }

        flags
    }

    fn set_status(&self, flags: OpenFlags) -> bool {
        self.set_nonblocking(flags.contains(OpenFlags::NON_BLOCK));

        true
    }

    fn ready_to_read(&self) -> bool {
        self.readable()
    }

    fn ready_to_write(&self) -> bool {
        self.writable()
    }
}

pub unsafe fn socket_address_from(addr: *const u8) -> SocketAddr {
    let addr = addr as *const u16;
    let domain = Domain::try_from(*addr as usize).expect("Unsupported Domain (Address Family)");
    match domain {
        Domain::AF_UNIX => unimplemented!(),
        Domain::AF_INET => {
            let port = u16::from_be(*addr.add(1));
            let a = (*(addr.add(2) as *const u32)).to_le_bytes();

            let addr = IpAddr::v4(a[0], a[1], a[2], a[3]);
            SocketAddr { addr, port }
        }
    }
}

/// Only support INET (ipv4)
///
/// ipv4 socket address buffer:
/// socket_domain (address_family) u16
/// port u16 (big endian)
/// addr u32 (big endian)
///
/// TODO: Returns error if buf or buf_len is in invalid memory
pub unsafe fn socket_address_to(addr: SocketAddr, buf: *mut u8, buf_len: *mut u32) -> AxResult {
    let mut tot_len = *buf_len as usize;

    *buf_len = 8;

    // 写入 AF_INET
    if tot_len == 0 {
        return Ok(());
    }
    let domain = (Domain::AF_INET as u16).to_ne_bytes();
    let write_len = tot_len.min(2);
    copy_nonoverlapping(domain.as_ptr(), buf, write_len);
    let buf = buf.add(write_len);
    tot_len -= write_len;

    // 写入 port
    if tot_len == 0 {
        return Ok(());
    }
    let port = &addr.port.to_be_bytes();
    let write_len = tot_len.min(2);
    copy_nonoverlapping(port.as_ptr(), buf, write_len);
    let buf = buf.add(write_len);
    tot_len -= write_len;

    // 写入 address
    if tot_len == 0 {
        return Ok(());
    }
    let address = &addr.addr.as_bytes();
    let write_len = tot_len.min(4);
    copy_nonoverlapping(address.as_ptr(), buf, write_len);

    Ok(())
}

pub fn syscall_socket(domain: usize, s_type: usize, _protocol: usize) -> isize {
    let Ok(domain) = Domain::try_from(domain) else {
        error!("[socket()] Address Family not supported: {domain}");
        return ErrorNo::EAFNOSUPPORT as isize;
    };
    let Ok(socket_type) = SocketType::try_from(s_type & SOCKET_TYPE_MASK) else {
        return ErrorNo::EINVAL as isize;
    };
    let mut socket = Socket::new(domain, socket_type);
    if s_type & SOCK_NONBLOCK != 0 {
        socket.set_nonblocking(true)
    }
    if s_type & SOCK_CLOEXEC != 0 {
        socket.close_exec = true;
    }
    let curr = current_process();
    let mut fd_table = curr.fd_manager.fd_table.lock();
    let Ok(fd) = curr.alloc_fd(&mut fd_table) else {
        return ErrorNo::EMFILE as isize;
    };

    fd_table[fd] = Some(Arc::new(socket));

    debug!("[socket()] create socket {fd}");

    fd as isize
}

pub fn syscall_bind(fd: usize, addr: *const u8, _addr_len: usize) -> isize {
    let curr = current_process();

    let file = match curr.fd_manager.fd_table.lock().get(fd) {
        Some(Some(file)) => file.clone(),
        _ => return ErrorNo::EBADF as isize,
    };

    let addr = unsafe { socket_address_from(addr) };

    let Some(socket) = file.as_any().downcast_ref::<Socket>() else {
        return ErrorNo::ENOTSOCK as isize;
    };

    info!("[bind()] binding socket {} to {:?}", fd, addr);

    socket.bind(addr).map_or(-1, |_| 0)
}

// TODO: support change `backlog` for tcp socket
pub fn syscall_listen(fd: usize, _backlog: usize) -> isize {
    let curr = current_process();

    let file = match curr.fd_manager.fd_table.lock().get(fd) {
        Some(Some(file)) => file.clone(),
        _ => return ErrorNo::EBADF as isize,
    };

    let Some(socket) = file.as_any().downcast_ref::<Socket>() else {
        return ErrorNo::ENOTSOCK as isize;
    };

    socket.listen().map_or(-1, |_| 0)
}

pub fn syscall_accept4(fd: usize, addr_buf: *mut u8, addr_len: *mut u32, flags: usize) -> isize {
    let curr = current_process();

    let file = match curr.fd_manager.fd_table.lock().get(fd) {
        Some(Some(file)) => file.clone(),
        _ => return ErrorNo::EBADF as isize,
    };

    let Some(socket) = file.as_any().downcast_ref::<Socket>() else {
        return ErrorNo::ENOTSOCK as isize;
    };

    debug!("[accept()] socket {fd} accept");

    // socket.accept() might block, we need to release all lock now.

    match socket.accept() {
        Ok((mut s, addr)) => {
            let _ = unsafe { socket_address_to(addr, addr_buf, addr_len) };

            let mut fd_table = curr.fd_manager.fd_table.lock();
            let Ok(new_fd) = curr.alloc_fd(&mut fd_table) else {
                return ErrorNo::EMFILE as isize; // Maybe ENFILE
            };

            debug!("[accept()] socket {fd} accept new socket {new_fd} {addr:?}");

            // handle flags
            if flags & SOCK_NONBLOCK != 0 {
                s.set_nonblocking(true);
            }
            if flags & SOCK_CLOEXEC != 0 {
                s.close_exec = true;
            }

            fd_table[new_fd] = Some(Arc::new(s));
            new_fd as isize
        }
        Err(AxError::Unsupported) => ErrorNo::EOPNOTSUPP as isize,
        Err(AxError::Interrupted) => ErrorNo::EINTR as isize,
        Err(AxError::WouldBlock) => ErrorNo::EAGAIN as isize,
        Err(_) => -1,
    }
}

pub fn syscall_connect(fd: usize, addr_buf: *const u8, _addr_len: usize) -> isize {
    let curr = current_process();

    let file = match curr.fd_manager.fd_table.lock().get(fd) {
        Some(Some(file)) => file.clone(),
        _ => return ErrorNo::EBADF as isize,
    };

    let Some(socket) = file.as_any().downcast_ref::<Socket>() else {
        return ErrorNo::ENOTSOCK as isize;
    };

    let addr = unsafe { socket_address_from(addr_buf) };

    debug!("[connect()] socket {fd} connecting to {addr:?}");

    match socket.connect(addr) {
        Ok(_) => 0,
        Err(AxError::WouldBlock) => ErrorNo::EINPROGRESS as isize,
        Err(AxError::Interrupted) => ErrorNo::EINTR as isize,
        Err(AxError::AlreadyExists) => ErrorNo::EISCONN as isize,
        Err(_) => -1,
    }
}

/// NOTE: linux man 中没有说明若socket未bound应返回什么错误
pub fn syscall_get_sock_name(fd: usize, addr: *mut u8, addr_len: *mut u32) -> isize {
    let curr = current_process();

    let file = match curr.fd_manager.fd_table.lock().get(fd) {
        Some(Some(file)) => file.clone(),
        _ => return ErrorNo::EBADF as isize,
    };

    let Some(socket) = file.as_any().downcast_ref::<Socket>() else {
        return ErrorNo::ENOTSOCK as isize;
    };

    debug!("[getsockname()] socket {fd}");

    let Ok(name) = socket.name() else {
        return -1;
    };

    info!("[getsockname()] socket {fd} name: {:?}", name);

    unsafe { socket_address_to(name, addr, addr_len) }.map_or(-1, |_| 0)
}

#[allow(unused)]
pub fn syscall_getpeername(fd: usize, addr_buf: *mut u8, addr_len: *mut u32) -> isize {
    let curr = current_process();

    let file = match curr.fd_manager.fd_table.lock().get(fd) {
        Some(Some(file)) => file.clone(),
        _ => return ErrorNo::EBADF as isize,
    };

    let len = match curr.manual_alloc_type_for_lazy(addr_len as *const u32) {
        Ok(_) => unsafe { *addr_len },
        Err(_) => return ErrorNo::EFAULT as isize,
    };
    // It seems it could be negative according to Linux man page.
    if (len as i32) < 0 {
        return ErrorNo::EINVAL as isize;
    }

    if curr
        .manual_alloc_range_for_lazy(
            (addr_buf as usize).into(),
            unsafe { addr_buf.add(len as usize) as usize }.into(),
        )
        .is_err()
    {
        return ErrorNo::EFAULT as isize;
    }

    let Some(socket) = file.as_any().downcast_ref::<Socket>() else {
        return ErrorNo::ENOTSOCK as isize;
    };

    match socket.peer_name() {
        Ok(name) => unsafe { socket_address_to(name, addr_buf, addr_len) }.map_or(-1, |_| 0),
        Err(AxError::NotConnected) => ErrorNo::ENOTCONN as isize,
        Err(_) => unreachable!(),
    }
}

// TODO: flags
/// Calling sendto() will bind the socket if it's not bound.
pub fn syscall_sendto(
    fd: usize,
    buf: *const u8,
    len: usize,
    _flags: usize,
    addr: *const u8,
    addr_len: usize,
) -> isize {
    let curr = current_process();

    let file = match curr.fd_manager.fd_table.lock().get(fd) {
        Some(Some(file)) => file.clone(),
        _ => return ErrorNo::EBADF as isize,
    };

    let Some(socket) = file.as_any().downcast_ref::<Socket>() else {
        return ErrorNo::ENOTSOCK as isize;
    };

    if buf.is_null() {
        return ErrorNo::EFAULT as isize;
    }
    let Ok(buf) = curr
        .manual_alloc_range_for_lazy(
            (buf as usize).into(),
            unsafe { buf.add(len) as usize }.into(),
        )
        .map(|_| unsafe { from_raw_parts(buf, len) })
    else {
        error!("[sendto()] buf address {buf:?} invalid");
        return ErrorNo::EFAULT as isize;
    };

    let addr = if !addr.is_null() && addr_len != 0 {
        match curr.manual_alloc_range_for_lazy(
            (addr as usize).into(),
            unsafe { addr.add(addr_len) as usize }.into(),
        ) {
            Ok(_) => Some(unsafe { socket_address_from(addr) }),
            Err(_) => {
                error!("[sendto()] addr address {addr:?} invalid");
                return ErrorNo::EFAULT as isize;
            }
        }
    } else {
        None
    };
    let inner = socket.inner.lock();
    let send_result = match &*inner {
        SocketInner::Udp(s) => {
            // udp socket not bound
            if s.local_addr().is_err() {
                s.bind(into_core_sockaddr(SocketAddr::new(
                    IpAddr::v4(0, 0, 0, 0),
                    0,
                )))
                .unwrap();
            }
            match addr {
                Some(addr) => s.send_to(buf, into_core_sockaddr(addr)),
                None => {
                    // not connected and no target is given
                    if s.peer_addr().is_err() {
                        return ErrorNo::ENOTCONN as isize;
                    }
                    s.send(buf)
                }
            }
        }
        SocketInner::Tcp(s) => {
            if addr.is_some() {
                return ErrorNo::EISCONN as isize;
            }

            if !s.is_connected() {
                return ErrorNo::ENOTCONN as isize;
            }

            s.send(buf)
        }
    };

    match send_result {
        Ok(len) => {
            info!("[sendto()] socket {fd} sent {len} bytes to addr {:?}", addr);
            len as isize
        }
        Err(AxError::Interrupted) => ErrorNo::EINTR as isize,
        Err(_) => -1,
    }
}

pub fn syscall_recvfrom(
    fd: usize,
    buf: *mut u8,
    len: usize,
    _flags: usize,
    addr_buf: *mut u8,
    addr_len: *mut u32,
) -> isize {
    let curr = current_process();

    let file = match curr.fd_manager.fd_table.lock().get(fd) {
        Some(Some(file)) => file.clone(),
        _ => return ErrorNo::EBADF as isize,
    };
    let Some(socket) = file.as_any().downcast_ref::<Socket>() else {
        return ErrorNo::ENOTSOCK as isize;
    };

    if !addr_len.is_null()
        && curr
            .manual_alloc_for_lazy((addr_len as usize).into())
            .is_err()
    {
        error!("[recvfrom()] addr_len address {addr_len:?} invalid");
        return ErrorNo::EFAULT as isize;
    }
    if !addr_buf.is_null()
        && !addr_len.is_null()
        && curr
            .manual_alloc_range_for_lazy(
                (addr_buf as usize).into(),
                unsafe { addr_buf.add(*addr_len as usize) as usize }.into(),
            )
            .is_err()
    {
        error!(
            "[recvfrom()] addr_buf address {addr_buf:?}, len: {} invalid",
            unsafe { *addr_len }
        );
        return ErrorNo::EFAULT as isize;
    }
    let buf = unsafe { from_raw_parts_mut(buf, len) };
    info!("recv addr: {:?}", socket.name().unwrap());
    match socket.recv_from(buf) {
        Ok((len, addr)) => {
            info!("socket {fd} recv {len} bytes from {addr:?}");
            if !addr_buf.is_null() && !addr_len.is_null() {
                unsafe { socket_address_to(addr, addr_buf, addr_len) }.map_or(-1, |_| len as isize)
            } else {
                len as isize
            }
        }
        Err(AxError::ConnectionRefused) => 0,
        Err(AxError::Interrupted) => ErrorNo::EINTR as isize,
        Err(AxError::Timeout) | Err(AxError::WouldBlock) => ErrorNo::EAGAIN as isize,
        Err(_) => -1,
    }
}

/// NOTE: only support socket level options (SOL_SOCKET)
pub fn syscall_set_sock_opt(
    fd: usize,
    level: usize,
    opt_name: usize,
    opt_value: *const u8,
    opt_len: u32,
) -> isize {
    let Ok(level) = SocketOptionLevel::try_from(level) else {
        error!("[setsockopt()] level {level} not supported");
        unimplemented!();
    };

    let curr = current_process();

    let file = match curr.fd_manager.fd_table.lock().get(fd) {
        Some(Some(file)) => file.clone(),
        _ => return ErrorNo::EBADF as isize,
    };

    let Some(socket) = file.as_any().downcast_ref::<Socket>() else {
        return ErrorNo::ENOTSOCK as isize;
    };

    let opt = unsafe { from_raw_parts(opt_value, opt_len as usize) };

    match level {
        SocketOptionLevel::IP => 0,
        SocketOptionLevel::SOCKET => {
            let Ok(option) = SocketOption::try_from(opt_name) else {
                warn!("[setsockopt()] option {opt_name} not supported in socket level");
                return 0;
            };

            option.set(socket, opt);
            0
        }
        SocketOptionLevel::TCP => {
            let Ok(option) = TcpSocketOption::try_from(opt_name) else {
                warn!("[setsockopt()] option {opt_name} not supported in tcp level");
                return 0;
            };

            option.set(socket, opt);
            0
        }
    }
}

pub fn syscall_get_sock_opt(
    fd: usize,
    level: usize,
    opt_name: usize,
    opt_value: *mut u8,
    opt_len: *mut u32,
) -> isize {
    let Ok(level) = SocketOptionLevel::try_from(level) else {
        error!("[setsockopt()] level {level} not supported");
        unimplemented!();
    };

    if opt_value.is_null() || opt_len.is_null() {
        return ErrorNo::EFAULT as isize;
    }

    let curr = current_process();

    let file = match curr.fd_manager.fd_table.lock().get(fd) {
        Some(Some(file)) => file.clone(),
        _ => return ErrorNo::EBADF as isize,
    };

    let Some(socket) = file.as_any().downcast_ref::<Socket>() else {
        return ErrorNo::ENOTSOCK as isize;
    };

    if curr
        .manual_alloc_type_for_lazy(opt_len as *const u32)
        .is_err()
    {
        error!("[getsockopt()] opt_len address {opt_len:?} invalid");
        return ErrorNo::EFAULT as isize;
    }
    if curr
        .manual_alloc_range_for_lazy(
            (opt_value as usize).into(),
            (unsafe { opt_value.add(*opt_len as usize) } as usize).into(),
        )
        .is_err()
    {
        error!(
            "[getsockopt()] opt_value {opt_value:?}, len {} invalid",
            unsafe { *opt_len }
        );
        return ErrorNo::EFAULT as isize;
    }

    match level {
        SocketOptionLevel::IP => {}
        SocketOptionLevel::SOCKET => {
            let Ok(option) = SocketOption::try_from(opt_name) else {
                panic!("[setsockopt()] option {opt_name} not supported in socket level");
            };

            option.get(socket, opt_value, opt_len);
        }
        SocketOptionLevel::TCP => {
            let Ok(option) = TcpSocketOption::try_from(opt_name) else {
                panic!("[setsockopt()] option {opt_name} not supported in tcp level");
            };

            if option == TcpSocketOption::TCP_INFO {
                return ErrorNo::ENOPROTOOPT as isize;
            }

            option.get(socket, opt_value, opt_len);
        }
    }

    0
}

#[derive(TryFromPrimitive)]
#[repr(usize)]
enum SocketShutdown {
    Read = 0,
    Write = 1,
    ReadWrite = 2,
}

pub fn syscall_shutdown(fd: usize, how: usize) -> isize {
    let curr = current_process();

    let file = match curr.fd_manager.fd_table.lock().get(fd) {
        Some(Some(file)) => file.clone(),
        _ => return ErrorNo::EBADF as isize,
    };

    let Some(socket) = file.as_any().downcast_ref::<Socket>() else {
        return ErrorNo::ENOTSOCK as isize;
    };

    let Ok(how) = SocketShutdown::try_from(how) else {
        return ErrorNo::EINVAL as isize;
    };

    match how {
        SocketShutdown::Read => {
            error!("[shutdown()] SHUT_RD is noop")
        }
        SocketShutdown::Write => socket.shutdown(),
        SocketShutdown::ReadWrite => {
            socket.abort();
        }
    }

    0
}
