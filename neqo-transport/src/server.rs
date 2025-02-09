// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

// This file implements a server that can handle multiple connections.

use neqo_common::{hex, matches, qinfo, qtrace, qwarn, timer::Timer, Datagram, Decoder};
use neqo_crypto::AntiReplay;

use crate::connection::{Connection, ConnectionIdManager, Output, State};
use crate::packet::{
    decode_packet_hdr, encode_packet_vn, encode_retry, ConnectionId, ConnectionIdDecoder,
    PacketHdr, PacketType, Version,
};
use crate::QUIC_VERSION;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::mem;
use std::ops::{Deref, DerefMut};
use std::rc::Rc;
use std::time::{Duration, Instant};

pub enum InitialResult {
    Accept,
    Drop,
    Retry(Vec<u8>),
}

/// MIN_INITIAL_PACKET_SIZE is the smallest packet that can be used to establish
/// a new connection across all QUIC versions this server supports.
const MIN_INITIAL_PACKET_SIZE: usize = 1200;
const TIMER_GRANULARITY: Duration = Duration::from_millis(10);
const TIMER_CAPACITY: usize = 16384;
const FIXED_TOKEN: &[u8] = &[1, 2, 3];

type StateRef = Rc<RefCell<ServerConnectionState>>;
type CidMgr = Rc<RefCell<dyn ConnectionIdManager>>;
type ConnectionTableRef = Rc<RefCell<HashMap<ConnectionId, StateRef>>>;

#[derive(Debug)]
struct ServerConnectionState {
    c: Connection,
    last_timer: Instant,
}

impl Deref for ServerConnectionState {
    type Target = Connection;
    fn deref(&self) -> &Self::Target {
        &self.c
    }
}

impl DerefMut for ServerConnectionState {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.c
    }
}

enum RetryTokenResult {
    Pass,
    Valid(ConnectionId),
    Validate,
    Invalid,
}

// TODO(mt) self-encrypt tokens
#[derive(Default)]
struct RetryToken {
    require_retry: bool,
}

impl RetryToken {
    pub fn generate_token(&mut self, dcid: &ConnectionId) -> Vec<u8> {
        let mut token = Vec::from(FIXED_TOKEN);
        token.extend_from_slice(dcid);
        token
    }

    pub fn set_retry_required(&mut self, retry: bool) {
        self.require_retry = retry;
    }

    pub fn validate(&self, hdr: &PacketHdr) -> RetryTokenResult {
        if let PacketType::Initial(token) = &hdr.tipe {
            if token.is_empty() {
                if self.require_retry {
                    RetryTokenResult::Validate
                } else {
                    RetryTokenResult::Pass
                }
            } else if token[0..FIXED_TOKEN.len()] == FIXED_TOKEN[..] {
                let cid = ConnectionId::from(&token[FIXED_TOKEN.len()..]);
                RetryTokenResult::Valid(cid)
            } else {
                RetryTokenResult::Invalid
            }
        } else {
            RetryTokenResult::Invalid
        }
    }
}

pub struct Server {
    /// The version this server supports (currently just one).
    version: Version,
    /// The names of certificates.
    certs: Vec<String>,
    /// The ALPN values that the server supports.
    protocols: Vec<String>,
    anti_replay: AntiReplay,
    /// A connection ID manager.
    cid_manager: CidMgr,
    /// All connections, keyed by ConnectionId.
    connections: ConnectionTableRef,
    /// The connections that have new events.
    active: HashSet<ActiveConnectionRef>,
    /// The set of connections that need immediate processing.
    waiting: VecDeque<StateRef>,
    /// Outstanding timers for connections.
    timers: Timer<StateRef>,
    /// Whether a Retry packet will be sent in response to new
    /// Initial packets.
    retry: RetryToken,
}

impl Server {
    /// Construct a new server.
    /// `now` is the time that the server is instantiated.
    /// `cid_manager` is responsible for generating connection IDs and parsing them;
    /// connection IDs produced by the manager cannot be zero-length.
    /// `certs` is a list of the certificates that should be configured.
    /// `protocols` is the preference list of ALPN values.
    /// `anti_replay` is an anti-replay context.
    pub fn new(
        now: Instant,
        certs: &[impl AsRef<str>],
        protocols: &[impl AsRef<str>],
        anti_replay: AntiReplay,
        cid_manager: CidMgr,
    ) -> Server {
        Server {
            version: QUIC_VERSION,
            certs: certs.iter().map(|x| String::from(x.as_ref())).collect(),
            protocols: protocols.iter().map(|x| String::from(x.as_ref())).collect(),
            anti_replay,
            cid_manager,
            connections: Rc::new(RefCell::new(Default::default())),
            active: Default::default(),
            waiting: Default::default(),
            timers: Timer::new(now, TIMER_GRANULARITY, TIMER_CAPACITY),
            retry: Default::default(),
        }
    }

    fn create_vn(&self, hdr: &PacketHdr, received: Datagram) -> Datagram {
        let vn = encode_packet_vn(&PacketHdr::new(
            0,
            // Actual version we support and a greased value.
            PacketType::VN(vec![self.version, 0xaaba_cada]),
            Some(0),
            hdr.scid.as_ref().unwrap().clone(),
            Some(hdr.dcid.clone()),
            0, // unused
            0, // unused
        ));
        Datagram::new(received.destination(), received.source(), vn)
    }

    pub fn set_retry_required(&mut self, require_retry: bool) {
        self.retry.set_retry_required(require_retry);
    }

    fn remove_timer(&mut self, c: &StateRef) {
        let last = c.borrow().last_timer;
        self.timers.remove(last, |t| Rc::ptr_eq(t, c));
    }

    fn process_connection(
        &mut self,
        c: StateRef,
        dgram: Option<Datagram>,
        now: Instant,
    ) -> Option<Datagram> {
        qtrace!([self] "Process connection {:?}", c);
        let out = c.borrow_mut().process(dgram, now);
        match out {
            Output::Datagram(_) => {
                qtrace!([self] "Sending packet, added to waiting connections");
                self.waiting.push_back(c.clone());
            }
            Output::Callback(delay) => {
                let next = now + delay;
                if next != c.borrow().last_timer {
                    qtrace!([self] "Change timer to {:?}", next);
                    self.remove_timer(&c);
                    c.borrow_mut().last_timer = next;
                    self.timers.add(next, c.clone());
                }
            }
            _ => {
                self.remove_timer(&c);
            }
        }
        if c.borrow().has_events() {
            qtrace!([self] "Connection active: {:?}", c);
            self.active.insert(ActiveConnectionRef { c: c.clone() });
        }
        if matches!(c.borrow().state(), State::Closed(_)) {
            self.connections
                .borrow_mut()
                .retain(|_, v| !Rc::ptr_eq(v, &c));
        }
        out.dgram()
    }

    fn connection(&self, cid: &ConnectionId) -> Option<StateRef> {
        if let Some(c) = self.connections.borrow().get(cid) {
            Some(c.clone())
        } else {
            None
        }
    }

    fn handle_initial(
        &mut self,
        hdr: PacketHdr,
        dgram: Datagram,
        now: Instant,
    ) -> Option<Datagram> {
        match self.retry.validate(&hdr) {
            RetryTokenResult::Invalid => None,
            RetryTokenResult::Pass => self.accept_connection(None, dgram, now),
            RetryTokenResult::Valid(dcid) => self.accept_connection(Some(dcid), dgram, now),
            RetryTokenResult::Validate => {
                qinfo!([self] "Send retry for {:?}", hdr.dcid);
                let token = self.retry.generate_token(&hdr.dcid);
                let payload = encode_retry(&PacketHdr::new(
                    0, // tbyte (unused on encode)
                    PacketType::Retry {
                        odcid: hdr.dcid.clone(),
                        token,
                    },
                    Some(self.version),
                    hdr.scid.as_ref().unwrap().clone(),
                    Some(self.cid_manager.borrow_mut().generate_cid()),
                    0, // Packet number
                    0, // Epoch
                ));
                let retry = Datagram::new(dgram.destination(), dgram.source(), payload);
                Some(retry)
            }
        }
    }

    fn accept_connection(
        &mut self,
        odcid: Option<ConnectionId>,
        dgram: Datagram,
        now: Instant,
    ) -> Option<Datagram> {
        qinfo!([self] "Accept connection");
        // The internal connection ID manager that we use is not used directly.
        // Instead, wrap it so that we can save connection IDs.
        let cid_mgr = Rc::new(RefCell::new(ServerConnectionIdManager {
            c: None,
            cid_manager: self.cid_manager.clone(),
            connections: self.connections.clone(),
        }));
        let sconn = Connection::new_server(
            &self.certs,
            &self.protocols,
            &self.anti_replay,
            cid_mgr.clone(),
        );
        if let Ok(mut c) = sconn {
            if let Some(odcid) = odcid {
                c.original_connection_id(&odcid);
            }
            let c = Rc::new(RefCell::new(ServerConnectionState { c, last_timer: now }));
            cid_mgr.borrow_mut().c = Some(c.clone());
            self.process_connection(c, Some(dgram), now)
        } else {
            qwarn!([self] "Unable to create connection");
            None
        }
    }

    fn process_input(&mut self, dgram: Datagram, now: Instant) -> Option<Datagram> {
        qtrace!("Process datagram: {}", hex(&dgram[..]));

        // This is only looking at the first packet header in the datagram.
        // All packets in the datagram are routed to the same connection.
        let res = decode_packet_hdr(self.cid_manager.borrow().as_decoder(), &dgram[..]);
        let hdr = match res {
            Ok(h) => h,
            _ => {
                qtrace!([self] "Discarding {:?}", dgram);
                return None;
            }
        };

        // Finding an existing connection. Should be the most common case.
        if let Some(c) = self.connection(&hdr.dcid) {
            return self.process_connection(c, Some(dgram), now);
        }

        if hdr.tipe == PacketType::Short {
            // TODO send a stateless reset here.
            qtrace!([self] "Short header packet for an unknown connection");
            return None;
        }

        if dgram.len() < MIN_INITIAL_PACKET_SIZE {
            qtrace!([self] "Bogus packet");
            return None;
        }

        if hdr.version != Some(self.version) {
            return Some(self.create_vn(&hdr, dgram));
        }

        self.handle_initial(hdr, dgram, now)
    }

    /// Iterate through the pending connections looking for any that might want
    /// to send a datagram.  Stop at the first one that does.
    fn process_next_output(&mut self, now: Instant) -> Option<Datagram> {
        qtrace!([self] "No packet to send, look at waiting connections");
        while let Some(c) = self.waiting.pop_front() {
            if let Some(d) = self.process_connection(c, None, now) {
                return Some(d);
            }
        }
        qtrace!([self] "No packet to send still, run timers");
        while let Some(c) = self.timers.take_next(now) {
            if let Some(d) = self.process_connection(c, None, now) {
                return Some(d);
            }
        }
        None
    }

    fn next_time(&mut self, now: Instant) -> Option<Duration> {
        if self.waiting.is_empty() {
            self.timers.next_time().map(|x| x - now)
        } else {
            Some(Duration::new(0, 0))
        }
    }

    pub fn process(&mut self, dgram: Option<Datagram>, now: Instant) -> Output {
        let out = if let Some(d) = dgram {
            self.process_input(d, now)
        } else {
            None
        };
        let out = out.or_else(|| self.process_next_output(now));
        match out {
            Some(d) => {
                qtrace!([self] "Send packet: {:?}", d);
                Output::Datagram(d)
            }
            _ => match self.next_time(now) {
                Some(delay) => {
                    qtrace!([self] "Wait: {:?}", delay);
                    Output::Callback(delay)
                }
                _ => {
                    qtrace!([self] "Go dormant");
                    Output::None
                }
            },
        }
    }

    /// This lists the connections that have received new events
    /// as a result of calling `process()`.
    pub fn active_connections(&mut self) -> Vec<ActiveConnectionRef> {
        mem::replace(&mut self.active, Default::default())
            .into_iter()
            .collect()
    }
}

#[derive(Clone, Debug)]
pub struct ActiveConnectionRef {
    c: StateRef,
}

impl ActiveConnectionRef {
    pub fn borrow<'a>(&'a self) -> impl Deref<Target = Connection> + 'a {
        std::cell::Ref::map(self.c.borrow(), |c| &c.c)
    }

    pub fn borrow_mut<'a>(&'a mut self) -> impl DerefMut<Target = Connection> + 'a {
        std::cell::RefMut::map(self.c.borrow_mut(), |c| &mut c.c)
    }
}

impl std::hash::Hash for ActiveConnectionRef {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        let ptr: *const _ = self.c.as_ref();
        ptr.hash(state)
    }
}

impl PartialEq for ActiveConnectionRef {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.c, &other.c)
    }
}
impl Eq for ActiveConnectionRef {}

struct ServerConnectionIdManager {
    c: Option<StateRef>,
    connections: ConnectionTableRef,
    cid_manager: CidMgr,
}

impl ConnectionIdDecoder for ServerConnectionIdManager {
    fn decode_cid(&self, dec: &mut Decoder) -> Option<ConnectionId> {
        self.cid_manager.borrow_mut().decode_cid(dec)
    }
}
impl ConnectionIdManager for ServerConnectionIdManager {
    fn generate_cid(&mut self) -> ConnectionId {
        let cid = self.cid_manager.borrow_mut().generate_cid();
        assert!(!cid.is_empty());
        let v = self
            .connections
            .borrow_mut()
            .insert(cid.clone(), self.c.as_ref().unwrap().clone());
        if let Some(v) = v {
            debug_assert!(Rc::ptr_eq(&v, self.c.as_ref().unwrap()));
        }
        cid
    }
    fn as_decoder(&self) -> &dyn ConnectionIdDecoder {
        self
    }
}

impl ::std::fmt::Display for Server {
    fn fmt(&self, f: &mut ::std::fmt::Formatter) -> ::std::fmt::Result {
        write!(f, "Server")
    }
}
