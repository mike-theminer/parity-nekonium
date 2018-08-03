// Copyright 2015-2017 Parity Technologies (UK) Ltd.
// This file is part of Parity.

// Parity is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Parity is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Parity.  If not, see <http://www.gnu.org/licenses/>.

use ethcore_bytes::Bytes;
use std::net::SocketAddr;
use std::collections::{HashSet, HashMap, BTreeMap, VecDeque};
use std::mem;
use std::default::Default;
use mio::*;
use mio::deprecated::{Handler, EventLoop};
use mio::udp::*;
use hash::keccak;
use time;
use ethereum_types::{H256, H520};
use rlp::*;
use node_table::*;
use error::{Error, ErrorKind};
use io::{StreamToken, IoContext};
use ethkey::{Secret, KeyPair, sign, recover};
use IpFilter;

use PROTOCOL_VERSION;

const ADDRESS_BYTES_SIZE: u32 = 32;							// Size of address type in bytes.
const ADDRESS_BITS: u32 = 8 * ADDRESS_BYTES_SIZE;			// Denoted by n in [Kademlia].
const NODE_BINS: u32 = ADDRESS_BITS - 1;					// Size of m_state (excludes root, which is us).
const DISCOVERY_MAX_STEPS: u16 = 8;							// Max iterations of discovery. (discover)
const BUCKET_SIZE: usize = 16;		// Denoted by k in [Kademlia]. Number of nodes stored in each bucket.
const ALPHA: usize = 3;				// Denoted by \alpha in [Kademlia]. Number of concurrent FindNode requests.
const MAX_DATAGRAM_SIZE: usize = 1280;

const PACKET_PING: u8 = 1;
const PACKET_PONG: u8 = 2;
const PACKET_FIND_NODE: u8 = 3;
const PACKET_NEIGHBOURS: u8 = 4;

const PING_TIMEOUT_MS: u64 = 300;
const MAX_NODES_PING: usize = 32; // Max nodes to add/ping at once

#[derive(Clone, Debug)]
pub struct NodeEntry {
	pub id: NodeId,
	pub endpoint: NodeEndpoint,
}

pub struct BucketEntry {
	pub address: NodeEntry,
	pub id_hash: H256,
	pub timeout: Option<u64>,
}

pub struct NodeBucket {
	nodes: VecDeque<BucketEntry>, //sorted by last active
}

impl Default for NodeBucket {
	fn default() -> Self {
		NodeBucket::new()
	}
}

impl NodeBucket {
	fn new() -> Self {
		NodeBucket {
			nodes: VecDeque::new()
		}
	}
}

struct Datagramm {
	payload: Bytes,
	address: SocketAddr,
}

pub struct Discovery {
	id: NodeId,
	id_hash: H256,
	secret: Secret,
	public_endpoint: NodeEndpoint,
	udp_socket: UdpSocket,
	token: StreamToken,
	discovery_round: u16,
	discovery_id: NodeId,
	discovery_nodes: HashSet<NodeId>,
	node_buckets: Vec<NodeBucket>,
	send_queue: VecDeque<Datagramm>,
	check_timestamps: bool,
	adding_nodes: Vec<NodeEntry>,
	ip_filter: IpFilter,
}

pub struct TableUpdates {
	pub added: HashMap<NodeId, NodeEntry>,
	pub removed: HashSet<NodeId>,
}

impl Discovery {
	pub fn new(key: &KeyPair, listen: SocketAddr, public: NodeEndpoint, token: StreamToken, ip_filter: IpFilter) -> Discovery {
		let socket = UdpSocket::bind(&listen).expect("Error binding UDP socket");
		Discovery {
			id: key.public().clone(),
			id_hash: keccak(key.public()),
			secret: key.secret().clone(),
			public_endpoint: public,
			token: token,
			discovery_round: 0,
			discovery_id: NodeId::new(),
			discovery_nodes: HashSet::new(),
			node_buckets: (0..NODE_BINS).map(|_| NodeBucket::new()).collect(),
			udp_socket: socket,
			send_queue: VecDeque::new(),
			check_timestamps: true,
			adding_nodes: Vec::new(),
			ip_filter: ip_filter,
		}
	}

	/// Add a new node to discovery table. Pings the node.
	pub fn add_node(&mut self, e: NodeEntry) {
		if self.is_allowed(&e) {
			let endpoint = e.endpoint.clone();
			self.update_node(e);
			self.ping(&endpoint);
		}
	}

	/// Add a list of nodes. Pings a few nodes each round
	pub fn add_node_list(&mut self, nodes: Vec<NodeEntry>) {
		self.adding_nodes = nodes;
		self.update_new_nodes();
	}

	/// Add a list of known nodes to the table.
	pub fn init_node_list(&mut self, mut nodes: Vec<NodeEntry>) {
		for n in nodes.drain(..) {
			if self.is_allowed(&n) {
				self.update_node(n);
			}
		}
	}

	fn update_node(&mut self, e: NodeEntry) {
		trace!(target: "discovery", "Inserting {:?}", &e);
		let id_hash = keccak(e.id);
		let ping = {
			let bucket = &mut self.node_buckets[Discovery::distance(&self.id_hash, &id_hash) as usize];
			let updated = if let Some(node) = bucket.nodes.iter_mut().find(|n| n.address.id == e.id) {
				node.address = e.clone();
				node.timeout = None;
				true
			} else { false };

			if !updated {
				bucket.nodes.push_front(BucketEntry { address: e, timeout: None, id_hash: id_hash, });
			}

			if bucket.nodes.len() > BUCKET_SIZE {
				//ping least active node
				let last = bucket.nodes.back_mut().expect("Last item is always present when len() > 0");
				last.timeout = Some(time::precise_time_ns());
				Some(last.address.endpoint.clone())
			} else { None }
		};
		if let Some(endpoint) = ping {
			self.ping(&endpoint);
		}
	}

	fn clear_ping(&mut self, id: &NodeId) {
		let bucket = &mut self.node_buckets[Discovery::distance(&self.id_hash, &keccak(id)) as usize];
		if let Some(node) = bucket.nodes.iter_mut().find(|n| &n.address.id == id) {
			node.timeout = None;
		}
	}

	fn start(&mut self) {
		trace!(target: "discovery", "Starting discovery");
		self.discovery_round = 0;
		self.discovery_id.randomize(); //TODO: use cryptographic nonce
		self.discovery_nodes.clear();
	}

	fn update_new_nodes(&mut self) {
		let mut count = 0usize;
		while !self.adding_nodes.is_empty() && count < MAX_NODES_PING {
			let node = self.adding_nodes.pop().expect("pop is always Some if not empty; qed");
			self.add_node(node);
			count += 1;
		}
	}

	fn discover(&mut self) {
		self.update_new_nodes();
		if self.discovery_round == DISCOVERY_MAX_STEPS {
			return;
		}
		trace!(target: "discovery", "Starting round {:?}", self.discovery_round);
		let mut tried_count = 0;
		{
			let nearest = Discovery::nearest_node_entries(&self.discovery_id, &self.node_buckets).into_iter();
			let nearest = nearest.filter(|x| !self.discovery_nodes.contains(&x.id)).take(ALPHA).collect::<Vec<_>>();
			for r in nearest {
				let rlp = encode_list(&(&[self.discovery_id.clone()][..]));
				self.send_packet(PACKET_FIND_NODE, &r.endpoint.udp_address(), &rlp);
				self.discovery_nodes.insert(r.id.clone());
				tried_count += 1;
				trace!(target: "discovery", "Sent FindNode to {:?}", &r.endpoint);
			}
		}

		if tried_count == 0 {
			trace!(target: "discovery", "Completing discovery");
			self.discovery_round = DISCOVERY_MAX_STEPS;
			self.discovery_nodes.clear();
			return;
		}
		self.discovery_round += 1;
	}

	fn distance(a: &H256, b: &H256) -> u32 {
		let d = *a ^ *b;
		let mut ret:u32 = 0;
		for i in 0..32 {
			let mut v: u8 = d[i];
			while v != 0 {
				v >>= 1;
				ret += 1;
			}
		}
		ret
	}

	fn ping(&mut self, node: &NodeEndpoint) {
		let mut rlp = RlpStream::new_list(3);
		rlp.append(&PROTOCOL_VERSION);
		self.public_endpoint.to_rlp_list(&mut rlp);
		node.to_rlp_list(&mut rlp);
		trace!(target: "discovery", "Sent Ping to {:?}", &node);
		self.send_packet(PACKET_PING, &node.udp_address(), &rlp.drain());
	}

	fn send_packet(&mut self, packet_id: u8, address: &SocketAddr, payload: &[u8]) {
		let mut rlp = RlpStream::new();
		rlp.append_raw(&[packet_id], 1);
		let source = Rlp::new(payload);
		rlp.begin_list(source.item_count() + 1);
		for i in 0 .. source.item_count() {
			rlp.append_raw(source.at(i).as_raw(), 1);
		}
		let timestamp = time::get_time().sec as u32 + 60;
		rlp.append(&timestamp);

		let bytes = rlp.drain();
		let hash = keccak(bytes.as_ref());
		let signature = match sign(&self.secret, &hash) {
			Ok(s) => s,
			Err(_) => {
				warn!("Error signing UDP packet");
				return;
			}
		};
		let mut packet = Bytes::with_capacity(bytes.len() + 32 + 65);
		packet.extend(hash.iter());
		packet.extend(signature.iter());
		packet.extend(bytes.iter());
		let signed_hash = keccak(&packet[32..]);
		packet[0..32].clone_from_slice(&signed_hash);
		self.send_to(packet, address.clone());
	}

	fn nearest_node_entries(target: &NodeId, buckets: &[NodeBucket]) -> Vec<NodeEntry> {
		let mut found: BTreeMap<u32, Vec<&NodeEntry>> = BTreeMap::new();
		let mut count = 0;
		let target_hash = keccak(target);

		// Sort nodes by distance to target
		for bucket in buckets {
			for node in &bucket.nodes {
				let distance = Discovery::distance(&target_hash, &node.id_hash);
				found.entry(distance).or_insert_with(Vec::new).push(&node.address);
				if count == BUCKET_SIZE {
					// delete the most distant element
					let remove = {
						let (key, last) = found.iter_mut().next_back().expect("Last element is always Some when count > 0");
						last.pop();
						if last.is_empty() { Some(key.clone()) } else { None }
					};
					if let Some(remove) = remove {
						found.remove(&remove);
					}
				}
				else {
					count += 1;
				}
			}
		}

		let mut ret:Vec<NodeEntry> = Vec::new();
		for nodes in found.values() {
			ret.extend(nodes.iter().map(|&n| n.clone()));
		}
		ret
	}

	pub fn writable<Message>(&mut self, io: &IoContext<Message>) where Message: Send + Sync + Clone {
		while let Some(data) = self.send_queue.pop_front() {
			match self.udp_socket.send_to(&data.payload, &data.address) {
				Ok(Some(size)) if size == data.payload.len() => {
				},
				Ok(Some(_)) => {
					warn!("UDP sent incomplete datagramm");
				},
				Ok(None) => {
					self.send_queue.push_front(data);
					return;
				}
				Err(e) => {
					debug!("UDP send error: {:?}, address: {:?}", e, &data.address);
					return;
				}
			}
		}
		io.update_registration(self.token).unwrap_or_else(|e| debug!("Error updating discovery registration: {:?}", e));
	}

	fn send_to(&mut self, payload: Bytes, address: SocketAddr) {
		self.send_queue.push_back(Datagramm { payload: payload, address: address });
	}

	pub fn readable<Message>(&mut self, io: &IoContext<Message>) -> Option<TableUpdates> where Message: Send + Sync + Clone {
		let mut buf: [u8; MAX_DATAGRAM_SIZE] = unsafe { mem::uninitialized() };
		let writable = !self.send_queue.is_empty();
		let res = match self.udp_socket.recv_from(&mut buf) {
			Ok(Some((len, address))) => self.on_packet(&buf[0..len], address).unwrap_or_else(|e| {
				debug!("Error processing UDP packet: {:?}", e);
				None
			}),
			Ok(_) => None,
			Err(e) => {
				debug!("Error reading UPD socket: {:?}", e);
				None
			}
		};
		let new_writable = !self.send_queue.is_empty();
		if writable != new_writable {
			io.update_registration(self.token).unwrap_or_else(|e| debug!("Error updating discovery registration: {:?}", e));
		}
		res
	}

	fn on_packet(&mut self, packet: &[u8], from: SocketAddr) -> Result<Option<TableUpdates>, Error> {
		// validate packet
		if packet.len() < 32 + 65 + 4 + 1 {
			return Err(ErrorKind::BadProtocol.into());
		}

		let hash_signed = keccak(&packet[32..]);
		if hash_signed[..] != packet[0..32] {
			return Err(ErrorKind::BadProtocol.into());
		}

		let signed = &packet[(32 + 65)..];
		let signature = H520::from_slice(&packet[32..(32 + 65)]);
		let node_id = recover(&signature.into(), &keccak(signed))?;

		let packet_id = signed[0];
		let rlp = UntrustedRlp::new(&signed[1..]);
		match packet_id {
			PACKET_PING => self.on_ping(&rlp, &node_id, &from),
			PACKET_PONG => self.on_pong(&rlp, &node_id, &from),
			PACKET_FIND_NODE => self.on_find_node(&rlp, &node_id, &from),
			PACKET_NEIGHBOURS => self.on_neighbours(&rlp, &node_id, &from),
			_ => {
				debug!("Unknown UDP packet: {}", packet_id);
				Ok(None)
			}
		}
	}

	fn check_timestamp(&self, timestamp: u64) -> Result<(), Error> {
		if self.check_timestamps && timestamp < time::get_time().sec as u64{
			debug!(target: "discovery", "Expired packet");
			return Err(ErrorKind::Expired.into());
		}
		Ok(())
	}

	fn is_allowed(&self, entry: &NodeEntry) -> bool {
		entry.endpoint.is_allowed(&self.ip_filter) && entry.id != self.id
	}

	fn on_ping(&mut self, rlp: &UntrustedRlp, node: &NodeId, from: &SocketAddr) -> Result<Option<TableUpdates>, Error> {
		trace!(target: "discovery", "Got Ping from {:?}", &from);
		let source = NodeEndpoint::from_rlp(&rlp.at(1)?)?;
		let dest = NodeEndpoint::from_rlp(&rlp.at(2)?)?;
		let timestamp: u64 = rlp.val_at(3)?;
		self.check_timestamp(timestamp)?;
		let mut added_map = HashMap::new();
		let entry = NodeEntry { id: node.clone(), endpoint: source.clone() };
		if !entry.endpoint.is_valid() {
			debug!(target: "discovery", "Got bad address: {:?}", entry);
		} else if !self.is_allowed(&entry) {
			debug!(target: "discovery", "Address not allowed: {:?}", entry);
		} else {
			self.update_node(entry.clone());
			added_map.insert(node.clone(), entry);
		}
		let hash = keccak(rlp.as_raw());
		let mut response = RlpStream::new_list(2);
		dest.to_rlp_list(&mut response);
		response.append(&hash);
		self.send_packet(PACKET_PONG, from, &response.drain());

		Ok(Some(TableUpdates { added: added_map, removed: HashSet::new() }))
	}

	fn on_pong(&mut self, rlp: &UntrustedRlp, node: &NodeId, from: &SocketAddr) -> Result<Option<TableUpdates>, Error> {
		trace!(target: "discovery", "Got Pong from {:?}", &from);
		// TODO: validate pong packet
		let dest = NodeEndpoint::from_rlp(&rlp.at(0)?)?;
		let timestamp: u64 = rlp.val_at(2)?;
		self.check_timestamp(timestamp)?;
		let mut entry = NodeEntry { id: node.clone(), endpoint: dest };
		if !entry.endpoint.is_valid() {
			debug!(target: "discovery", "Bad address: {:?}", entry);
			entry.endpoint.address = from.clone();
		}
		self.clear_ping(node);
		let mut added_map = HashMap::new();
		added_map.insert(node.clone(), entry);
		Ok(None)
	}

	fn on_find_node(&mut self, rlp: &UntrustedRlp, _node: &NodeId, from: &SocketAddr) -> Result<Option<TableUpdates>, Error> {
		trace!(target: "discovery", "Got FindNode from {:?}", &from);
		let target: NodeId = rlp.val_at(0)?;
		let timestamp: u64 = rlp.val_at(1)?;
		self.check_timestamp(timestamp)?;
		let nearest = Discovery::nearest_node_entries(&target, &self.node_buckets);
		if nearest.is_empty() {
			return Ok(None);
		}
		let mut packets = Discovery::prepare_neighbours_packets(&nearest);
		for p in packets.drain(..) {
			self.send_packet(PACKET_NEIGHBOURS, from, &p);
		}
		trace!(target: "discovery", "Sent {} Neighbours to {:?}", nearest.len(), &from);
		Ok(None)
	}

	fn prepare_neighbours_packets(nearest: &[NodeEntry]) -> Vec<Bytes> {
		let limit = (MAX_DATAGRAM_SIZE - 109) / 90;
		let chunks = nearest.chunks(limit);
		let packets = chunks.map(|c| {
			let mut rlp = RlpStream::new_list(1);
			rlp.begin_list(c.len());
			for n in 0 .. c.len() {
				rlp.begin_list(4);
				c[n].endpoint.to_rlp(&mut rlp);
				rlp.append(&c[n].id);
			}
			rlp.out()
		});
		packets.collect()
	}

	fn on_neighbours(&mut self, rlp: &UntrustedRlp, _node: &NodeId, from: &SocketAddr) -> Result<Option<TableUpdates>, Error> {
		// TODO: validate packet
		let mut added = HashMap::new();
		trace!(target: "discovery", "Got {} Neighbours from {:?}", rlp.at(0)?.item_count()?, &from);
		for r in rlp.at(0)?.iter() {
			let endpoint = NodeEndpoint::from_rlp(&r)?;
			if !endpoint.is_valid() {
				debug!(target: "discovery", "Bad address: {:?}", endpoint);
				continue;
			}
			let node_id: NodeId = r.val_at(3)?;
			if node_id == self.id {
				continue;
			}
			let entry = NodeEntry { id: node_id.clone(), endpoint: endpoint };
			if !self.is_allowed(&entry) {
				debug!(target: "discovery", "Address not allowed: {:?}", entry);
				continue;
			}
			added.insert(node_id, entry.clone());
			self.ping(&entry.endpoint);
			self.update_node(entry);
		}
		Ok(Some(TableUpdates { added: added, removed: HashSet::new() }))
	}

	fn check_expired(&mut self, force: bool) -> HashSet<NodeId> {
		let now = time::precise_time_ns();
		let mut removed: HashSet<NodeId> = HashSet::new();
		for bucket in &mut self.node_buckets {
			bucket.nodes.retain(|node| {
				if let Some(timeout) = node.timeout {
					if !force && now - timeout < PING_TIMEOUT_MS * 1000_0000 {
						true
					}
					else {
						trace!(target: "discovery", "Removed expired node {:?}", &node.address);
						removed.insert(node.address.id.clone());
						false
					}
				} else { true }
			});
		}
		removed
	}

	pub fn round(&mut self) -> Option<TableUpdates> {
		let removed = self.check_expired(false);
		self.discover();
		if !removed.is_empty() {
			Some(TableUpdates { added: HashMap::new(), removed: removed })
		} else { None }
	}

	pub fn refresh(&mut self) {
		self.start();
	}

	pub fn register_socket<Host:Handler>(&self, event_loop: &mut EventLoop<Host>) -> Result<(), Error> {
		event_loop.register(&self.udp_socket, Token(self.token), Ready::all(), PollOpt::edge()).expect("Error registering UDP socket");
		Ok(())
	}

	pub fn update_registration<Host:Handler>(&self, event_loop: &mut EventLoop<Host>) -> Result<(), Error> {
		let registration = if !self.send_queue.is_empty() {
			Ready::readable() | Ready::writable()
		} else {
			Ready::readable()
		};
		event_loop.reregister(&self.udp_socket, Token(self.token), registration, PollOpt::edge()).expect("Error reregistering UDP socket");
		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::net::{SocketAddr};
	use node_table::{Node, NodeId, NodeEndpoint};

	use std::str::FromStr;
	use rustc_hex::FromHex;
	use ethkey::{Random, Generator};

	#[test]
	fn find_node() {
		let mut nearest = Vec::new();
		let node = Node::from_str("enode://a979fb575495b8d6db44f750317d0f4622bf4c2aa3365d6af7c284339968eef29b69ad0dce72a4d8db5ebb4968de0e3bec910127f134779fbcb0cb6d3331163c@127.0.0.1:7770").unwrap();
		for _ in 0..1000 {
			nearest.push( NodeEntry { id: node.id.clone(), endpoint: node.endpoint.clone() });
		}

		let packets = Discovery::prepare_neighbours_packets(&nearest);
		assert_eq!(packets.len(), 77);
		for p in &packets[0..76] {
			assert!(p.len() > 1280/2);
			assert!(p.len() <= 1280);
		}
		assert!(packets.last().unwrap().len() > 0);
	}

	#[test]
	fn discovery() {
		let key1 = Random.generate().unwrap();
		let key2 = Random.generate().unwrap();
		let ep1 = NodeEndpoint { address: SocketAddr::from_str("127.0.0.1:40444").unwrap(), udp_port: 40444 };
		let ep2 = NodeEndpoint { address: SocketAddr::from_str("127.0.0.1:40445").unwrap(), udp_port: 40445 };
		let mut discovery1 = Discovery::new(&key1, ep1.address.clone(), ep1.clone(), 0, IpFilter::default());
		let mut discovery2 = Discovery::new(&key2, ep2.address.clone(), ep2.clone(), 0, IpFilter::default());

		let node1 = Node::from_str("enode://a979fb575495b8d6db44f750317d0f4622bf4c2aa3365d6af7c284339968eef29b69ad0dce72a4d8db5ebb4968de0e3bec910127f134779fbcb0cb6d3331163c@127.0.0.1:7770").unwrap();
		let node2 = Node::from_str("enode://b979fb575495b8d6db44f750317d0f4622bf4c2aa3365d6af7c284339968eef29b69ad0dce72a4d8db5ebb4968de0e3bec910127f134779fbcb0cb6d3331163c@127.0.0.1:7771").unwrap();
		discovery1.add_node(NodeEntry { id: node1.id.clone(), endpoint: node1.endpoint.clone() });
		discovery1.add_node(NodeEntry { id: node2.id.clone(), endpoint: node2.endpoint.clone() });

		discovery2.add_node(NodeEntry { id: key1.public().clone(), endpoint: ep1.clone() });
		discovery2.refresh();

		for _ in 0 .. 10 {
			while !discovery1.send_queue.is_empty() {
				let datagramm = discovery1.send_queue.pop_front().unwrap();
				if datagramm.address == ep2.address {
					discovery2.on_packet(&datagramm.payload, ep1.address.clone()).ok();
				}
			}
			while !discovery2.send_queue.is_empty() {
				let datagramm = discovery2.send_queue.pop_front().unwrap();
				if datagramm.address == ep1.address {
					discovery1.on_packet(&datagramm.payload, ep2.address.clone()).ok();
				}
			}
			discovery2.round();
		}
		assert_eq!(Discovery::nearest_node_entries(&NodeId::new(), &discovery2.node_buckets).len(), 3)
	}

	#[test]
	fn removes_expired() {
		let key = Random.generate().unwrap();
		let ep = NodeEndpoint { address: SocketAddr::from_str("127.0.0.1:40446").unwrap(), udp_port: 40447 };
		let mut discovery = Discovery::new(&key, ep.address.clone(), ep.clone(), 0, IpFilter::default());
		for _ in 0..1200 {
			discovery.add_node(NodeEntry { id: NodeId::random(), endpoint: ep.clone() });
		}
		assert!(Discovery::nearest_node_entries(&NodeId::new(), &discovery.node_buckets).len() <= 16);
		let removed = discovery.check_expired(true).len();
		assert!(removed > 0);
	}

	#[test]
	fn find_nearest_saturated() {
		use super::*;
		let mut buckets: Vec<_> = (0..256).map(|_| NodeBucket::new()).collect();
		let ep = NodeEndpoint { address: SocketAddr::from_str("127.0.0.1:40447").unwrap(), udp_port: 40447 };
		for _ in 0..(16 + 10) {
			buckets[0].nodes.push_back(BucketEntry {
				address: NodeEntry { id: NodeId::new(), endpoint: ep.clone() },
				timeout: None,
				id_hash: keccak(NodeId::new()),
			});
		}
		let nearest = Discovery::nearest_node_entries(&NodeId::new(), &buckets);
		assert_eq!(nearest.len(), 16)
	}

	#[test]
	fn packets() {
		let key = Random.generate().unwrap();
		let ep = NodeEndpoint { address: SocketAddr::from_str("127.0.0.1:40447").unwrap(), udp_port: 40447 };
		let mut discovery = Discovery::new(&key, ep.address.clone(), ep.clone(), 0, IpFilter::default());
		discovery.check_timestamps = false;
		let from = SocketAddr::from_str("99.99.99.99:40445").unwrap();

		let packet = "\
		e9614ccfd9fc3e74360018522d30e1419a143407ffcce748de3e22116b7e8dc92ff74788c0b6663a\
		aa3d67d641936511c8f8d6ad8698b820a7cf9e1be7155e9a241f556658c55428ec0563514365799a\
		4be2be5a685a80971ddcfa80cb422cdd0101ec04cb847f000001820cfa8215a8d790000000000000\
		000000000000000000018208ae820d058443b9a3550102\
		".from_hex().unwrap();
		assert!(discovery.on_packet(&packet, from.clone()).is_ok());

		let packet = "\
		577be4349c4dd26768081f58de4c6f375a7a22f3f7adda654d1428637412c3d7fe917cadc56d4e5e\
		7ffae1dbe3efffb9849feb71b262de37977e7c7a44e677295680e9e38ab26bee2fcbae207fba3ff3\
		d74069a50b902a82c9903ed37cc993c50001f83e82022bd79020010db83c4d001500000000abcdef\
		12820cfa8215a8d79020010db885a308d313198a2e037073488208ae82823a8443b9a355c5010203\
		040531b9019afde696e582a78fa8d95ea13ce3297d4afb8ba6433e4154caa5ac6431af1b80ba7602\
		3fa4090c408f6b4bc3701562c031041d4702971d102c9ab7fa5eed4cd6bab8f7af956f7d565ee191\
		7084a95398b6a21eac920fe3dd1345ec0a7ef39367ee69ddf092cbfe5b93e5e568ebc491983c09c7\
		6d922dc3\
		".from_hex().unwrap();
		assert!(discovery.on_packet(&packet, from.clone()).is_ok());

		let packet = "\
		09b2428d83348d27cdf7064ad9024f526cebc19e4958f0fdad87c15eb598dd61d08423e0bf66b206\
		9869e1724125f820d851c136684082774f870e614d95a2855d000f05d1648b2d5945470bc187c2d2\
		216fbe870f43ed0909009882e176a46b0102f846d79020010db885a308d313198a2e037073488208\
		ae82823aa0fbc914b16819237dcd8801d7e53f69e9719adecb3cc0e790c57e91ca4461c9548443b9\
		a355c6010203c2040506a0c969a58f6f9095004c0177a6b47f451530cab38966a25cca5cb58f0555
		42124e\
		".from_hex().unwrap();
		assert!(discovery.on_packet(&packet, from.clone()).is_ok());

		let packet = "\
		c7c44041b9f7c7e41934417ebac9a8e1a4c6298f74553f2fcfdcae6ed6fe53163eb3d2b52e39fe91\
		831b8a927bf4fc222c3902202027e5e9eb812195f95d20061ef5cd31d502e47ecb61183f74a504fe\
		04c51e73df81f25c4d506b26db4517490103f84eb840ca634cae0d49acb401d8a4c6b6fe8c55b70d\
		115bf400769cc1400f3258cd31387574077f301b421bc84df7266c44e9e6d569fc56be0081290476\
		7bf5ccd1fc7f8443b9a35582999983999999280dc62cc8255c73471e0a61da0c89acdc0e035e260a\
		dd7fc0c04ad9ebf3919644c91cb247affc82b69bd2ca235c71eab8e49737c937a2c396\
		".from_hex().unwrap();
		assert!(discovery.on_packet(&packet, from.clone()).is_ok());

		let packet = "\
		c679fc8fe0b8b12f06577f2e802d34f6fa257e6137a995f6f4cbfc9ee50ed3710faf6e66f932c4c8\
		d81d64343f429651328758b47d3dbc02c4042f0fff6946a50f4a49037a72bb550f3a7872363a83e1\
		b9ee6469856c24eb4ef80b7535bcf99c0004f9015bf90150f84d846321163782115c82115db84031\
		55e1427f85f10a5c9a7755877748041af1bcd8d474ec065eb33df57a97babf54bfd2103575fa8291\
		15d224c523596b401065a97f74010610fce76382c0bf32f84984010203040101b840312c55512422\
		cf9b8a4097e9a6ad79402e87a15ae909a4bfefa22398f03d20951933beea1e4dfa6f968212385e82\
		9f04c2d314fc2d4e255e0d3bc08792b069dbf8599020010db83c4d001500000000abcdef12820d05\
		820d05b84038643200b172dcfef857492156971f0e6aa2c538d8b74010f8e140811d53b98c765dd2\
		d96126051913f44582e8c199ad7c6d6819e9a56483f637feaac9448aacf8599020010db885a308d3\
		13198a2e037073488203e78203e8b8408dcab8618c3253b558d459da53bd8fa68935a719aff8b811\
		197101a4b2b47dd2d47295286fc00cc081bb542d760717d1bdd6bec2c37cd72eca367d6dd3b9df73\
		8443b9a355010203b525a138aa34383fec3d2719a0\
		".from_hex().unwrap();
		assert!(discovery.on_packet(&packet, from.clone()).is_ok());
	}

}
