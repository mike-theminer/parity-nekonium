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

//! Smart contract based node filter.

extern crate ethabi;
extern crate ethcore;
extern crate ethcore_bytes as bytes;
extern crate ethcore_network as network;
extern crate ethereum_types;
extern crate lru_cache;
extern crate parking_lot;

#[macro_use]
extern crate ethabi_derive;
#[macro_use]
extern crate ethabi_contract;
#[cfg(test)]
extern crate ethcore_io as io;
#[cfg(test)]
extern crate kvdb_memorydb;
#[macro_use]
extern crate log;

use std::sync::Weak;

use lru_cache::LruCache;
use parking_lot::Mutex;

use bytes::Bytes;
use ethcore::client::{BlockChainClient, BlockId, ChainNotify};
use ethereum_types::{H256, Address};
use network::{NodeId, ConnectionFilter, ConnectionDirection};

use_contract!(peer_set, "PeerSet", "res/peer_set.json");

const MAX_CACHE_SIZE: usize = 4096;

/// Connection filter that uses a contract to manage permissions.
pub struct NodeFilter {
	contract: peer_set::PeerSet,
	client: Weak<BlockChainClient>,
	contract_address: Address,
	permission_cache: Mutex<LruCache<NodeId, bool>>,
}

impl NodeFilter {
	/// Create a new instance. Accepts a contract address.
	pub fn new(client: Weak<BlockChainClient>, contract_address: Address) -> NodeFilter {
		NodeFilter {
			contract: peer_set::PeerSet::default(),
			client: client,
			contract_address: contract_address,
			permission_cache: Mutex::new(LruCache::new(MAX_CACHE_SIZE)),
		}
	}

	/// Clear cached permissions.
	pub fn clear_cache(&self) {
		self.permission_cache.lock().clear();
	}
}

impl ConnectionFilter for NodeFilter {
	fn connection_allowed(&self, own_id: &NodeId, connecting_id: &NodeId, _direction: ConnectionDirection) -> bool {

		let mut cache = self.permission_cache.lock();
		if let Some(res) = cache.get_mut(connecting_id) {
			return *res;
		}

		let client = match self.client.upgrade() {
			Some(client) => client,
			None => return false,
		};

		let address = self.contract_address;
		let own_low = H256::from_slice(&own_id[0..32]);
		let own_high = H256::from_slice(&own_id[32..64]);
		let id_low = H256::from_slice(&connecting_id[0..32]);
		let id_high = H256::from_slice(&connecting_id[32..64]);

		let allowed = self.contract.functions()
			.connection_allowed()
			.call(own_low, own_high, id_low, id_high, &|data| client.call_contract(BlockId::Latest, address, data))
			.unwrap_or_else(|e| {
				debug!("Error callling peer set contract: {:?}", e);
				false
			});

		cache.insert(*connecting_id, allowed);
		allowed
	}
}

impl ChainNotify for NodeFilter {
	fn new_blocks(&self, imported: Vec<H256>, _invalid: Vec<H256>, _enacted: Vec<H256>, _retracted: Vec<H256>, _sealed: Vec<H256>, _proposed: Vec<Bytes>, _duration: u64) {
		if !imported.is_empty() {
			self.clear_cache();
		}
	}
}


#[cfg(test)]
mod test {
	use std::sync::{Arc, Weak};
	use std::str::FromStr;
	use ethcore::spec::Spec;
	use ethcore::client::{BlockChainClient, Client, ClientConfig};
	use ethcore::miner::Miner;
	use ethereum_types::Address;
	use network::{ConnectionDirection, ConnectionFilter, NodeId};
	use io::IoChannel;
	use super::NodeFilter;

	/// Contract code: https://gist.github.com/arkpar/467dbcc73cbb85b0997a7a10ffa0695f
	#[test]
	fn node_filter() {
		let contract_addr = Address::from_str("0000000000000000000000000000000000000005").unwrap();
		let data = include_bytes!("../res/node_filter.json");
		let spec = Spec::load(&::std::env::temp_dir(), &data[..]).unwrap();
		let client_db = Arc::new(::kvdb_memorydb::create(::ethcore::db::NUM_COLUMNS.unwrap_or(0)));

		let client = Client::new(
			ClientConfig::default(),
			&spec,
			client_db,
			Arc::new(Miner::with_spec(&spec)),
			IoChannel::disconnected(),
		).unwrap();
		let filter = NodeFilter::new(Arc::downgrade(&client) as Weak<BlockChainClient>, contract_addr);
		let self1 = NodeId::from_str("00000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000002").unwrap();
		let self2 = NodeId::from_str("00000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000000000000000003").unwrap();
		let node1 = NodeId::from_str("00000000000000000000000000000000000000000000000000000000000000110000000000000000000000000000000000000000000000000000000000000012").unwrap();
		let node2 = NodeId::from_str("00000000000000000000000000000000000000000000000000000000000000210000000000000000000000000000000000000000000000000000000000000022").unwrap();
		let nodex = NodeId::from_str("77000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000").unwrap();

		assert!(filter.connection_allowed(&self1, &node1, ConnectionDirection::Inbound));
		assert!(filter.connection_allowed(&self1, &nodex, ConnectionDirection::Inbound));
		filter.clear_cache();
		assert!(filter.connection_allowed(&self2, &node1, ConnectionDirection::Inbound));
		assert!(filter.connection_allowed(&self2, &node2, ConnectionDirection::Inbound));
		assert!(!filter.connection_allowed(&self2, &nodex, ConnectionDirection::Inbound));
	}
}
