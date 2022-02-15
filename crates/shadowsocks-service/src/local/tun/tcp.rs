use std::{
    collections::{BTreeMap, HashMap},
    io::{self, ErrorKind},
    mem,
    net::{IpAddr, SocketAddr},
    pin::Pin,
    sync::Arc,
    task::{Context, Poll, Waker},
    time::Duration as StdDuration,
};

use log::{error, trace};
use parking_lot::Mutex as ParkingMutex;
use shadowsocks::{net::TcpSocketOpts, relay::socks5::Address};
use smoltcp::{
    iface::{Interface, InterfaceBuilder, Routes, SocketHandle},
    phy::{DeviceCapabilities, Medium},
    socket::{TcpSocket, TcpSocketBuffer, TcpState},
    storage::RingBuffer,
    time::{Duration, Instant},
    wire::{IpAddress, IpCidr, Ipv4Address, Ipv6Address, TcpPacket},
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    sync::{mpsc, Notify},
    task::JoinHandle,
    time,
};

use crate::local::{
    context::ServiceContext,
    loadbalancing::PingBalancer,
    net::AutoProxyClientStream,
    utils::{establish_tcp_tunnel, to_ipv4_mapped},
};

use super::virt_device::VirtTunDevice;

// NOTE: Default value is taken from Linux
// recv: /proc/sys/net/ipv4/tcp_rmem 87380 bytes
// send: /proc/sys/net/ipv4/tcp_wmem 16384 bytes
const DEFAULT_TCP_SEND_BUFFER_SIZE: u32 = 16384;
const DEFAULT_TCP_RECV_BUFFER_SIZE: u32 = 87380;

struct TcpSocketControl {
    send_buffer: RingBuffer<'static, u8>,
    send_waker: Option<Waker>,
    recv_buffer: RingBuffer<'static, u8>,
    recv_waker: Option<Waker>,
    is_closed: bool,
}

struct TcpSocketManager {
    iface: Interface<'static, VirtTunDevice>,
    manager_notify: Arc<Notify>,
    sockets: HashMap<SocketHandle, Arc<ParkingMutex<TcpSocketControl>>>,
}

type SharedTcpSocketManager = Arc<ParkingMutex<TcpSocketManager>>;

struct TcpConnection {
    control: Arc<ParkingMutex<TcpSocketControl>>,
    manager_notify: Arc<Notify>,
}

impl Drop for TcpConnection {
    fn drop(&mut self) {
        let mut control = self.control.lock();
        control.is_closed = true;
    }
}

impl TcpConnection {
    fn new(socket: TcpSocket<'static>, manager: SharedTcpSocketManager, tcp_opts: &TcpSocketOpts) -> TcpConnection {
        let send_buffer_size = tcp_opts.send_buffer_size.unwrap_or(DEFAULT_TCP_SEND_BUFFER_SIZE);
        let recv_buffer_size = tcp_opts.recv_buffer_size.unwrap_or(DEFAULT_TCP_RECV_BUFFER_SIZE);

        let (control, manager_notify) = {
            let mut manager = manager.lock();
            let socket_handle = manager.iface.add_socket(socket);

            let control = Arc::new(ParkingMutex::new(TcpSocketControl {
                send_buffer: RingBuffer::new(vec![0u8; send_buffer_size as usize]),
                send_waker: None,
                recv_buffer: RingBuffer::new(vec![0u8; recv_buffer_size as usize]),
                recv_waker: None,
                is_closed: false,
            }));

            manager.sockets.insert(socket_handle.clone(), control.clone());
            (control, manager.manager_notify.clone())
        };

        TcpConnection {
            control,
            manager_notify,
        }
    }
}

impl AsyncRead for TcpConnection {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let mut control = self.control.lock();

        // If socket is already closed, just return EOF directly.
        if control.is_closed {
            return Ok(()).into();
        }

        // Read from buffer

        if control.recv_buffer.is_empty() {
            // Nothing could be read. Wait for notify.
            if let Some(old_waker) = control.recv_waker.replace(cx.waker().clone()) {
                if !old_waker.will_wake(cx.waker()) {
                    old_waker.wake();
                }
            }

            return Poll::Pending;
        }

        let recv_buf = unsafe { mem::transmute::<_, &mut [u8]>(buf.unfilled_mut()) };
        let n = control.recv_buffer.dequeue_slice(recv_buf);
        buf.advance(n);

        self.manager_notify.notify_one();
        Ok(()).into()
    }
}

impl AsyncWrite for TcpConnection {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        let mut control = self.control.lock();
        if control.is_closed {
            return Err(io::ErrorKind::BrokenPipe.into()).into();
        }

        // Write to buffer

        if control.send_buffer.is_full() {
            if let Some(old_waker) = control.send_waker.replace(cx.waker().clone()) {
                if !old_waker.will_wake(cx.waker()) {
                    old_waker.wake();
                }
            }

            return Poll::Pending;
        }

        let n = control.send_buffer.enqueue_slice(buf);

        self.manager_notify.notify_one();
        Ok(n).into()
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Ok(()).into()
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut control = self.control.lock();

        if control.is_closed {
            return Ok(()).into();
        }

        control.is_closed = true;
        if let Some(old_waker) = control.send_waker.replace(cx.waker().clone()) {
            if !old_waker.will_wake(cx.waker()) {
                old_waker.wake();
            }
        }

        Poll::Pending
    }
}

pub struct TcpTun {
    context: Arc<ServiceContext>,
    manager: SharedTcpSocketManager,
    manager_handle: JoinHandle<()>,
    manager_notify: Arc<Notify>,
    balancer: PingBalancer,
    iface_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    iface_tx: mpsc::Sender<Vec<u8>>,
}

impl Drop for TcpTun {
    fn drop(&mut self) {
        self.manager_handle.abort();
    }
}

impl TcpTun {
    pub fn new(context: Arc<ServiceContext>, balancer: PingBalancer, mtu: u32) -> TcpTun {
        let mut capabilities = DeviceCapabilities::default();
        capabilities.medium = Medium::Ip;
        capabilities.max_transmission_unit = mtu as usize;

        let (virt, iface_rx, iface_tx) = VirtTunDevice::new(capabilities);

        let iface_builder = InterfaceBuilder::new(virt, vec![]);
        let iface_ipaddrs = [
            IpCidr::new(IpAddress::v4(0, 0, 0, 1), 0),
            IpCidr::new(IpAddress::v6(0, 0, 0, 0, 0, 0, 0, 1), 0),
        ];
        let mut iface_routes = Routes::new(BTreeMap::new());
        iface_routes
            .add_default_ipv4_route(Ipv4Address::new(0, 0, 0, 1))
            .expect("IPv4 route");
        iface_routes
            .add_default_ipv6_route(Ipv6Address::new(0, 0, 0, 0, 0, 0, 0, 1))
            .expect("IPv6 route");
        let iface = iface_builder
            .any_ip(true)
            .ip_addrs(iface_ipaddrs)
            .routes(iface_routes)
            .finalize();

        let manager_notify = Arc::new(Notify::new());
        let manager = Arc::new(ParkingMutex::new(TcpSocketManager {
            iface,
            manager_notify: manager_notify.clone(),
            sockets: HashMap::new(),
        }));

        let manager_handle = {
            let manager = manager.clone();
            let manager_notify = manager_notify.clone();
            tokio::spawn(async move {
                loop {
                    let next_duration = {
                        let TcpSocketManager {
                            ref mut iface,
                            ref mut sockets,
                            ..
                        } = *(manager.lock());

                        let before_poll = Instant::now();
                        let updated_sockets = match iface.poll(before_poll) {
                            Ok(u) => u,
                            Err(err) => {
                                error!("VirtDevice::poll error: {}", err);
                                false
                            }
                        };

                        let after_poll = Instant::now();

                        if updated_sockets {
                            trace!("VirtDevice::poll costed {}", after_poll - before_poll);
                        }

                        // Check all the sockets' status
                        let mut sockets_to_remove = Vec::new();

                        for (socket_handle, control) in sockets.iter() {
                            let socket_handle = socket_handle.clone();
                            let socket = iface.get_socket::<TcpSocket>(socket_handle);
                            let mut control = control.lock();

                            #[inline]
                            fn close_socket_control(control: &mut TcpSocketControl) {
                                control.is_closed = true;
                                if let Some(waker) = control.send_waker.take() {
                                    waker.wake();
                                }
                                if let Some(waker) = control.recv_waker.take() {
                                    waker.wake();
                                }
                            }

                            if !socket.is_open() || socket.state() == TcpState::Closed {
                                sockets_to_remove.push(socket_handle);
                                close_socket_control(&mut *control);
                                continue;
                            }

                            if control.is_closed {
                                // Close the socket.
                                socket.close();
                                // sockets_to_remove.push(socket_handle);
                                // close_socket_control(&mut *control);
                                continue;
                            }

                            // Check if readable
                            let mut has_received = false;
                            while socket.can_recv() && !control.recv_buffer.is_full() {
                                let result = socket.recv(|buffer| {
                                    let n = control.recv_buffer.enqueue_slice(buffer);
                                    (n, ())
                                });

                                match result {
                                    Ok(..) => {
                                        has_received = true;
                                    }
                                    Err(err) => {
                                        error!("socket recv error: {}", err);
                                        sockets_to_remove.push(socket_handle);
                                        close_socket_control(&mut *control);
                                        break;
                                    }
                                }
                            }

                            if has_received {
                                if let Some(waker) = control.recv_waker.take() {
                                    waker.wake();
                                }
                            }

                            // Check if writable
                            let mut has_sent = false;
                            while socket.can_send() && !control.send_buffer.is_empty() {
                                let result = socket.send(|buffer| {
                                    let n = control.send_buffer.dequeue_slice(buffer);
                                    (n, ())
                                });

                                match result {
                                    Ok(..) => {
                                        has_sent = true;
                                    }
                                    Err(err) => {
                                        error!("socket send error: {}", err);
                                        sockets_to_remove.push(socket_handle);
                                        close_socket_control(&mut *control);
                                        break;
                                    }
                                }
                            }

                            if has_sent {
                                if let Some(waker) = control.send_waker.take() {
                                    waker.wake();
                                }
                            }
                        }

                        for socket_handle in sockets_to_remove {
                            sockets.remove(&socket_handle);
                            iface.remove_socket(socket_handle);
                        }

                        let next_duration = iface.poll_delay(Instant::now()).unwrap_or(Duration::from_millis(50));

                        next_duration
                    };

                    tokio::select! {
                        _ = time::sleep(StdDuration::from(next_duration)) => {}
                        _ = manager_notify.notified() => {}
                    }
                }
            })
        };

        TcpTun {
            context,
            manager,
            manager_handle,
            manager_notify,
            balancer,
            iface_rx,
            iface_tx,
        }
    }

    pub async fn handle_packet(
        &mut self,
        src_addr: SocketAddr,
        dst_addr: SocketAddr,
        tcp_packet: &TcpPacket<&[u8]>,
    ) -> io::Result<()> {
        // TCP first handshake packet, create a new Connection
        if tcp_packet.syn() && !tcp_packet.ack() {
            let accept_opts = self.context.accept_opts();

            let send_buffer_size = accept_opts.tcp.send_buffer_size.unwrap_or(DEFAULT_TCP_SEND_BUFFER_SIZE);
            let recv_buffer_size = accept_opts.tcp.recv_buffer_size.unwrap_or(DEFAULT_TCP_RECV_BUFFER_SIZE);

            let mut socket = TcpSocket::new(
                TcpSocketBuffer::new(vec![0u8; recv_buffer_size as usize]),
                TcpSocketBuffer::new(vec![0u8; send_buffer_size as usize]),
            );
            socket.set_keep_alive(accept_opts.tcp.keepalive.map(From::from));
            // FIXME: This should follows system's setting. 7200 is Linux's default.
            socket.set_timeout(Some(Duration::from_secs(7200)));

            if let Err(err) = socket.listen(dst_addr) {
                return Err(io::Error::new(ErrorKind::Other, err));
            }

            trace!("created TCP connection for {} <-> {}", src_addr, dst_addr);

            let connection = TcpConnection::new(socket, self.manager.clone(), &accept_opts.tcp);

            // establish a tunnel
            let context = self.context.clone();
            let balancer = self.balancer.clone();
            tokio::spawn(async move {
                if let Err(err) = handle_redir_client(context, balancer, connection, src_addr, dst_addr).await {
                    error!("TCP tunnel failure, {} <-> {}, error: {}", src_addr, dst_addr, err);
                }
            });
        }

        Ok(())
    }

    pub async fn drive_interface_state(&mut self, frame: &[u8]) {
        if let Err(..) = self.iface_tx.send(frame.to_vec()).await {
            panic!("interface send channel closed unexpectly");
        }

        // Wake up and poll the interface.
        self.manager_notify.notify_one();
    }

    pub async fn recv_packet(&mut self) -> Vec<u8> {
        match self.iface_rx.recv().await {
            Some(v) => v,
            None => unreachable!("channel closed unexpectedly"),
        }
    }
}

/// Established Client Transparent Proxy
///
/// This method must be called after handshaking with client (for example, socks5 handshaking)
async fn establish_client_tcp_redir<'a>(
    context: Arc<ServiceContext>,
    balancer: PingBalancer,
    mut stream: TcpConnection,
    peer_addr: SocketAddr,
    addr: &Address,
) -> io::Result<()> {
    let server = balancer.best_tcp_server();
    let svr_cfg = server.server_config();

    let mut remote = AutoProxyClientStream::connect(context, &server, addr).await?;

    establish_tcp_tunnel(svr_cfg, &mut stream, &mut remote, peer_addr, addr).await
}

async fn handle_redir_client(
    context: Arc<ServiceContext>,
    balancer: PingBalancer,
    s: TcpConnection,
    peer_addr: SocketAddr,
    mut daddr: SocketAddr,
) -> io::Result<()> {
    // Get forward address from socket
    //
    // Try to convert IPv4 mapped IPv6 address for dual-stack mode.
    if let SocketAddr::V6(ref a) = daddr {
        if let Some(v4) = to_ipv4_mapped(a.ip()) {
            daddr = SocketAddr::new(IpAddr::from(v4), a.port());
        }
    }
    let target_addr = Address::from(daddr);
    establish_client_tcp_redir(context, balancer, s, peer_addr, &target_addr).await
}
