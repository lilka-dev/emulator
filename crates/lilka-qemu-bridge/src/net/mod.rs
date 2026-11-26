pub const MAX_FRAME: usize = 1514;
pub const QUEUE: usize = 256;
fn validate_frame(frame: &[u8]) -> Result<()> {
    ensure!(
        (14..=MAX_FRAME).contains(&frame.len()),
        "Invalid Ethernet frame length"
    );
    Ok(())
}
mod dhcp;
use anyhow::{Result, ensure};
use smoltcp::{
    iface::{Config, Interface, SocketHandle, SocketSet},
    phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken},
    socket::{tcp, udp},
    time::Instant,
    wire::{
        EthernetAddress, EthernetFrame, EthernetProtocol, IpAddress, IpCidr, IpEndpoint,
        IpListenEndpoint, IpProtocol, Ipv4Address, Ipv4Packet, TcpPacket, UdpPacket,
    },
};
use std::{
    collections::{HashMap, VecDeque},
    io::{Read, Write},
    net::{Ipv4Addr, SocketAddr, TcpStream, UdpSocket},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    time::{Duration, Instant as Clock},
};
const GATEWAY: Ipv4Address = Ipv4Address::new(192, 168, 4, 1);
const FLOWS: usize = 64;
#[derive(Default)]
struct Wire {
    rx: VecDeque<Vec<u8>>,
    tx: VecDeque<Vec<u8>>,
}
struct Rx(Vec<u8>);
struct Tx<'a>(&'a mut VecDeque<Vec<u8>>);
impl RxToken for Rx {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.0)
    }
}
impl TxToken for Tx<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut data = vec![0; len];
        let result = f(&mut data);
        if self.0.len() < QUEUE && len <= MAX_FRAME {
            self.0.push_back(data);
        }
        result
    }
}
impl Device for Wire {
    type RxToken<'a> = Rx;
    type TxToken<'a> = Tx<'a>;
    fn receive(&mut self, _: Instant) -> Option<(Rx, Tx<'_>)> {
        self.rx.pop_front().map(|p| (Rx(p), Tx(&mut self.tx)))
    }
    fn transmit(&mut self, _: Instant) -> Option<Tx<'_>> {
        (self.tx.len() < QUEUE).then_some(Tx(&mut self.tx))
    }
    fn capabilities(&self) -> DeviceCapabilities {
        let mut c = DeviceCapabilities::default();
        c.medium = Medium::Ethernet;
        c.max_transmission_unit = MAX_FRAME;
        c
    }
}
struct TcpFlow {
    handle: SocketHandle,
    guest: IpEndpoint,
    destination: IpEndpoint,
    host: TcpStream,
    seen: Clock,
    host_eof: bool,
}
struct UdpFlow {
    host: UdpSocket,
    seen: Clock,
}
pub struct UserNetwork {
    iface: Interface,
    wire: Wire,
    sockets: SocketSet<'static>,
    tcp: Vec<TcpFlow>,
    udp: HashMap<IpEndpoint, SocketHandle>,
    udp_flows: HashMap<(IpEndpoint, IpEndpoint), UdpFlow>,
    start: Clock,
    dns: Ipv4Addr,
}
impl UserNetwork {
    pub fn new() -> Self {
        let mut wire = Wire::default();
        let mut config = Config::new(EthernetAddress([2, 0x4c, 0x49, 0x4c, 0x4b, 1]).into());
        config.random_seed = 0x4c494c4b41;
        let mut iface = Interface::new(config, &mut wire, Instant::ZERO);
        iface.update_ip_addrs(|addrs| addrs.push(IpCidr::new(GATEWAY.into(), 24)).unwrap());
        iface.routes_mut().add_default_ipv4_route(GATEWAY).unwrap();
        iface.set_any_ip(true);
        Self {
            iface,
            wire,
            sockets: SocketSet::new(vec![]),
            tcp: vec![],
            udp: HashMap::new(),
            udp_flows: HashMap::new(),
            start: Clock::now(),
            dns: Ipv4Addr::new(1, 1, 1, 1),
        }
    }
    fn destination(&self, endpoint: IpEndpoint) -> SocketAddr {
        let IpAddress::Ipv4(ip) = endpoint.addr;
        let ip = if ip == GATEWAY {
            if endpoint.port == 53 {
                self.dns
            } else {
                Ipv4Addr::LOCALHOST
            }
        } else {
            ip
        };
        SocketAddr::from((ip, endpoint.port))
    }
    pub fn receive(&mut self, frame: Vec<u8>) -> Result<()> {
        validate_frame(&frame)?;
        if let Some(reply) = dhcp::reply(&frame) {
            if self.wire.tx.len() < QUEUE {
                self.wire.tx.push_back(reply);
            }
            return Ok(());
        }
        if frame.len() >= 42
            && frame[12..14] == [8, 6]
            && frame[20..22] == [0, 1]
            && frame[38..42] != [192, 168, 4, 1]
        {
            return Ok(());
        }
        ensure!(self.wire.rx.len() < QUEUE, "Network RX queue full");
        let ethernet = EthernetFrame::new_checked(frame.as_slice())?;
        if ethernet.ethertype() == EthernetProtocol::Ipv4 {
            let ip = Ipv4Packet::new_checked(ethernet.payload())?;
            if ip.verify_checksum() && !ip.more_frags() && ip.frag_offset() == 0 {
                match ip.next_header() {
                    IpProtocol::Tcp => {
                        let p = TcpPacket::new_checked(ip.payload())?;
                        if p.syn()
                            && !p.ack()
                            && p.verify_checksum(&ip.src_addr().into(), &ip.dst_addr().into())
                        {
                            let guest = IpEndpoint::new(ip.src_addr().into(), p.src_port());
                            let destination = IpEndpoint::new(ip.dst_addr().into(), p.dst_port());
                            if !self
                                .tcp
                                .iter()
                                .any(|f| f.guest == guest && f.destination == destination)
                                && self.tcp.len() < FLOWS
                            {
                                let mut socket = tcp::Socket::new(
                                    tcp::SocketBuffer::new(vec![0; 32768]),
                                    tcp::SocketBuffer::new(vec![0; 32768]),
                                );
                                socket.set_timeout(Some(smoltcp::time::Duration::from_secs(120)));
                                socket.listen(IpListenEndpoint::from(destination))?;
                                if let Ok(host) = TcpStream::connect_timeout(
                                    &self.destination(destination),
                                    Duration::from_millis(200),
                                ) {
                                    host.set_nonblocking(true)?;
                                    let handle = self.sockets.add(socket);
                                    self.tcp.push(TcpFlow {
                                        handle,
                                        guest,
                                        destination,
                                        host,
                                        seen: Clock::now(),
                                        host_eof: false,
                                    });
                                }
                            }
                        }
                    }
                    IpProtocol::Udp => {
                        let p = UdpPacket::new_checked(ip.payload())?;
                        let destination = IpEndpoint::new(ip.dst_addr().into(), p.dst_port());
                        if p.dst_port() != 67
                            && !self.udp.contains_key(&destination)
                            && self.udp.len() < FLOWS
                        {
                            let mut socket = udp::Socket::new(
                                udp::PacketBuffer::new(
                                    vec![udp::PacketMetadata::EMPTY; 16],
                                    vec![0; 32768],
                                ),
                                udp::PacketBuffer::new(
                                    vec![udp::PacketMetadata::EMPTY; 16],
                                    vec![0; 32768],
                                ),
                            );
                            socket.bind(IpListenEndpoint::from(destination))?;
                            self.udp.insert(destination, self.sockets.add(socket));
                        }
                    }
                    _ => {}
                }
            }
        }
        self.wire.rx.push_back(frame);
        Ok(())
    }
    pub fn poll(&mut self) -> Vec<Vec<u8>> {
        let now = Instant::from_millis(self.start.elapsed().as_millis() as i64);
        self.iface.poll(now, &mut self.wire, &mut self.sockets);
        for flow in &mut self.tcp {
            let socket = self.sockets.get_mut::<tcp::Socket>(flow.handle);
            if socket.can_recv() {
                let _ = socket.recv(|bytes| match flow.host.write(bytes) {
                    Ok(n) => {
                        flow.seen = Clock::now();
                        (n, ())
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => (0, ()),
                    Err(_) => {
                        flow.host_eof = true;
                        (0, ())
                    }
                });
            }
            if socket.can_send() && !flow.host_eof {
                let _ = socket.send(|bytes| match flow.host.read(bytes) {
                    Ok(0) => {
                        flow.host_eof = true;
                        (0, ())
                    }
                    Ok(n) => {
                        flow.seen = Clock::now();
                        (n, ())
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => (0, ()),
                    Err(_) => {
                        flow.host_eof = true;
                        (0, ())
                    }
                });
            }
            if flow.host_eof {
                socket.close();
            }
            if socket.state() == tcp::State::CloseWait && !socket.can_recv() {
                let _ = flow.host.shutdown(std::net::Shutdown::Write);
            }
            if flow.seen.elapsed() > Duration::from_secs(120) {
                socket.abort();
            }
        }
        self.tcp.retain(|flow| {
            if !self.sockets.get::<tcp::Socket>(flow.handle).is_open() {
                self.sockets.remove(flow.handle);
                false
            } else {
                true
            }
        });
        let mut bytes = [0; 65536];
        for (&destination, &handle) in &self.udp {
            loop {
                let received = self
                    .sockets
                    .get_mut::<udp::Socket>(handle)
                    .recv_slice(&mut bytes);
                let Ok((length, meta)) = received else { break };
                let key = (meta.endpoint, destination);
                if !self.udp_flows.contains_key(&key) && self.udp_flows.len() < FLOWS {
                    let target = self.destination(destination);
                    if let Ok(host) = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
                        && host.connect(target).is_ok()
                        && host.set_nonblocking(true).is_ok()
                    {
                        self.udp_flows.insert(
                            key,
                            UdpFlow {
                                host,
                                seen: Clock::now(),
                            },
                        );
                    }
                }
                if let Some(flow) = self.udp_flows.get_mut(&key) {
                    let _ = flow.host.send(&bytes[..length]);
                    flow.seen = Clock::now();
                }
            }
        }
        self.udp_flows.retain(|&(guest, destination), flow| {
            for _ in 0..16 {
                match flow.host.recv(&mut bytes) {
                    Ok(n) => {
                        if let Some(&h) = self.udp.get(&destination) {
                            let _ = self
                                .sockets
                                .get_mut::<udp::Socket>(h)
                                .send_slice(&bytes[..n], guest);
                        }
                        flow.seen = Clock::now();
                    }
                    Err(_) => break,
                }
            }
            flow.seen.elapsed() < Duration::from_secs(60)
        });
        self.udp.retain(|&destination, &mut handle| {
            if self.udp_flows.keys().any(|(_, d)| *d == destination) {
                true
            } else {
                self.sockets.remove(handle);
                false
            }
        });
        self.iface.poll(now, &mut self.wire, &mut self.sockets);
        self.wire.tx.drain(..).collect()
    }
}
impl Default for UserNetwork {
    fn default() -> Self {
        Self::new()
    }
}

pub struct NetworkWorker {
    tx: SyncSender<Vec<u8>>,
    rx: Receiver<Vec<u8>>,
    stop: Arc<AtomicBool>,
}
impl NetworkWorker {
    pub fn new() -> Self {
        let (tx, input) = mpsc::sync_channel(QUEUE);
        let (output, rx) = mpsc::sync_channel(QUEUE);
        let stop = Arc::new(AtomicBool::new(false));
        let signal = stop.clone();
        std::thread::spawn(move || {
            let mut net = UserNetwork::new();
            while !signal.load(Ordering::Relaxed) {
                for frame in input.try_iter().take(QUEUE) {
                    let _ = net.receive(frame);
                }
                for frame in net.poll() {
                    let _ = output.try_send(frame);
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        });
        Self { tx, rx, stop }
    }
    pub fn send(&self, frame: Vec<u8>) -> bool {
        self.tx.try_send(frame).is_ok()
    }
    pub fn receive(&self) -> impl Iterator<Item = Vec<u8>> + '_ {
        self.rx.try_iter().take(QUEUE)
    }
}
impl Default for NetworkWorker {
    fn default() -> Self {
        Self::new()
    }
}
impl Drop for NetworkWorker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    fn peer() -> (Interface, Wire, SocketSet<'static>) {
        let mut wire = Wire::default();
        let config = Config::new(EthernetAddress([2, 3, 4, 5, 6, 7]).into());
        let mut iface = Interface::new(config, &mut wire, Instant::ZERO);
        iface.update_ip_addrs(|a| {
            a.push(IpCidr::new(IpAddress::v4(192, 168, 4, 2), 24))
                .unwrap()
        });
        iface.routes_mut().add_default_ipv4_route(GATEWAY).unwrap();
        (iface, wire, SocketSet::new(vec![]))
    }
    fn exchange(
        iface: &mut Interface,
        wire: &mut Wire,
        sockets: &mut SocketSet<'static>,
        net: &mut UserNetwork,
        start: Clock,
    ) {
        iface.poll(
            Instant::from_millis(start.elapsed().as_millis() as i64),
            wire,
            sockets,
        );
        for packet in wire.tx.drain(..) {
            net.receive(packet).unwrap();
        }
        wire.rx.extend(net.poll());
        std::thread::sleep(Duration::from_millis(1));
    }
    #[test]
    fn ethernet_arp_and_tcp_reach_real_host_echo() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let host = std::thread::spawn(move || {
            let start = Clock::now();
            loop {
                if let Ok((mut stream, _)) = listener.accept() {
                    // BSD/macOS sockets inherit O_NONBLOCK from the listener; Linux ones do not.
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(3)))
                        .unwrap();
                    let mut data = [0; 4];
                    stream.read_exact(&mut data).unwrap();
                    assert_eq!(&data, b"ping");
                    stream.write_all(b"pong").unwrap();
                    break;
                }
                assert!(start.elapsed() < Duration::from_secs(5));
                std::thread::sleep(Duration::from_millis(1));
            }
        });
        let (mut iface, mut wire, mut sockets) = peer();
        let mut net = UserNetwork::new();
        let mut tcp = tcp::Socket::new(
            tcp::SocketBuffer::new(vec![0; 4096]),
            tcp::SocketBuffer::new(vec![0; 4096]),
        );
        tcp.connect(iface.context(), (GATEWAY, port), 45000)
            .unwrap();
        let handle = sockets.add(tcp);
        let start = Clock::now();
        let mut sent = false;
        let mut reply: Vec<u8> = Vec::new();
        while start.elapsed() < Duration::from_secs(5) {
            exchange(&mut iface, &mut wire, &mut sockets, &mut net, start);
            let socket = sockets.get_mut::<tcp::Socket>(handle);
            if socket.can_send() && !sent {
                socket.send_slice(b"ping").unwrap();
                sent = true;
            }
            if socket.can_recv() {
                socket
                    .recv(|bytes| {
                        reply.extend_from_slice(bytes);
                        (bytes.len(), ())
                    })
                    .unwrap();
            }
            if reply.len() >= 4 {
                break;
            }
        }
        assert_eq!(&reply, b"pong");
        host.join().unwrap();
    }
    #[test]
    fn ethernet_udp_reaches_real_host_echo() {
        let host = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = host.local_addr().unwrap().port();
        host.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let server = std::thread::spawn(move || {
            let mut b = [0; 4];
            let (n, peer) = host.recv_from(&mut b).unwrap();
            assert_eq!(&b[..n], b"ping");
            host.send_to(b"pong", peer).unwrap();
        });
        let (mut iface, mut wire, mut sockets) = peer();
        let mut net = UserNetwork::new();
        let mut udp = udp::Socket::new(
            udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 4], vec![0; 4096]),
            udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 4], vec![0; 4096]),
        );
        udp.bind(45000).unwrap();
        udp.send_slice(b"ping", (GATEWAY, port)).unwrap();
        let handle = sockets.add(udp);
        let start = Clock::now();
        let mut reply: Vec<u8> = Vec::new();
        while start.elapsed() < Duration::from_secs(5) {
            exchange(&mut iface, &mut wire, &mut sockets, &mut net, start);
            if let Ok((data, meta)) = sockets.get_mut::<udp::Socket>(handle).recv() {
                assert_eq!(meta.endpoint, IpEndpoint::new(GATEWAY.into(), port));
                reply.extend(data);
                break;
            }
        }
        assert_eq!(&reply, b"pong");
        server.join().unwrap();
    }
}
