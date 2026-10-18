//! Native host sockets. Readiness only queues module events; handlers remain serialized.
use crate::pool::HELD_OWNED;
use crate::runtime::{deliver, Inst, QItem, Shared};
use std::{
    collections::HashMap,
    ffi::{c_int, c_void},
    io,
    net::{IpAddr, SocketAddr},
    rc::Rc,
    task::{Context, Poll},
};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use xip_abi::*;

const SOCKETS: usize = 16;
#[derive(Clone, Debug, Default)]
pub struct Ports {
    pub datagram: Option<SocketAddr>,
    pub stream: Option<SocketAddr>,
}
struct Listener {
    owner: usize,
    socket: TcpListener,
}
struct Peer {
    address: SocketAddr,
    seen: u64,
}
struct Datagram {
    owner: usize,
    socket: UdpSocket,
    link: u32,
    peers: HashMap<u64, Peer>,
    next: u64,
}
struct WritePacket {
    slot: i32,
    start: usize,
    len: usize,
    at: usize,
}
struct Socket {
    owner: usize,
    io: TcpStream,
    opening: bool,
    read_pending: bool,
    writable: bool,
    eof: bool,
    pending: u32,
    packet: Option<WritePacket>,
}
#[derive(Default)]
pub(crate) struct Network {
    listeners: Vec<Listener>,
    udp: Vec<Datagram>,
    sockets: HashMap<u32, Socket>,
    next: u32,
}
fn event(sh: &Shared, owner: usize, handle: u32, value: u32) -> bool {
    let insts = sh.insts.borrow();
    let me = &insts[owner];
    let component = me.st.borrow().component.unwrap_or(0);
    let mut st = me.st.borrow_mut();
    if st.queue.len() < crate::runtime::QUEUE {
        st.queue.push_back(QItem {
            request: 4,
            component,
            method: 0,
            value: value as i32,
            id: handle as u64,
            ..Default::default()
        });
        true
    } else {
        false
    }
}
fn err(e: io::Error) -> i32 {
    if e.kind() == io::ErrorKind::WouldBlock {
        ERR_AGAIN
    } else {
        ERR_IO
    }
}
impl Network {
    pub(crate) fn release(&mut self, sh: &Shared, owner: usize) {
        self.listeners.retain(|l| l.owner != owner);
        self.udp.retain(|l| {
            if l.owner == owner {
                crate::link::remove_datagram_link(sh, l.link);
                false
            } else {
                true
            }
        });
        self.sockets.retain(|_, s| {
            if s.owner != owner {
                return true;
            }
            if let Some(p) = &s.packet {
                sh.pool.release(p.slot);
            }
            false
        });
    }
    pub(crate) fn poll(&mut self, sh: &Shared, cx: &mut Context<'_>) -> Poll<()> {
        let mut active = false;
        for l in &self.listeners {
            if self.sockets.len() == SOCKETS {
                continue;
            }
            if let Poll::Ready(Ok((io, _))) = l.socket.poll_accept(cx) {
                self.next = self.next.checked_add(1).expect("socket handles exhausted");
                let id = self.next;
                let _ = io.set_nodelay(true);
                self.sockets.insert(
                    id,
                    Socket {
                        owner: l.owner,
                        io,
                        opening: true,
                        read_pending: false,
                        writable: false,
                        eof: false,
                        pending: 0,
                        packet: None,
                    },
                );
                active = true;
            }
        }
        for s in self.sockets.values_mut() {
            if s.opening {
                if s.io.poll_write_ready(cx).is_pending() {
                    continue;
                }
                s.opening = false;
                s.pending |= 1 << SOCKET_EVENT_OPEN;
                active = true;
            }
            if !s.read_pending && !s.eof {
                if let Poll::Ready(_) = s.io.poll_read_ready(cx) {
                    s.read_pending = true;
                    s.pending |= 1 << SOCKET_EVENT_READABLE;
                    active = true;
                }
            }
            if s.packet.is_some() || s.writable {
                if let Poll::Ready(_) = s.io.poll_write_ready(cx) {
                    if let Some(p) = &mut s.packet {
                        let data = unsafe {
                            std::slice::from_raw_parts(
                                sh.pool.slot(p.slot).add(p.start + p.at),
                                p.len - p.at,
                            )
                        };
                        match s.io.try_write(data) {
                            Ok(n) => {
                                p.at += n;
                                if p.at == p.len {
                                    let p = s.packet.take().unwrap();
                                    sh.pool.release(p.slot);
                                    s.pending |= 1 << SOCKET_EVENT_WRITE_DONE;
                                }
                                active = true;
                            }
                            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                            Err(_) => {
                                let p = s.packet.take().unwrap();
                                sh.pool.release(p.slot);
                                s.eof = true;
                                s.pending |= 1 << SOCKET_EVENT_CLOSED;
                                active = true;
                            }
                        }
                    } else if s.writable {
                        s.writable = false;
                        s.pending |= 1 << SOCKET_EVENT_WRITABLE;
                        active = true;
                    }
                }
            }
        }
        for (&id, s) in &mut self.sockets {
            for kind in [
                SOCKET_EVENT_OPEN,
                SOCKET_EVENT_READABLE,
                SOCKET_EVENT_WRITABLE,
                SOCKET_EVENT_EOF,
                SOCKET_EVENT_CLOSED,
                SOCKET_EVENT_WRITE_DONE,
            ] {
                if s.pending & (1 << kind) != 0 {
                    active = true;
                    if event(sh, s.owner, id, kind) {
                        s.pending &= !(1 << kind);
                    } else {
                        break;
                    }
                }
            }
        }
        for d in &mut self.udp {
            let mut bytes = [0u8; PKT_MAX as usize + 1];
            let mut buf = tokio::io::ReadBuf::new(&mut bytes);
            if let Poll::Ready(Ok(peer)) = d.socket.poll_recv_from(cx, &mut buf) {
                active = true;
                if buf.filled().len() > PKT_MAX as usize {
                    continue;
                }
                let now = sh.st.borrow().now;
                d.peers
                    .retain(|_, p| now.saturating_sub(p.seen) < sh.cfg.udp_peer_idle_ms);
                let token = if let Some((&token, entry)) =
                    d.peers.iter_mut().find(|(_, p)| p.address == peer)
                {
                    entry.seen = now;
                    token
                } else {
                    if sh.cfg.udp_peer_capacity == 0 {
                        continue;
                    }
                    if d.peers.len() == sh.cfg.udp_peer_capacity {
                        if let Some((&old, _)) =
                            d.peers.iter().min_by_key(|(token, p)| (p.seen, **token))
                        {
                            d.peers.remove(&old);
                        }
                    }
                    let Some(next) = d.next.checked_add(1) else {
                        continue;
                    };
                    d.next = next;
                    d.peers.insert(
                        next,
                        Peer {
                            address: peer,
                            seen: now,
                        },
                    );
                    next
                };
                let Some(slot) = sh.pool.acquire(0) else {
                    continue;
                };
                let data = buf.filled();
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        data.as_ptr(),
                        sh.pool.slot(slot).add(PKT_HEADROOM as usize),
                        data.len(),
                    );
                }
                let insts = sh.insts.borrow();
                let me = &insts[d.owner];
                let component = me.st.borrow().component.unwrap_or(0);
                if deliver(
                    sh,
                    me,
                    slot,
                    PKT_HEADROOM as u16,
                    data.len() as u16,
                    component,
                    0,
                    d.link,
                    token,
                    0,
                    CALL_LINK,
                    0,
                ) != 0
                {
                    sh.pool.release(slot);
                }
            }
        }
        if active {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
    fn owned(&mut self, owner: usize, handle: u32) -> Option<&mut Socket> {
        self.sockets.get_mut(&handle).filter(|s| s.owner == owner)
    }
}

impl crate::Instance {
    /// Bind this instance's manifest ports to native sockets. `base = 0` chooses
    /// ephemeral ports; otherwise each local port is `base + manifest port`.
    pub async fn listen_network(&self, ip: IpAddr, base: u16) -> io::Result<Ports> {
        if self.ended()
            || self
                .rt
                .sh
                .network
                .borrow()
                .listeners
                .iter()
                .any(|l| l.owner == self.me.id)
            || self
                .rt
                .sh
                .network
                .borrow()
                .udp
                .iter()
                .any(|l| l.owner == self.me.id)
        {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "instance is stopped or already bound",
            ));
        }
        let manifest = self.manifest();
        let port = xip_abi::cbor::get_int(&manifest, 9)
            .ok()
            .flatten()
            .unwrap_or(0);
        let stream = xip_abi::cbor::get_int(&manifest, 10)
            .ok()
            .flatten()
            .unwrap_or(0);
        let address = |n: i64| -> io::Result<SocketAddr> {
            let n = u16::try_from(n).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "invalid manifest port")
            })?;
            let port = if base == 0 {
                0
            } else {
                base.checked_add(n)
                    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "port overflow"))?
            };
            Ok(SocketAddr::new(ip, port))
        };
        let listener = if stream > 0 {
            Some(TcpListener::bind(address(stream)?).await?)
        } else {
            None
        };
        let udp = if port > 0 {
            Some(UdpSocket::bind(address(port)?).await?)
        } else {
            None
        };
        // Prepare everything before publishing either endpoint. A failed bind or
        // readiness registration must leave no half-installed listener behind.
        if let Some(socket) = &udp {
            socket.writable().await?;
        }
        let ports = Ports {
            stream: listener.as_ref().map(|s| s.local_addr()).transpose()?,
            datagram: udp.as_ref().map(|s| s.local_addr()).transpose()?,
        };
        if self.ended()
            || self
                .rt
                .sh
                .network
                .borrow()
                .listeners
                .iter()
                .any(|l| l.owner == self.me.id)
            || self
                .rt
                .sh
                .network
                .borrow()
                .udp
                .iter()
                .any(|l| l.owner == self.me.id)
        {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "instance is stopped or already bound",
            ));
        }
        let link = if udp.is_some() {
            Some(
                crate::link::add_datagram_link(&self.rt.sh)
                    .map_err(|_| io::Error::other("no room for UDP link"))?,
            )
        } else {
            None
        };
        let mut network = self.rt.sh.network.borrow_mut();
        if let Some(socket) = listener {
            network.listeners.push(Listener {
                owner: self.me.id,
                socket,
            });
        }
        if let Some(socket) = udp {
            network.udp.push(Datagram {
                owner: self.me.id,
                socket,
                link: link.unwrap(),
                peers: HashMap::new(),
                next: 0,
            });
        }
        self.rt.sh.notify.notify_one();
        Ok(ports)
    }
}
pub(crate) fn udp_tx(sh: &Shared, dst: u32, data: &[u8], token: u64) -> Option<i32> {
    let mut net = sh.network.borrow_mut();
    let d = net.udp.iter_mut().find(|d| d.link == dst)?;
    let now = sh.st.borrow().now;
    d.peers
        .retain(|_, p| now.saturating_sub(p.seen) < sh.cfg.udp_peer_idle_ms);
    let Some(peer) = d.peers.get_mut(&token) else {
        return Some(ERR_NOTFOUND);
    };
    peer.seen = now;
    Some(match d.socket.try_send_to(data, peer.address) {
        Ok(n) if n == data.len() => 0,
        Ok(_) => ERR_IO,
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => ERR_BUSY,
        Err(_) => ERR_IO,
    })
}
unsafe fn context<'a>(user: *mut c_void) -> (&'a Shared, Rc<Inst>) {
    crate::hostcalls::ctx(user)
}
pub(crate) unsafe extern "C" fn read(
    user: *mut c_void,
    handle: u32,
    out: *mut u8,
    len: usize,
) -> c_int {
    if out.is_null() && len != 0 {
        return ERR_ARG;
    }
    let (sh, me) = context(user);
    let mut net = sh.network.borrow_mut();
    let Some(s) = net.owned(me.id, handle) else {
        return ERR_ARG;
    };
    if len == 0 {
        return 0;
    }
    s.read_pending = false;
    match s.io.try_read(std::slice::from_raw_parts_mut(out, len)) {
        Ok(0) => {
            if !s.eof {
                s.eof = true;
                s.pending |= 1 << SOCKET_EVENT_EOF;
            }
            0
        }
        Ok(n) => n as i32,
        Err(e) => err(e),
    }
}
pub(crate) unsafe extern "C" fn write(
    user: *mut c_void,
    handle: u32,
    data: *const u8,
    len: usize,
) -> c_int {
    if data.is_null() && len != 0 {
        return ERR_ARG;
    }
    let (sh, me) = context(user);
    let mut net = sh.network.borrow_mut();
    let Some(s) = net.owned(me.id, handle) else {
        return ERR_ARG;
    };
    if len == 0 {
        return 0;
    }
    if s.packet.is_some() {
        s.writable = true;
        return ERR_AGAIN;
    }
    let rc = match s.io.try_write(std::slice::from_raw_parts(data, len)) {
        Ok(n) => n as i32,
        Err(e) => err(e),
    };
    if rc == ERR_AGAIN || (rc > 0 && rc < len as i32) {
        s.writable = true;
    }
    sh.notify.notify_one();
    rc
}
pub(crate) unsafe extern "C" fn close(user: *mut c_void, handle: u32) -> c_int {
    let (sh, me) = context(user);
    let mut net = sh.network.borrow_mut();
    if net.owned(me.id, handle).is_none() {
        return ERR_ARG;
    }
    let s = net.sockets.remove(&handle).unwrap();
    if let Some(p) = s.packet {
        sh.pool.release(p.slot);
    }
    sh.notify.notify_one();
    0
}
pub(crate) unsafe extern "C" fn peer(
    user: *mut c_void,
    handle: u32,
    addr: *mut u8,
    port: *mut u16,
    scope: *mut u32,
) -> c_int {
    if addr.is_null() || port.is_null() || scope.is_null() {
        return ERR_ARG;
    }
    let (sh, me) = context(user);
    let mut net = sh.network.borrow_mut();
    let Some(s) = net.owned(me.id, handle) else {
        return ERR_ARG;
    };
    let Ok(peer) = s.io.peer_addr() else {
        return ERR_IO;
    };
    let (ip, scope_id) = match peer {
        SocketAddr::V4(a) => (a.ip().to_ipv6_mapped().octets(), 0),
        SocketAddr::V6(a) => (a.ip().octets(), a.scope_id()),
    };
    std::ptr::copy_nonoverlapping(ip.as_ptr(), addr, 16);
    *port = peer.port();
    *scope = scope_id;
    0
}
pub(crate) unsafe extern "C" fn write_pkt(
    user: *mut c_void,
    handle: u32,
    pkt: u32,
    start: u32,
    len: u32,
) -> c_int {
    let (sh, me) = context(user);
    let Some(e) = me
        .st
        .borrow_mut()
        .held
        .get(pkt)
        .filter(|e| e.state as u32 == HELD_OWNED)
    else {
        return ERR_ARG;
    };
    if sh.pool.refs(e.slot as i32) != 1 {
        return ERR_BUSY;
    }
    if len == 0
        || start as usize > sh.pool.slot_size() as usize
        || len as usize > sh.pool.slot_size() as usize - start as usize
    {
        return ERR_ARG;
    }
    let mut net = sh.network.borrow_mut();
    let Some(s) = net.owned(me.id, handle) else {
        return ERR_ARG;
    };
    if s.packet.is_some() {
        return ERR_BUSY;
    }
    let e = me.st.borrow_mut().held.take(pkt).unwrap();
    s.packet = Some(WritePacket {
        slot: e.slot as i32,
        start: start as usize,
        len: len as usize,
        at: 0,
    });
    sh.notify.notify_one();
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Runtime;
    #[tokio::test(flavor = "current_thread")]
    async fn lifecycle_event_retries_after_dispatch_queue_is_full() {
        unsafe extern "C" fn noop(_: *const Host, _: *mut u8, _: *const Call) -> i32 {
            0
        }
        let rt = Runtime::default();
        let m = rt.add_handler("events", noop, 1);
        m.set_component(2);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (client, server) = tokio::join!(
            TcpStream::connect(listener.local_addr().unwrap()),
            listener.accept()
        );
        let _client = client.unwrap();
        let (io, _) = server.unwrap();
        rt.sh.network.borrow_mut().sockets.insert(
            1,
            Socket {
                owner: m.me.id,
                io,
                opening: false,
                read_pending: true,
                writable: false,
                eof: true,
                pending: 1 << SOCKET_EVENT_OPEN,
                packet: None,
            },
        );
        for _ in 0..crate::runtime::QUEUE {
            assert_eq!(m.post(2, 42, b""), 0);
        }
        core::future::poll_fn(|cx| {
            assert!(rt.sh.network.borrow_mut().poll(&rt.sh, cx).is_ready());
            Poll::Ready(())
        })
        .await;
        assert_ne!(rt.sh.network.borrow().sockets[&1].pending, 0);
        m.me.st.borrow_mut().queue.clear();
        core::future::poll_fn(|cx| {
            assert!(rt.sh.network.borrow_mut().poll(&rt.sh, cx).is_ready());
            Poll::Ready(())
        })
        .await;
        assert_eq!(rt.sh.network.borrow().sockets[&1].pending, 0);
        let st = m.me.st.borrow();
        assert_eq!(st.queue.front().unwrap().value, SOCKET_EVENT_OPEN as i32);
        assert_eq!(st.queue.front().unwrap().id, 1);
    }
}

#[cfg(test)]
mod notification_and_retention_tests {
    use super::*;
    #[tokio::test(flavor = "current_thread")]
    async fn native_serial_transmit_notifies_without_input() {
        unsafe extern "C" fn handle(h: *const Host, _: *mut u8, c: *const Call) -> i32 {
            if (*c).component == 0 && (*c).method == HOST_START as i32 {
                assert_eq!(
                    Hw::new(h)
                        .unwrap()
                        .message(ADDR_LINK, 1, 0, 0, b"spontaneous"),
                    0
                );
            }
            RET_CONTINUE as i32
        }
        let rt = crate::Runtime::default();
        rt.add_native_link("serial", 256, crate::NativeTx::Done);
        let m = rt.add_handler("output", handle, 1);
        m.start(b"");
        rt.sh.notify.notified().await; // consume admission of HOST_START
        rt.run(0); // serial TX must create its own notification
        core::future::poll_fn(|cx| {
            let mut wake = core::pin::pin!(rt.sh.notify.notified());
            assert!(core::future::Future::poll(wake.as_mut(), cx).is_ready());
            Poll::Ready(())
        })
        .await;
        assert_eq!(
            rt.take_link_tx("serial"),
            vec![(0, b"spontaneous".to_vec())]
        );
    }
    #[tokio::test(flavor = "current_thread")]
    async fn udp_peer_expiry_rejects_old_token() {
        let rt = crate::Runtime::default();
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        socket.writable().await.unwrap();
        let mut peers = HashMap::new();
        peers.insert(
            1,
            Peer {
                address: "127.0.0.1:9".parse().unwrap(),
                seen: 0,
            },
        );
        rt.sh.network.borrow_mut().udp.push(Datagram {
            owner: 0,
            socket,
            link: ADDR_LINK,
            peers,
            next: 1,
        });
        rt.sh.st.borrow_mut().now = rt.sh.cfg.udp_peer_idle_ms;
        assert_eq!(udp_tx(&rt.sh, ADDR_LINK, b"stale", 1), Some(ERR_NOTFOUND));
        assert!(rt.sh.network.borrow().udp[0].peers.is_empty());
    }
}
