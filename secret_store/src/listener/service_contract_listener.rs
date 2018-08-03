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

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use parking_lot::Mutex;
use ethcore::client::ChainNotify;
use ethkey::{Random, Generator, Public, sign};
use bytes::Bytes;
use ethereum_types::{H256, U256};
use key_server_set::KeyServerSet;
use key_server_cluster::{ClusterClient, ClusterSessionsListener, ClusterSession};
use key_server_cluster::generation_session::SessionImpl as GenerationSession;
use key_storage::KeyStorage;
use listener::service_contract::ServiceContract;
use listener::tasks_queue::TasksQueue;
use {ServerKeyId, NodeKeyPair, KeyServer};

/// Retry interval (in blocks). Every RETRY_INTERVAL_BLOCKS blocks each KeyServer reads pending requests from
/// service contract && tries to re-execute. The reason to have this mechanism is primarily because keys
/// servers set change takes a lot of time + there could be some races, when blocks are coming to different
/// KS at different times. This isn't intended to fix && respond to general session errors!
const RETRY_INTERVAL_BLOCKS: usize = 30;

/// Max failed retry requests (in single retry interval). The reason behind this constant is that if several
/// pending requests have failed, then most probably other will fail too.
const MAX_FAILED_RETRY_REQUESTS: usize = 1;

/// SecretStore <-> Authority connector responsible for:
/// 1. listening for new requests on SecretStore contract
/// 2. redirecting requests to key server
/// 3. publishing response on SecretStore contract
pub struct ServiceContractListener {
	/// Service contract listener data.
	data: Arc<ServiceContractListenerData>,
	/// Service thread handle.
	service_handle: Option<thread::JoinHandle<()>>,
}

/// Service contract listener parameters.
pub struct ServiceContractListenerParams {
	/// Service contract.
	pub contract: Arc<ServiceContract>,
	/// Key server reference.
	pub key_server: Arc<KeyServer>,
	/// This node key pair.
	pub self_key_pair: Arc<NodeKeyPair>,
	/// Key servers set.
	pub key_server_set: Arc<KeyServerSet>,
	/// Cluster reference.
	pub cluster: Arc<ClusterClient>,
	/// Key storage reference.
	pub key_storage: Arc<KeyStorage>,
}

/// Service contract listener data.
struct ServiceContractListenerData {
	/// Blocks since last retry.
	pub last_retry: AtomicUsize,
	/// Retry-related data.
	pub retry_data: Mutex<ServiceContractRetryData>,
	/// Service tasks queue.
	pub tasks_queue: Arc<TasksQueue<ServiceTask>>,
	/// Service contract.
	pub contract: Arc<ServiceContract>,
	/// Key server reference.
	pub key_server: Arc<KeyServer>,
	/// This node key pair.
	pub self_key_pair: Arc<NodeKeyPair>,
	/// Key servers set.
	pub key_server_set: Arc<KeyServerSet>,
	/// Key storage reference.
	pub key_storage: Arc<KeyStorage>,

}

/// Retry-related data.
#[derive(Default)]
struct ServiceContractRetryData {
	/// Server keys, which we have generated (or tried to generate) since last retry moment.
	pub generated_keys: HashSet<ServerKeyId>,
}

/// Service task.
#[derive(Debug, Clone, PartialEq)]
pub enum ServiceTask {
	/// Retry all 'stalled' tasks.
	Retry,
	/// Generate server key (server_key_id, threshold).
	GenerateServerKey(H256, H256),
	/// Confirm server key (server_key_id).
	RestoreServerKey(H256),
	/// Shutdown listener.
	Shutdown,
}

impl ServiceContractListener {
	/// Create new service contract listener.
	pub fn new(params: ServiceContractListenerParams) -> Arc<ServiceContractListener> {
		let data = Arc::new(ServiceContractListenerData {
			last_retry: AtomicUsize::new(0),
			retry_data: Default::default(),
			tasks_queue: Arc::new(TasksQueue::new()),
			contract: params.contract,
			key_server: params.key_server,
			self_key_pair: params.self_key_pair,
			key_server_set: params.key_server_set,
			key_storage: params.key_storage,
		});
		data.tasks_queue.push(ServiceTask::Retry);

		// we are not starting thread when in test mode
		let service_handle = if cfg!(test) {
			None
		} else {
			let service_thread_data = data.clone();
			Some(thread::spawn(move || Self::run_service_thread(service_thread_data)))
		};
		let contract = Arc::new(ServiceContractListener {
			data: data,
			service_handle: service_handle,
		});
		params.cluster.add_generation_listener(contract.clone());
		contract
	}

	/// Process incoming events of service contract.
	fn process_service_contract_events(&self) {
		self.data.tasks_queue.push_many(self.data.contract.read_logs()
			.filter_map(|topics| match topics.len() {
				// when key is already generated && we have this key
				3 if self.data.key_storage.get(&topics[1]).map(|k| k.is_some()).unwrap_or_default() => {
					Some(ServiceTask::RestoreServerKey(
						topics[1],
					))
				}
				// when key is not yet generated && this node should be master of this key generation session
				3 if is_processed_by_this_key_server(&*self.data.key_server_set, &*self.data.self_key_pair, &topics[1]) => {
					Some(ServiceTask::GenerateServerKey(
						topics[1],
						topics[2],
					))
				},
				3 => None,
				l @ _ => {
					warn!(target: "secretstore", "Ignoring ServerKeyRequested event with wrong number of params {}", l);
					None
				},
			}));
	}

	/// Service thread procedure.
	fn run_service_thread(data: Arc<ServiceContractListenerData>) {
		loop {
			let task = data.tasks_queue.wait();
			trace!(target: "secretstore", "{}: processing {:?} task", data.self_key_pair.public(), task);

			match task {
				ServiceTask::Shutdown => break,
				task @ _ => {
					// the only possible reaction to an error is a trace && it is already happened
					let _ = Self::process_service_task(&data, task);
				},
			};
		}
	}

	/// Process single service task.
	fn process_service_task(data: &Arc<ServiceContractListenerData>, task: ServiceTask) -> Result<(), String> {
		match task {
			ServiceTask::Retry =>
				Self::retry_pending_requests(&data)
					.map(|processed_requests| {
						if processed_requests != 0 {
							trace!(target: "secretstore", "{}: successfully retried {} pending requests",
								data.self_key_pair.public(), processed_requests);
						}
						()
					})
					.map_err(|error| {
						warn!(target: "secretstore", "{}: retrying pending requests has failed with: {}",
							data.self_key_pair.public(), error);
						error
					}),
			ServiceTask::RestoreServerKey(server_key_id) => {
				data.retry_data.lock().generated_keys.insert(server_key_id.clone());
				Self::restore_server_key(&data, &server_key_id)
					.and_then(|server_key| Self::publish_server_key(&data, &server_key_id, &server_key))
					.map(|_| {
						trace!(target: "secretstore", "{}: processed RestoreServerKey({}) request",
							data.self_key_pair.public(), server_key_id);
						()
					})
					.map_err(|error| {
						warn!(target: "secretstore", "{}: failed to process RestoreServerKey({}) request with: {}",
							data.self_key_pair.public(), server_key_id, error);
						error
					})
			},
			ServiceTask::GenerateServerKey(server_key_id, threshold) => {
				data.retry_data.lock().generated_keys.insert(server_key_id.clone());
				Self::generate_server_key(&data, &server_key_id, &threshold)
					.and_then(|server_key| Self::publish_server_key(&data, &server_key_id, &server_key))
					.map(|_| {
						trace!(target: "secretstore", "{}: processed GenerateServerKey({}, {}) request",
							data.self_key_pair.public(), server_key_id, threshold);
						()
					})
					.map_err(|error| {
						warn!(target: "secretstore", "{}: failed to process GenerateServerKey({}, {}) request with: {}",
							data.self_key_pair.public(), server_key_id, threshold, error);
						error
					})
			},
			ServiceTask::Shutdown => unreachable!("it must be checked outside"),
		}
	}

	/// Retry processing pending requests.
	fn retry_pending_requests(data: &Arc<ServiceContractListenerData>) -> Result<usize, String> {
		let mut failed_requests = 0;
		let mut processed_requests = 0;
		let retry_data = ::std::mem::replace(&mut *data.retry_data.lock(), Default::default());
		for (is_confirmed, task) in data.contract.read_pending_requests() {
			// only process requests, which we haven't confirmed yet
			if is_confirmed {
				continue;
			}

			let request_result = match task {
				ServiceTask::GenerateServerKey(server_key_id, threshold) => {
					// only process request, which haven't been processed recently
					// there could be a lag when we've just generated server key && retrying on the same block
					// (or before our tx is mined) - state is not updated yet
					if retry_data.generated_keys.contains(&server_key_id) {
						continue;
					}

					// process request
					let is_own_request = is_processed_by_this_key_server(&*data.key_server_set, &*data.self_key_pair, &server_key_id);
					Self::process_service_task(data, match is_own_request {
						true => ServiceTask::GenerateServerKey(server_key_id, threshold.into()),
						false => ServiceTask::RestoreServerKey(server_key_id),
					})
				},
				_ => Err("not supported".into()),
			};

			// process request result
			match request_result {
				Ok(_) => processed_requests += 1,
				Err(_) => {
					failed_requests += 1;
					if failed_requests > MAX_FAILED_RETRY_REQUESTS {
						return Err("too many failed requests".into());
					}
				},
			}
		}

		Ok(processed_requests)
	}

	/// Generate server key.
	fn generate_server_key(data: &Arc<ServiceContractListenerData>, server_key_id: &ServerKeyId, threshold: &H256) -> Result<Public, String> {
		let threshold_num = threshold.low_u64();
		if threshold != &threshold_num.into() || threshold_num >= ::std::usize::MAX as u64 {
			return Err(format!("invalid threshold {:?}", threshold));
		}

		// key server expects signed server_key_id in server_key_generation procedure
		// only signer could store document key for this server key later
		// => this API (server key generation) is not suitable for usage in encryption via contract endpoint
		let author_key = Random.generate().map_err(|e| format!("{}", e))?;
		let server_key_id_signature = sign(author_key.secret(), server_key_id).map_err(|e| format!("{}", e))?;
		data.key_server.generate_key(server_key_id, &server_key_id_signature, threshold_num as usize)
			.map_err(Into::into)
	}

	/// Restore server key.
	fn restore_server_key(data: &Arc<ServiceContractListenerData>, server_key_id: &ServerKeyId) -> Result<Public, String> {
		data.key_storage.get(server_key_id)
			.map_err(|e| format!("{}", e))
			.and_then(|ks| ks.ok_or("missing key".to_owned()))
			.map(|ks| ks.public)
	}

	/// Publish server key.
	fn publish_server_key(data: &Arc<ServiceContractListenerData>, server_key_id: &ServerKeyId, server_key: &Public) -> Result<(), String> {
		data.contract.publish_server_key(server_key_id, server_key)
	}
}

impl Drop for ServiceContractListener {
	fn drop(&mut self) {
		if let Some(service_handle) = self.service_handle.take() {
			self.data.tasks_queue.push_front(ServiceTask::Shutdown);
			// ignore error as we are already closing
			let _ = service_handle.join();
		}
	}
}

impl ChainNotify for ServiceContractListener {
	fn new_blocks(&self, _imported: Vec<H256>, _invalid: Vec<H256>, enacted: Vec<H256>, _retracted: Vec<H256>, _sealed: Vec<H256>, _proposed: Vec<Bytes>, _duration: u64) {
		let enacted_len = enacted.len();
		if enacted_len == 0 {
			return;
		}

		if !self.data.contract.update() {
			return;
		}

		self.process_service_contract_events();

		// schedule retry if received enough blocks since last retry
		// it maybe inaccurate when switching syncing/synced states, but that's ok
		if self.data.last_retry.fetch_add(enacted_len, Ordering::Relaxed) >= RETRY_INTERVAL_BLOCKS {
			self.data.tasks_queue.push(ServiceTask::Retry);
			self.data.last_retry.store(0, Ordering::Relaxed);
		}
	}
}

impl ClusterSessionsListener<GenerationSession> for ServiceContractListener {
	fn on_session_removed(&self, session: Arc<GenerationSession>) {
		// only publish when the session is started by another node
		// when it is started by this node, it is published from process_service_task
		if !is_processed_by_this_key_server(&*self.data.key_server_set, &*self.data.self_key_pair, &session.id()) {
			// by this time sesion must already be completed - either successfully, or not
			assert!(session.is_finished());

			// ignore result - the only thing that we can do is to log the error
			match session.wait(Some(Default::default()))
				.map_err(|e| format!("{}", e))
				.and_then(|server_key| Self::publish_server_key(&self.data, &session.id(), &server_key)) {
				Ok(_) => trace!(target: "secretstore", "{}: completed foreign GenerateServerKey({}) request",
					self.data.self_key_pair.public(), session.id()),
				Err(error) => warn!(target: "secretstore", "{}: failed to process GenerateServerKey({}) request with: {}",
					self.data.self_key_pair.public(), session.id(), error),
			}
		}
	}
}

/// Returns true when session, related to `server_key_id` must be started on this KeyServer.
fn is_processed_by_this_key_server(key_server_set: &KeyServerSet, self_key_pair: &NodeKeyPair, server_key_id: &H256) -> bool {
	let servers = key_server_set.snapshot().current_set;
	let total_servers_count = servers.len();
	match total_servers_count {
		0 => return false,
		1 => return true,
		_ => (),
	}

	let this_server_index = match servers.keys().enumerate().find(|&(_, s)| s == self_key_pair.public()) {
		Some((index, _)) => index,
		None => return false,
	};

	let server_key_id_value: U256 = server_key_id.into();
	let range_interval = U256::max_value() / total_servers_count.into();
	let range_begin = (range_interval + 1.into()) * this_server_index as u32;
	let range_end = range_begin.saturating_add(range_interval);

	server_key_id_value >= range_begin && server_key_id_value <= range_end
}

#[cfg(test)]
mod tests {
	use std::sync::Arc;
	use std::sync::atomic::Ordering;
	use ethkey::{Random, Generator, KeyPair};
	use listener::service_contract::ServiceContract;
	use listener::service_contract::tests::DummyServiceContract;
	use key_server_cluster::DummyClusterClient;
	use key_server::tests::DummyKeyServer;
	use key_storage::{KeyStorage, DocumentKeyShare};
	use key_storage::tests::DummyKeyStorage;
	use key_server_set::tests::MapKeyServerSet;
	use PlainNodeKeyPair;
	use super::{ServiceTask, ServiceContractListener, ServiceContractListenerParams, is_processed_by_this_key_server};

	fn make_service_contract_listener(contract: Option<Arc<ServiceContract>>, key_server: Option<Arc<DummyKeyServer>>, key_storage: Option<Arc<KeyStorage>>) -> Arc<ServiceContractListener> {
		let contract = contract.unwrap_or_else(|| Arc::new(DummyServiceContract::default()));
		let key_server = key_server.unwrap_or_else(|| Arc::new(DummyKeyServer::default()));
		let key_storage = key_storage.unwrap_or_else(|| Arc::new(DummyKeyStorage::default()));
		let servers_set = Arc::new(MapKeyServerSet::new(vec![
			("79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798483ada7726a3c4655da4fbfc0e1108a8fd17b448a68554199c47d08ffb10d4b8".parse().unwrap(),
				"127.0.0.1:8080".parse().unwrap()),
			("c6047f9441ed7d6d3045406e95c07cd85c778e4b8cef3ca7abac09b95c709ee51ae168fea63dc339a3c58419466ceaeef7f632653266d0e1236431a950cfe52a".parse().unwrap(),
				"127.0.0.1:8080".parse().unwrap()),
			("f9308a019258c31049344f85f89d5229b531c845836f99b08601f113bce036f9388f7b0f632de8140fe337e62a37f3566500a99934c2231b6cb9fd7584b8e672".parse().unwrap(),
				"127.0.0.1:8080".parse().unwrap()),
		].into_iter().collect()));
		let self_key_pair = Arc::new(PlainNodeKeyPair::new(KeyPair::from_secret("0000000000000000000000000000000000000000000000000000000000000001".parse().unwrap()).unwrap()));
		ServiceContractListener::new(ServiceContractListenerParams {
			contract: contract,
			key_server: key_server,
			self_key_pair: self_key_pair,
			key_server_set: servers_set,
			cluster: Arc::new(DummyClusterClient::default()),
			key_storage: key_storage,
		})
	}

	#[test]
	fn is_not_processed_by_this_key_server_with_zero_servers() {
		assert_eq!(is_processed_by_this_key_server(
			&MapKeyServerSet::default(),
			&PlainNodeKeyPair::new(Random.generate().unwrap()),
			&Default::default()), false);
	}

	#[test]
	fn is_processed_by_this_key_server_with_single_server() {
		let self_key_pair = Random.generate().unwrap();
		assert_eq!(is_processed_by_this_key_server(
			&MapKeyServerSet::new(vec![
				(self_key_pair.public().clone(), "127.0.0.1:8080".parse().unwrap())
			].into_iter().collect()),
			&PlainNodeKeyPair::new(self_key_pair),
			&Default::default()), true);
	}

	#[test]
	fn is_not_processed_by_this_key_server_when_not_a_part_of_servers_set() {
		assert!(is_processed_by_this_key_server(
			&MapKeyServerSet::new(vec![
				(Random.generate().unwrap().public().clone(), "127.0.0.1:8080".parse().unwrap())
			].into_iter().collect()),
			&PlainNodeKeyPair::new(Random.generate().unwrap()),
			&Default::default()));
	}

	#[test]
	fn is_processed_by_this_key_server_in_set_of_3() {
		// servers set is ordered && server range depends on index of this server
		let servers_set = MapKeyServerSet::new(vec![
			// secret: 0000000000000000000000000000000000000000000000000000000000000001
			("79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798483ada7726a3c4655da4fbfc0e1108a8fd17b448a68554199c47d08ffb10d4b8".parse().unwrap(),
				"127.0.0.1:8080".parse().unwrap()),
			// secret: 0000000000000000000000000000000000000000000000000000000000000002
			("c6047f9441ed7d6d3045406e95c07cd85c778e4b8cef3ca7abac09b95c709ee51ae168fea63dc339a3c58419466ceaeef7f632653266d0e1236431a950cfe52a".parse().unwrap(),
				"127.0.0.1:8080".parse().unwrap()),
			// secret: 0000000000000000000000000000000000000000000000000000000000000003
			("f9308a019258c31049344f85f89d5229b531c845836f99b08601f113bce036f9388f7b0f632de8140fe337e62a37f3566500a99934c2231b6cb9fd7584b8e672".parse().unwrap(),
				"127.0.0.1:8080".parse().unwrap()),
		].into_iter().collect());

		// 1st server: process hashes [0x0; 0x555...555]
		let key_pair = PlainNodeKeyPair::new(KeyPair::from_secret(
			"0000000000000000000000000000000000000000000000000000000000000001".parse().unwrap()).unwrap());
		assert_eq!(is_processed_by_this_key_server(&servers_set, &key_pair,
			&"0000000000000000000000000000000000000000000000000000000000000000".parse().unwrap()), true);
		assert_eq!(is_processed_by_this_key_server(&servers_set, &key_pair,
			&"3000000000000000000000000000000000000000000000000000000000000000".parse().unwrap()), true);
		assert_eq!(is_processed_by_this_key_server(&servers_set, &key_pair,
			&"5555555555555555555555555555555555555555555555555555555555555555".parse().unwrap()), true);
		assert_eq!(is_processed_by_this_key_server(&servers_set, &key_pair,
			&"5555555555555555555555555555555555555555555555555555555555555556".parse().unwrap()), false);

		// 2nd server: process hashes from 0x555...556 to 0xaaa...aab
		let key_pair = PlainNodeKeyPair::new(KeyPair::from_secret(
			"0000000000000000000000000000000000000000000000000000000000000002".parse().unwrap()).unwrap());
		assert_eq!(is_processed_by_this_key_server(&servers_set, &key_pair,
			&"5555555555555555555555555555555555555555555555555555555555555555".parse().unwrap()), false);
		assert_eq!(is_processed_by_this_key_server(&servers_set, &key_pair,
			&"5555555555555555555555555555555555555555555555555555555555555556".parse().unwrap()), true);
		assert_eq!(is_processed_by_this_key_server(&servers_set, &key_pair,
			&"7555555555555555555555555555555555555555555555555555555555555555".parse().unwrap()), true);
		assert_eq!(is_processed_by_this_key_server(&servers_set, &key_pair,
			&"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaab".parse().unwrap()), true);
		assert_eq!(is_processed_by_this_key_server(&servers_set, &key_pair,
			&"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaac".parse().unwrap()), false);

		// 3rd server: process hashes from 0x800...000 to 0xbff...ff
		let key_pair = PlainNodeKeyPair::new(KeyPair::from_secret(
			"0000000000000000000000000000000000000000000000000000000000000003".parse().unwrap()).unwrap());
		assert_eq!(is_processed_by_this_key_server(&servers_set, &key_pair,
			&"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaab".parse().unwrap()), false);
		assert_eq!(is_processed_by_this_key_server(&servers_set, &key_pair,
			&"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaac".parse().unwrap()), true);
		assert_eq!(is_processed_by_this_key_server(&servers_set, &key_pair,
			&"daaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaac".parse().unwrap()), true);
		assert_eq!(is_processed_by_this_key_server(&servers_set, &key_pair,
			&"ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff".parse().unwrap()), true);
	}

	#[test]
	fn is_processed_by_this_key_server_in_set_of_4() {
		// servers set is ordered && server range depends on index of this server
		let servers_set = MapKeyServerSet::new(vec![
			// secret: 0000000000000000000000000000000000000000000000000000000000000001
			("79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798483ada7726a3c4655da4fbfc0e1108a8fd17b448a68554199c47d08ffb10d4b8".parse().unwrap(),
				"127.0.0.1:8080".parse().unwrap()),
			// secret: 0000000000000000000000000000000000000000000000000000000000000002
			("c6047f9441ed7d6d3045406e95c07cd85c778e4b8cef3ca7abac09b95c709ee51ae168fea63dc339a3c58419466ceaeef7f632653266d0e1236431a950cfe52a".parse().unwrap(),
				"127.0.0.1:8080".parse().unwrap()),
			// secret: 0000000000000000000000000000000000000000000000000000000000000004
			("e493dbf1c10d80f3581e4904930b1404cc6c13900ee0758474fa94abe8c4cd1351ed993ea0d455b75642e2098ea51448d967ae33bfbdfe40cfe97bdc47739922".parse().unwrap(),
				"127.0.0.1:8080".parse().unwrap()),
			// secret: 0000000000000000000000000000000000000000000000000000000000000003
			("f9308a019258c31049344f85f89d5229b531c845836f99b08601f113bce036f9388f7b0f632de8140fe337e62a37f3566500a99934c2231b6cb9fd7584b8e672".parse().unwrap(),
				"127.0.0.1:8080".parse().unwrap()),
		].into_iter().collect());

		// 1st server: process hashes [0x0; 0x3ff...ff]
		let key_pair = PlainNodeKeyPair::new(KeyPair::from_secret(
			"0000000000000000000000000000000000000000000000000000000000000001".parse().unwrap()).unwrap());
		assert_eq!(is_processed_by_this_key_server(&servers_set, &key_pair,
			&"0000000000000000000000000000000000000000000000000000000000000000".parse().unwrap()), true);
		assert_eq!(is_processed_by_this_key_server(&servers_set, &key_pair,
			&"2000000000000000000000000000000000000000000000000000000000000000".parse().unwrap()), true);
		assert_eq!(is_processed_by_this_key_server(&servers_set, &key_pair,
			&"3fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff".parse().unwrap()), true);
		assert_eq!(is_processed_by_this_key_server(&servers_set, &key_pair,
			&"4000000000000000000000000000000000000000000000000000000000000000".parse().unwrap()), false);

		// 2nd server: process hashes from 0x400...000 to 0x7ff...ff
		let key_pair = PlainNodeKeyPair::new(KeyPair::from_secret(
			"0000000000000000000000000000000000000000000000000000000000000002".parse().unwrap()).unwrap());
		assert_eq!(is_processed_by_this_key_server(&servers_set, &key_pair,
			&"3fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff".parse().unwrap()), false);
		assert_eq!(is_processed_by_this_key_server(&servers_set, &key_pair,
			&"4000000000000000000000000000000000000000000000000000000000000000".parse().unwrap()), true);
		assert_eq!(is_processed_by_this_key_server(&servers_set, &key_pair,
			&"6000000000000000000000000000000000000000000000000000000000000000".parse().unwrap()), true);
		assert_eq!(is_processed_by_this_key_server(&servers_set, &key_pair,
			&"7fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff".parse().unwrap()), true);
		assert_eq!(is_processed_by_this_key_server(&servers_set, &key_pair,
			&"8000000000000000000000000000000000000000000000000000000000000000".parse().unwrap()), false);

		// 3rd server: process hashes from 0x800...000 to 0xbff...ff
		let key_pair = PlainNodeKeyPair::new(KeyPair::from_secret(
			"0000000000000000000000000000000000000000000000000000000000000004".parse().unwrap()).unwrap());
		assert_eq!(is_processed_by_this_key_server(&servers_set, &key_pair,
			&"7fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff".parse().unwrap()), false);
		assert_eq!(is_processed_by_this_key_server(&servers_set, &key_pair,
			&"8000000000000000000000000000000000000000000000000000000000000000".parse().unwrap()), true);
		assert_eq!(is_processed_by_this_key_server(&servers_set, &key_pair,
			&"a000000000000000000000000000000000000000000000000000000000000000".parse().unwrap()), true);
		assert_eq!(is_processed_by_this_key_server(&servers_set, &key_pair,
			&"bfffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff".parse().unwrap()), true);
		assert_eq!(is_processed_by_this_key_server(&servers_set, &key_pair,
			&"c000000000000000000000000000000000000000000000000000000000000000".parse().unwrap()), false);

		// 4th server: process hashes from 0xc00...000 to 0xfff...ff
		let key_pair = PlainNodeKeyPair::new(KeyPair::from_secret(
			"0000000000000000000000000000000000000000000000000000000000000003".parse().unwrap()).unwrap());
		assert_eq!(is_processed_by_this_key_server(&servers_set, &key_pair,
			&"bfffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff".parse().unwrap()), false);
		assert_eq!(is_processed_by_this_key_server(&servers_set, &key_pair,
			&"c000000000000000000000000000000000000000000000000000000000000000".parse().unwrap()), true);
		assert_eq!(is_processed_by_this_key_server(&servers_set, &key_pair,
			&"e000000000000000000000000000000000000000000000000000000000000000".parse().unwrap()), true);
		assert_eq!(is_processed_by_this_key_server(&servers_set, &key_pair,
			&"ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff".parse().unwrap()), true);
	}

	#[test]
	fn no_tasks_scheduled_when_no_contract_events() {
		let listener = make_service_contract_listener(None, None, None);
		assert_eq!(listener.data.tasks_queue.snapshot().len(), 1);
		listener.process_service_contract_events();
		assert_eq!(listener.data.tasks_queue.snapshot().len(), 1);
	}

	#[test]
	fn server_key_generation_is_scheduled_when_requested_key_is_unknown() {
		let mut contract = DummyServiceContract::default();
		contract.logs.push(vec![Default::default(), Default::default(), Default::default()]);
		let listener = make_service_contract_listener(Some(Arc::new(contract)), None, None);
		assert_eq!(listener.data.tasks_queue.snapshot().len(), 1);
		listener.process_service_contract_events();
		assert_eq!(listener.data.tasks_queue.snapshot().len(), 2);
		assert_eq!(listener.data.tasks_queue.snapshot().pop_back(), Some(ServiceTask::GenerateServerKey(Default::default(), Default::default())));
	}

	#[test]
	fn no_new_tasks_scheduled_when_requested_key_is_unknown_and_request_belongs_to_other_key_server() {
		let server_key_id = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff".parse().unwrap();
		let mut contract = DummyServiceContract::default();
		contract.logs.push(vec![Default::default(), server_key_id, Default::default()]);
		let listener = make_service_contract_listener(Some(Arc::new(contract)), None, None);
		assert_eq!(listener.data.tasks_queue.snapshot().len(), 1);
		listener.process_service_contract_events();
		assert_eq!(listener.data.tasks_queue.snapshot().len(), 1);
	}

	#[test]
	fn server_key_restore_is_scheduled_when_requested_key_is_known() {
		let mut contract = DummyServiceContract::default();
		contract.logs.push(vec![Default::default(), Default::default(), Default::default()]);
		let listener = make_service_contract_listener(Some(Arc::new(contract)), None, None);
		listener.data.key_storage.insert(Default::default(), Default::default()).unwrap();
		assert_eq!(listener.data.tasks_queue.snapshot().len(), 1);
		listener.process_service_contract_events();
		assert_eq!(listener.data.tasks_queue.snapshot().len(), 2);
		assert_eq!(listener.data.tasks_queue.snapshot().pop_back(), Some(ServiceTask::RestoreServerKey(Default::default())));
	}

	#[test]
	fn no_new_tasks_scheduled_when_wrong_number_of_topics_in_log() {
		let mut contract = DummyServiceContract::default();
		contract.logs.push(vec![Default::default(), Default::default()]);
		let listener = make_service_contract_listener(Some(Arc::new(contract)), None, None);
		assert_eq!(listener.data.tasks_queue.snapshot().len(), 1);
		listener.process_service_contract_events();
		assert_eq!(listener.data.tasks_queue.snapshot().len(), 1);
	}

	#[test]
	fn generation_session_is_created_when_processing_generate_server_key_task() {
		let key_server = Arc::new(DummyKeyServer::default());
		let listener = make_service_contract_listener(None, Some(key_server.clone()), None);
		ServiceContractListener::process_service_task(&listener.data, ServiceTask::GenerateServerKey(Default::default(), Default::default())).unwrap_err();
		assert_eq!(key_server.generation_requests_count.load(Ordering::Relaxed), 1);
	}

	#[test]
	fn key_is_read_and_published_when_processing_restore_server_key_task() {
		let contract = Arc::new(DummyServiceContract::default());
		let key_storage = Arc::new(DummyKeyStorage::default());
		let mut key_share = DocumentKeyShare::default();
		key_share.public = KeyPair::from_secret("0000000000000000000000000000000000000000000000000000000000000001".parse().unwrap()).unwrap().public().clone();
		key_storage.insert(Default::default(), key_share.clone()).unwrap();
		let listener = make_service_contract_listener(Some(contract.clone()), None, Some(key_storage));
		ServiceContractListener::process_service_task(&listener.data, ServiceTask::RestoreServerKey(Default::default())).unwrap();
		assert_eq!(*contract.published_keys.lock(), vec![(Default::default(), key_share.public)]);
	}

	#[test]
	fn generation_is_not_retried_if_tried_in_the_same_cycle() {
		let mut contract = DummyServiceContract::default();
		contract.pending_requests.push((false, ServiceTask::GenerateServerKey(Default::default(), Default::default())));
		let key_server = Arc::new(DummyKeyServer::default());
		let listener = make_service_contract_listener(Some(Arc::new(contract)), Some(key_server.clone()), None);
		listener.data.retry_data.lock().generated_keys.insert(Default::default());
		ServiceContractListener::retry_pending_requests(&listener.data).unwrap();
		assert_eq!(key_server.generation_requests_count.load(Ordering::Relaxed), 0);
	}
}
