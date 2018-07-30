use std::collections::{HashMap, HashSet};
use std::cmp::min;
use std::mem;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use futures::{Future, Stream, Async};
use futures::stream::futures_unordered::FuturesUnordered;
use futures::sync::mpsc::UnboundedReceiver;
use tokio::timer::Delay;
use tokio::clock::now;

use id::Id;
use remote::Message;
use remote::connection::Connection;
use remote::{Shared, SharedState};
use gossip::{Gossip, Peer};


pub const INITIAL_TIME: Duration = Duration::from_millis(100);
pub const MAX_TIME: Duration = Duration::from_secs(15);


pub struct Manager {
    rx: UnboundedReceiver<Message>,
    gossip: Gossip,
    state: Option<State>,
}

pub struct Throttle {
    timestamp: Instant,
    num: u32,
}

pub struct State {
    futures: FuturesUnordered<Connection>,
    active: HashSet<Id>,
    throttled: HashMap<Id, Throttle>,
    shared: Shared,
    timer: Delay,
}

impl Manager {
    pub fn new(rx: UnboundedReceiver<Message>, gossip: &Gossip) -> Manager {
        Manager {
            rx,
            gossip: gossip.clone(),
            state: None,
        }
    }

    fn receive_messages(&mut self) {
        use remote::Message::*;
        loop {
            let msg = match self.rx.poll() {
                Ok(Async::Ready(Some(msg))) => msg,
                Ok(Async::NotReady) => return,
                Ok(Async::Ready(None)) | Err(()) => {
                    panic!("remote input channel is dropped");
                }
            };
            match msg {
                Start => {
                    if self.state.is_none() {
                        let mut state = State {
                            shared: Arc::new(Mutex::new(SharedState {
                                dead_connections: Vec::new(),
                            })),
                            active: HashSet::new(),
                            futures: FuturesUnordered::new(),
                            throttled: HashMap::new(),
                            timer: Delay::new(now()),
                        };
                        state.check_connections(self.gossip.get_peers());
                        self.state = Some(state);
                    }
                }
                PeersUpdated => {
                    if let Some(ref mut state) = self.state {
                        state.check_connections(self.gossip.get_peers());
                    } else {
                        // skip it
                    }
                }
            }
        }
    }
}

impl Future for Manager {
    type Item = ();
    type Error = ();

    fn poll(&mut self) -> Result<Async<()>, ()> {
        self.receive_messages();
        if let Some(ref mut state) = self.state {
            while state.new_connections(&self.gossip) {
                state.poll_futures();
                state.dead_connections();
            }
        }
        Ok(Async::NotReady)
    }
}

impl State {
    fn shared(&mut self) -> MutexGuard<SharedState> {
        self.shared.lock()
            .expect("remote state is not poisoned")
    }
    fn poll_futures(&mut self) {
        loop {
            match self.futures.poll() {
                Ok(Async::NotReady) | Ok(Async::Ready(None)) => break,
                Ok(Async::Ready(Some(()))) | Err(()) => continue,
            }
        }
    }
    fn check_connections(&mut self, peers: Vec<Arc<Peer>>) {
        for peer in &peers {
            if self.active.contains(&peer.id) {
                continue;
            }
            if let Some(addr) = peer.primary_addr {
                self.futures.push(
                    Connection::new(&peer.id, addr, &self.shared));
                self.active.insert(peer.id.clone());
            } else {
                self.insert_throttle(peer.id.clone());
            }
        }
    }
    fn dead_connections(&mut self) {
        let dead = mem::replace(&mut self.shared().dead_connections,
                                Vec::new());
        for id in dead {
            self.active.remove(&id);
            self.bump_throttle(id);
        }
    }

    fn insert_throttle(&mut self, id: Id) {
        self.throttled.entry(id)
            .or_insert_with(|| Throttle {
                timestamp: now() - INITIAL_TIME,
                num: 1,
            });
    }
    fn bump_throttle(&mut self, id: Id) {
        use std::collections::hash_map::Entry::*;
        match self.throttled.entry(id) {
            Vacant(e) => { e.insert(Throttle::new()); }
            Occupied(mut e) => { e.get_mut().bump(); }
        }
    }
    fn new_connections(&mut self, gossip: &Gossip) -> bool {
        let mut new = false;
        let mut deadline = now() + Duration::from_secs(86400);
        let mut drop_peers = Vec::new();
        for (id, throttle) in &mut self.throttled {
            if self.active.contains(&id) {
                continue;
            }
            if throttle.timestamp < now() {
                if let Some(peer) = gossip.get_peer(id) {
                    if let Some(addr) = peer.primary_addr {
                        self.futures.push(
                            Connection::new(&id, addr, &self.shared));
                        self.active.insert(id.clone());
                        new = true;
                    } else {
                        throttle.bump();
                        deadline = min(deadline, throttle.timestamp);
                    }
                } else {
                    drop_peers.push(id.clone());
                }
            } else {
                deadline = min(deadline, throttle.timestamp);
            }
        }
        for drop in &drop_peers {
            self.throttled.remove(drop);
            assert!(!self.active.contains(drop));
        }
        self.timer.reset(deadline);
        return new;
    }
}

impl Throttle {
    fn new() -> Throttle {
        Throttle {
            timestamp: now() + INITIAL_TIME,
            num: 1,
        }
    }
    fn bump(&mut self) {
        self.num += 1;
        self.timestamp = now() + min(INITIAL_TIME * self.num, MAX_TIME);
    }
}
