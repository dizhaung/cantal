use std::net::{SocketAddr};
use std::str::FromStr;
use std::collections::HashMap;

use time::{Timespec, Duration, get_time};
use rand::{thread_rng, Rng};
use cbor::Encoder;
use mio::buf::ByteBuf;

use super::Context;
use super::peer::{Peer, Report};


pub const INTERVAL: u64 = 1000;
pub const MIN_PROBE: u64 = 5000;  // Don't probe more often than 5 sec
pub const NUM_FRIENDS: u64 = 10;

#[derive(Debug, Clone, RustcEncodable, RustcDecodable)]
pub enum Packet {
    Ping {
        me: MyInfo,
        now: Timespec,
        friends: Vec<FriendInfo>,
    },
    Pong {
        me: MyInfo,
        ping_time: Timespec,
        peer_time: Timespec,
        friends: Vec<FriendInfo>,
    },
}

#[derive(Debug, Clone, RustcEncodable, RustcDecodable)]
pub struct MyInfo {
    host: String,
    peers: u32,
}

#[derive(Debug, Clone, RustcEncodable, RustcDecodable)]
pub struct FriendInfo {
    ip: String,
    host: String,
    peers: u32,
    last_report: Option<Timespec>,
    roundtrip: Option<(Timespec, u64)>,
}


fn after(tm: Option<Timespec>, target_time: Timespec) -> bool {
    return tm.map(|x| x >= target_time).unwrap_or(false);
}


impl Context {
    pub fn gossip_broadcast(&mut self) {
        let cut_time = get_time() - Duration::milliseconds(MIN_PROBE as i64);
        let mut stats = self.stats.write().unwrap();
        if self.queue.len() == 0 {
            if stats.peers.len() == 0 {
                return // nothing to do
            }
            self.queue = stats.peers.keys().cloned().collect();
        }
        thread_rng().shuffle(&mut self.queue[..]);
        for _ in 0..NUM_FRIENDS {
            if self.queue.len() == 0 {
                break;
            }
            let target_ip = self.queue.pop().unwrap();
            // if not expired yet
            if let Some(peer) = stats.peers.get(&target_ip) {
                if after(peer.last_probe, cut_time) ||
                   after(peer.last_report, cut_time) {
                    continue;  // don't probe too often
                }
            }
            self.send_gossip(target_ip, &mut stats.peers);
        }
    }
    pub fn send_gossip(&self, addr: SocketAddr,
                       peers: &mut HashMap<SocketAddr, Peer>)
    {
        debug!("Sending gossip to {}", addr);
        let mut buf = ByteBuf::mut_with_capacity(1024);
        {
            let mut e = Encoder::from_writer(&mut buf);
            e.encode(&[&Packet::Ping {
                me: MyInfo {
                    host: self.hostname.clone(),
                    peers: peers.len() as u32,
                },
                now: get_time(),
                friends: vec![],
            }]).unwrap();
        }
        match self.sock.send_to(&mut buf.flip(),
            &FromStr::from_str(&format!("{}", addr)).unwrap()) {
            Ok(_) => {
                let peer = peers.entry(addr)
                    .or_insert_with(|| Peer::new(addr));
                peer.last_probe = Some(get_time());
            }
            Err(_) => {
                error!("Error sending probe to {:?}", addr);
            }
        }
    }

    pub fn consume_gossip(&self, packet: Packet, addr: SocketAddr) {
        let tm = get_time();
        let mut stats = self.stats.write().unwrap();
        let peer = stats.peers.entry(addr)
                    .or_insert_with(|| Peer::new(addr));
        match packet {
            Packet::Ping { me: info, .. } => {
                peer.report = Some(Report {
                    peers: info.peers,
                });
                peer.host = Some(info.host.clone());
                peer.last_report = Some(tm);
            }
            other => {
                error!("Unknown packet {:?}", other);
                return;
            }
        }
    }
}
