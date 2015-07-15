use std::iter::repeat;
use std::sync::{Arc, RwLock};
use std::net::SocketAddr;

use unicase::UniCase;
use byteorder::{BigEndian, ByteOrder};
use hyper::header::{Upgrade, ProtocolName};
use hyper::header::{Connection};
use hyper::version::HttpVersion as Version;
use hyper::header::ConnectionOption::ConnectionHeader;
use websocket::header::{WebSocketVersion, WebSocketKey};
use rustc_serialize::json;

use super::http;
use super::scan::time_ms;
use super::remote::Peers;
use super::p2p::GossipStats;
use super::http::{Request, BadRequest};
use super::util::Consume;
use super::server::{Context};
use super::stats::Stats;
use super::deps::{Dependencies, LockedDeps};


#[derive(RustcEncodable, RustcDecodable)]
struct Beacon {
    current_time: u64,
    startup_time: u64,
    boot_time: Option<u64>,
    scan_time: u64,
    scan_duration: u32,
    processes: usize,
    values: usize,
    peers: usize,
    fine_history_length: usize,
    history_age: u64,
    remote_total: Option<usize>,
    remote_connected: Option<usize>,
}

#[derive(RustcEncodable, RustcDecodable)]
enum Message {
    Beacon(Beacon),
    NewPeer(String),
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Opcode {
    Text,
    Binary,
}

impl Opcode {
    fn from(src: u8) -> Option<Opcode> {
        match src {
            1 => Some(Opcode::Text),
            2 => Some(Opcode::Binary),
            x => None,
        }
    }
}


pub fn respond_websock(req: &Request, _context: &mut Context)
    -> Result<http::Response, Box<http::Error>>
{
    if req.version != Version::Http11 {
        return Err(BadRequest::err("Unsupported request HTTP version"));
    }

    if req.headers.get() != Some(&(WebSocketVersion::WebSocket13)) {
        return Err(BadRequest::err("Unsupported WebSocket version"));
    }

    let key  = match req.headers.get::<WebSocketKey>() {
        Some(key) => key,
        None => {
            return Err(BadRequest::err("Missing Sec-WebSocket-Key"));
        }
    };

    match req.headers.get() {
        Some(&Upgrade(ref upgrade)) => {
            let mut correct_upgrade = false;
            for u in upgrade {
                if u.name == ProtocolName::WebSocket {
                    correct_upgrade = true;
                }
            }
            if !correct_upgrade {
                return Err(BadRequest::err(
                    "Invalid Upgrade WebSocket header"));
            }
        }
        None => {
            return Err(BadRequest::err("Missing Upgrade header"));
        }
    };

    match req.headers.get() {
        Some(&Connection(ref connection)) => {
            if !connection.contains(&(ConnectionHeader(
                UniCase("Upgrade".to_string()))))
            {
                return Err(BadRequest::err(
                    "Invalid Connection WebSocket header"));
            }
        }
        None => {
            return Err(BadRequest::err(
                "Missing Connection WebSocket header"));
        }
    }

    Ok(http::Response::accept_websock(key))
}

pub fn parse_message<F>(buf: &mut Vec<u8>, context: &mut Context, cb: F)
    where F: FnOnce(Opcode, &[u8], &mut Context)
{
    if buf.len() < 2 {
        return;
    }
    let fin = buf[0] & 0b10000000 != 0;
    let opcode = buf[0] & 0b00001111;
    let mask = buf[1] & 0b10000000 != 0;
    let mut ln = (buf[1] & 0b01111111) as usize;
    let mut pref = 2;
    if ln == 126 {
        if buf.len() < 4 {
            return;
        }
        ln = BigEndian::read_u16(&buf[2..4]) as usize;
        pref = 4;
    } else if ln == 127 {
        if buf.len() < 10 {
            return
        }
        ln = BigEndian::read_u64(&buf[2..10]) as usize;
        pref = 10;
    }
    if buf.len() < pref + ln + (if mask { 4 } else { 0 }) {
        return;
    }
    if mask {
        let mask = buf[pref..pref+4].to_vec(); // TODO(tailhook) optimize
        pref += 4;
        for (m, t) in mask.iter().cycle().zip(buf[pref..pref+ln].iter_mut()) {
            *t ^= *m;
        }
    }
    {
        if !fin {
            warn!("Partial frames are not supported");
        } else {
            match Opcode::from(opcode) {
                None => {
                    warn!("Invalid opcode {:?}", opcode);
                }
                Some(op) => cb(op, &buf[pref..pref+ln], context),
            }
        }
    }
    buf.consume(pref + ln);
}

pub fn write_text(buf: &mut Vec<u8>, chunk: &str) {
    let bytes = chunk.as_bytes();
    buf.push(0b10000001);  // text message
    if bytes.len() > 65535 {
        buf.push(127);
        let start = buf.len();
        buf.extend(repeat(0).take(8));
        BigEndian::write_u64(&mut buf[start ..],
                             bytes.len() as u64);
    } else if bytes.len() > 125 {
        buf.push(126);
        let start = buf.len();
        buf.extend(repeat(0).take(2));
        BigEndian::write_u16(&mut buf[start ..],
                             bytes.len() as u16);
    } else {
        buf.push(bytes.len() as u8);
    }
    buf.extend(bytes.iter().cloned());
}

pub fn beacon(deps: &Dependencies) -> String {
    // Lock one by one, to avoid deadlocks
    let (startup_time,
         boot_time,
         scan_time,
         scan_duration,
         processes,
         values,
         fine_history_length,
         history_age) = {
            let st = deps.read::<Stats>();
            (   st.startup_time,
                st.boot_time.map(|x| x*1000),
                st.last_scan,
                st.scan_duration,
                st.processes.len(),
                st.history.tip.len() + st.history.fine.len() +
                       st.history.coarse.len(),
                st.history.fine_timestamps.len(),
                st.history.age)
    };
    let gossip_peers = {
        let gossip = deps.read::<GossipStats>();
        gossip.peers.len()
    };
    let (remote_total, remote_connected) =
        if let Some(ref pr) = deps.get::<Arc<RwLock<Peers>>>() {
            let peers = pr.read().unwrap();
            (Some(peers.addresses.len()), Some(peers.connected))
        } else {
            (None, None)
        };
    json::encode(&Message::Beacon(Beacon {
        current_time: time_ms(),
        startup_time: startup_time,
        boot_time: boot_time,
        scan_time: scan_time,
        scan_duration: scan_duration,
        processes: processes,
        values: values,
        fine_history_length: fine_history_length,
        history_age: history_age,
        peers: gossip_peers,
        remote_total: remote_total,
        remote_connected: remote_connected,
    })).unwrap()
}

pub fn new_peer(peer: SocketAddr) -> String {
    json::encode(&Message::NewPeer(format!("{}", peer))).unwrap()
}
