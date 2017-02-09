use std::cmp::{PartialOrd, Ordering, min};
use std::collections::{HashMap, BinaryHeap};
use std::mem;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Instant, Duration};

use cbor::{Encoder, Decoder};
use futures::{Future, Async, Stream};
use quick_error::ResultExt;
use tk_easyloop::{self, timeout};
use tokio_core::net::UdpSocket;
use tokio_core::reactor::Timeout;
use void::{Void, unreachable};

use gossip::command::Command;
use gossip::Config;
use gossip::constants::MAX_PACKET_SIZE;
use gossip::errors::InitError;
use gossip::info::Info;
use gossip::peer::{Report, Peer};
use {HostId};
use time_util::time_ms;


#[derive(Eq)]
struct FutureHost {
    deadline: Instant,
    address: SocketAddr,
    attempts: u32,
    timeout: Duration,
}

#[derive(Clone, Copy)]
enum AddrStatus {
    Available,
    PingSent,
}

pub struct Proto<S> {
    sock: UdpSocket,
    config: Arc<Config>,
    info: Arc<Mutex<Info>>,
    addr_status: HashMap<SocketAddr, AddrStatus>,
    queue: BinaryHeap<FutureHost>,
    next_ping: Instant,
    clock: Timeout,
    stream: S,
    buf: Vec<u8>,
}

#[derive(Debug, Clone, RustcEncodable, RustcDecodable)]
pub enum Packet {
    Ping {
        cluster: Arc<String>,
        me: MyInfo,
        now: u64,
        friends: Vec<FriendInfo>,
    },
    Pong {
        cluster: Arc<String>,
        me: MyInfo,
        ping_time: u64,
        peer_time: u64,
        friends: Vec<FriendInfo>,
    },
}

#[derive(Debug, Clone, RustcEncodable, RustcDecodable)]
pub struct MyInfo {
    id: HostId,
    addresses: Arc<Vec<String>>,
    host: Arc<String>,
    name: Arc<String>,
    report: Report,
}

#[derive(Debug, Clone, RustcEncodable, RustcDecodable)]
pub struct FriendInfo {
    pub id: HostId,
    pub my_primary_addr: Option<String>,
    pub addresses: Vec<String>,
    pub host: Option<String>,
    pub name: Option<String>,
    pub report: Option<(u64, Report)>,
    pub roundtrip: Option<(u64, u64)>,
}


impl<S: Stream<Item=Command, Error=Void>> Proto<S> {
    pub fn new(info: &Arc<Mutex<Info>>, config: &Arc<Config>, stream: S)
       -> Result<Proto<S>, InitError>
    {
        let s = UdpSocket::bind(&config.bind, &tk_easyloop::handle())
            .context(config.bind)?;
        Ok(Proto {
            sock: s,
            config: config.clone(),
            info: info.clone(),
            stream: stream,
            addr_status: HashMap::new(),
            queue: BinaryHeap::new(),
            next_ping: Instant::now() + config.interval,
            clock: timeout(config.interval),
            buf: vec![0; config.max_packet_size],
        })
    }
}

impl<S: Stream<Item=Command, Error=Void>> Future for Proto<S> {
    type Item = ();
    type Error = ();
    fn poll(&mut self) -> Result<Async<()>, ()> {
        let current_timeout = self.next_wakeup();
        loop {
            self.internal_messages().unwrap_or_else(|e| unreachable(e));
            self.receive_messages();

            let new_timeout = self.next_wakeup();
            if new_timeout != current_timeout {
                let now = Instant::now();
                if new_timeout <= now {
                    continue;
                } else {
                    let mut timeo = timeout(new_timeout.duration_since(now));
                    // We need to `poll` it to get wakeup scheduled
                    match timeo.poll().map_err(|_| ())? {
                        Async::Ready(()) => continue,
                        Async::NotReady => {}
                    }
                    self.clock = timeo;
                    break;
                }
            } else {
                break;
            }
        }
        Ok(Async::NotReady)
    }
}

impl<S: Stream<Item=Command, Error=Void>> Proto<S> {
    fn next_wakeup(&self) -> Instant {
        self.queue.peek().map(|x| min(x.deadline, self.next_ping))
            .unwrap_or(self.next_ping)
    }
    fn internal_messages(&mut self) -> Result<(), Void> {
        use gossip::command::Command::*;
        while let Async::Ready(msg) = self.stream.poll()? {
            let msg = msg.expect("gossip stream never ends");
            match msg {
                AddHost(addr) => {
                    use self::AddrStatus::*;
                    let status = self.addr_status.get(&addr).map(|x| *x);
                    match status {
                        Some(Available) => {}
                        Some(PingSent)|None => {
                            // We send ping anyway, so that you can trigger
                            // adding failed host faster (not waiting for
                            // longer exponential back-off at this moment).
                            //
                            // While at a glance this may make us susceptible
                            // to DoS attacks, but presumably this requires a
                            // lot less resources than the initial HTTP
                            // request or websocket message that triggers
                            // `AddHost()` message itself
                            self.send_gossip(addr);
                        }
                    }
                    match status {
                        Some(Available) => {}
                        // .. but we keep same timestamp for the next retry
                        // to avoid memory leaks
                        Some(PingSent) => { }
                        None => {
                            self.addr_status.insert(addr, PingSent);
                            let timeout = self.config.add_host_first_sleep();
                            self.queue.push(FutureHost {
                                deadline: Instant::now() + timeout,
                                address: addr,
                                attempts: 1,
                                timeout: timeout,
                            });
                        }
                    }
                }
            }
        }
        Ok(())
    }
    fn receive_messages(&mut self) -> Result<(), Void> {
        // Steal buffer to satisfy borrow checker
        // It should be cheap, as empty vector is non-allocating
        let mut buf = mem::replace(&mut self.buf, Vec::new());
        assert!(buf.len() == self.config.max_packet_size);

        while let Ok((bytes, addr)) = self.sock.recv_from(&mut buf) {
            let data = &buf[..bytes];
            let mut dec = Decoder::from_reader(data);
            match dec.decode::<Packet>().next() {
                Some(Ok(packet)) => {
                    trace!("Packet {:?} from {:?}", packet, addr);
                    self.consume_gossip(packet, addr);
                }
                None => {
                    warn!("Empty or truncated packet from {:?}",
                          addr);
                }
                Some(Err(e)) => {
                    warn!("Errorneous packet from {:?}: {}",
                        addr, e);
                }
            }
        }
        // return buffer back
        self.buf = buf;
        Ok(())
    }
    pub fn consume_gossip(&mut self, packet: Packet, addr: SocketAddr) {
        let tm = time_ms();

        match packet {
            Packet::Ping { cluster,  me: pinfo, now, friends } => {
                {
                    if cluster != self.config.cluster_name {
                        info!("Got packet from cluster {:?}", cluster);
                        return;
                    }
                    if pinfo.id == self.config.machine_id {
                        debug!("Got packet from myself");
                        return;
                    }
                    let id = pinfo.id.clone();
                    let mut info = self.info.lock()
                        .expect("gossip info poisoned");
                    let peer = info.peers.entry(id.clone())
                        .or_insert_with(|| Peer::new(id.clone()));
                    peer.apply_addresses(
                        // TODO(tailhook) filter out own IP addressses
                        pinfo.addresses.iter().filter_map(|x| x.parse().ok()),
                        true);
                    peer.apply_report(Some((tm, pinfo.report)), true);
                    peer.apply_hostname(Some(pinfo.host.as_ref()), true);
                    peer.apply_node_name(Some(pinfo.name.as_ref()), true);
                    peer.pings_received += 1;
                    if peer.primary_addr.as_ref() != Some(&addr) {
                        peer.primary_addr = Some(addr);
                        self.send_touch(id);
                    }
                }
                self.apply_friends(friends, addr);
                let mut buf = Vec::with_capacity(self.config.max_packet_size);
                {
                    let info = self.info.lock().expect("gossip info poisoned");
                    let mut e = Encoder::from_writer(&mut buf);
                    e.encode(&[&Packet::Pong {
                        cluster: cluster,
                        me: MyInfo {
                            id: self.config.machine_id.clone(),
                            addresses: self.config.str_addresses.clone(),
                            host: self.config.hostname.clone(),
                            name: self.config.name.clone(),
                            report: Report {
                                peers: info.peers.len() as u32,
                                has_remote: info.has_remote,
                            },
                        },
                        ping_time: now,
                        peer_time: tm,
                        friends: info.get_friends(addr),
                    }]).unwrap();
                }

                if buf.len() == MAX_PACKET_SIZE {
                    // Unfortunately cbor encoder doesn't report error of
                    // truncated data so we consider full buffer the truncated
                    // data
                    error!("Error sending probe to {}: Data is too long. \
                        All limits are compile-time. So this error basically \
                        means  cantal developers were unwise at choosing the \
                        right values. If you didn't tweak the limits \
                        yourself, please file an issue at \
                        http://github.com/tailhook/cantal/issues", addr);
                }
                self.sock.send_to(&buf[..], &addr)
                    .map_err(|e| error!("Error sending probe to {:?}: {}",
                        addr, e))
                    .ok();
            }
            Packet::Pong { cluster, me: pinfo, ping_time, peer_time, friends }
            => {
                {
                    if cluster != self.config.cluster_name {
                        info!("Got packet from cluster {:?}", cluster);
                        return;
                    }
                    if pinfo.id == self.config.machine_id {
                        debug!("Got packet from myself");
                        return;
                    }
                    let mut info = self.info.lock()
                        .expect("gossip info poisoned");
                    let id = pinfo.id.clone();
                    let peer = info.peers.entry(id.clone())
                        .or_insert_with(|| Peer::new(id.clone()));
                    peer.apply_addresses(
                        // TODO(tailhook) filter out own IP addressses
                        pinfo.addresses.iter().filter_map(|x| x.parse().ok()),
                        true);
                    peer.apply_report(Some((tm, pinfo.report)), true);
                    peer.pongs_received += 1;
                    // sanity check
                    if ping_time <= tm && ping_time <= peer_time {
                        peer.apply_roundtrip((tm, (tm - ping_time)),
                            addr, true);
                    }
                    peer.apply_hostname(Some(pinfo.host.as_ref()), true);
                    peer.apply_node_name(Some(pinfo.name.as_ref()), true);
                    if peer.primary_addr.as_ref() != Some(&addr) {
                        peer.primary_addr = Some(addr);
                        self.send_touch(id);
                    }
                }
                self.apply_friends(friends, addr);
            }
        }
    }
    fn send_gossip(&mut self, addr: SocketAddr) {
        debug!("Sending gossip {}", addr);
        let mut buf = Vec::with_capacity(MAX_PACKET_SIZE);
        {
            let info = self.info.lock().expect("gossip info poisoned");
            let mut e = Encoder::from_writer(&mut buf);
            e.encode(&[&Packet::Ping {
                cluster: self.config.cluster_name.clone(),
                me: MyInfo {
                    id: self.config.machine_id.clone(),
                    addresses: self.config.str_addresses.clone(),
                    host: self.config.hostname.clone(),
                    name: self.config.name.clone(),
                    report: Report {
                        peers: info.peers.len() as u32,
                        has_remote: info.has_remote,
                    },
                },
                now: time_ms(),
                friends: info.get_friends(addr),
            }]).unwrap();
        }
        if buf.len() >= MAX_PACKET_SIZE {
            // Unfortunately cbor encoder doesn't report error of truncated
            // data so we consider full buffer the truncated data
            error!("Error sending probe to {}: Data is too long. \
                All limits are compile-time. So this error basically means \
                cantal developers were unwise at choosing the right values. \
                If you didn't tweak the limits yourself, please file an issue \
                at http://github.com/tailhook/cantal/issues", addr);
        }
        if let Err(e) = self.sock.send_to(&buf[..], &addr) {
            error!("Error sending probe to {}: {}", addr, e);
        }
    }
    fn send_touch(&self, _id: HostId) {
        // TODO(tailhook) this is a notification to a network subsystem that
        // new host created (i.e. we should connect to it with a websocket)
        unimplemented!();
    }
    fn apply_friends(&mut self, friends: Vec<FriendInfo>, source: SocketAddr) {
        for friend in friends.into_iter() {
            let sendto_addr = {
                let id = friend.id;
                if id == self.config.machine_id {
                    debug!("Got myself in friend list");
                    continue;
                }
                let mut info = self.info.lock()
                    .expect("gossip info poisoned");
                let peer = info.peers.entry(id.clone())
                    .or_insert_with(|| Peer::new(id.clone()));
                peer.apply_addresses(
                    // TODO(tailhook) filter out own IP addressses
                    friend.addresses.iter().filter_map(|x| x.parse().ok()),
                    false);
                peer.apply_report(friend.report, false);
                peer.apply_hostname(friend.host.as_ref().map(|x| &**x), false);
                peer.apply_node_name(
                    friend.name.as_ref().map(|x| &**x), false);
                friend.roundtrip.map(|rtt|
                    peer.apply_roundtrip(rtt, source, false));
                if peer.primary_addr.is_none() {
                    let addr = friend.my_primary_addr.and_then(|x| {
                        x.parse().map_err(|_| error!("Can't parse IP address"))
                        .ok()
                    });
                    peer.primary_addr = addr;
                    addr.map(|addr| {
                        self.send_touch(id);
                        peer.last_probe = Some((time_ms(), addr));
                        peer.probes_sent += 1;
                        addr
                    });
                    addr
                } else {
                    None
                }
            };
            sendto_addr.map(|addr| {
                self.send_gossip(addr);
            });
        }
    }
}

impl Ord for FutureHost {
    fn cmp(&self, other: &FutureHost) -> Ordering {
        self.deadline.cmp(&other.deadline)
    }
}

impl PartialOrd for FutureHost {
    fn partial_cmp(&self, other: &FutureHost) -> Option<Ordering> {
        self.deadline.partial_cmp(&other.deadline)
    }
}

impl PartialEq for FutureHost {
    fn eq(&self, other: &FutureHost) -> bool {
        self.deadline.eq(&other.deadline)
    }
}
