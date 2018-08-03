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

//! Creates and registers client and network services.

use std::sync::Arc;
use std::path::Path;
use ethereum_types::H256;
use kvdb::KeyValueDB;
use kvdb_rocksdb::{Database, DatabaseConfig};
use bytes::Bytes;
use io::*;
use spec::Spec;
use error::*;
use client::{Client, ClientConfig, ChainNotify};
use miner::Miner;

use snapshot::{ManifestData, RestorationStatus};
use snapshot::service::{Service as SnapshotService, ServiceParams as SnapServiceParams};
use ansi_term::Colour;
use stop_guard::StopGuard;

/// Message type for external and internal events
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ClientIoMessage {
	/// Best Block Hash in chain has been changed
	NewChainHead,
	/// A block is ready
	BlockVerified,
	/// New transaction RLPs are ready to be imported
	NewTransactions(Vec<Bytes>, usize),
	/// Begin snapshot restoration
	BeginRestoration(ManifestData),
	/// Feed a state chunk to the snapshot service
	FeedStateChunk(H256, Bytes),
	/// Feed a block chunk to the snapshot service
	FeedBlockChunk(H256, Bytes),
	/// Take a snapshot for the block with given number.
	TakeSnapshot(u64),
	/// New consensus message received.
	NewMessage(Bytes)
}

/// Client service setup. Creates and registers client and network services with the IO subsystem.
pub struct ClientService {
	io_service: Arc<IoService<ClientIoMessage>>,
	client: Arc<Client>,
	snapshot: Arc<SnapshotService>,
	database: Arc<Database>,
	_stop_guard: StopGuard,
}

impl ClientService {
	/// Start the `ClientService`.
	pub fn start(
		config: ClientConfig,
		spec: &Spec,
		client_path: &Path,
		snapshot_path: &Path,
		_ipc_path: &Path,
		miner: Arc<Miner>,
		) -> Result<ClientService, Error>
	{
		let io_service = IoService::<ClientIoMessage>::start()?;

		info!("Configured for {} using {} engine", Colour::White.bold().paint(spec.name.clone()), Colour::Yellow.bold().paint(spec.engine.name()));

		let mut db_config = DatabaseConfig::with_columns(::db::NUM_COLUMNS);

		db_config.memory_budget = config.db_cache_size;
		db_config.compaction = config.db_compaction.compaction_profile(client_path);
		db_config.wal = config.db_wal;

		let db = Arc::new(Database::open(
			&db_config,
			&client_path.to_str().expect("DB path could not be converted to string.")
		).map_err(::client::Error::Database)?);


		let pruning = config.pruning;
		let client = Client::new(config, &spec, db.clone(), miner, io_service.channel())?;

		let snapshot_params = SnapServiceParams {
			engine: spec.engine.clone(),
			genesis_block: spec.genesis_block(),
			db_config: db_config.clone(),
			pruning: pruning,
			channel: io_service.channel(),
			snapshot_root: snapshot_path.into(),
			db_restore: client.clone(),
		};
		let snapshot = Arc::new(SnapshotService::new(snapshot_params)?);

		let client_io = Arc::new(ClientIoHandler {
			client: client.clone(),
			snapshot: snapshot.clone(),
		});
		io_service.register_handler(client_io)?;

		spec.engine.register_client(Arc::downgrade(&client) as _);

		let stop_guard = StopGuard::new();

		Ok(ClientService {
			io_service: Arc::new(io_service),
			client: client,
			snapshot: snapshot,
			database: db,
			_stop_guard: stop_guard,
		})
	}

	/// Get general IO interface
	pub fn register_io_handler(&self, handler: Arc<IoHandler<ClientIoMessage> + Send>) -> Result<(), IoError> {
		self.io_service.register_handler(handler)
	}

	/// Get client interface
	pub fn client(&self) -> Arc<Client> {
		self.client.clone()
	}

	/// Get snapshot interface.
	pub fn snapshot_service(&self) -> Arc<SnapshotService> {
		self.snapshot.clone()
	}

	/// Get network service component
	pub fn io(&self) -> Arc<IoService<ClientIoMessage>> {
		self.io_service.clone()
	}

	/// Set the actor to be notified on certain chain events
	pub fn add_notify(&self, notify: Arc<ChainNotify>) {
		self.client.add_notify(notify);
	}

	/// Get a handle to the database.
	pub fn db(&self) -> Arc<KeyValueDB> { self.database.clone() }
}

/// IO interface for the Client handler
struct ClientIoHandler {
	client: Arc<Client>,
	snapshot: Arc<SnapshotService>,
}

const CLIENT_TICK_TIMER: TimerToken = 0;
const SNAPSHOT_TICK_TIMER: TimerToken = 1;

const CLIENT_TICK_MS: u64 = 5000;
const SNAPSHOT_TICK_MS: u64 = 10000;

impl IoHandler<ClientIoMessage> for ClientIoHandler {
	fn initialize(&self, io: &IoContext<ClientIoMessage>) {
		io.register_timer(CLIENT_TICK_TIMER, CLIENT_TICK_MS).expect("Error registering client timer");
		io.register_timer(SNAPSHOT_TICK_TIMER, SNAPSHOT_TICK_MS).expect("Error registering snapshot timer");
	}

	fn timeout(&self, _io: &IoContext<ClientIoMessage>, timer: TimerToken) {
		match timer {
			CLIENT_TICK_TIMER => {
				use snapshot::SnapshotService;
				let snapshot_restoration = if let RestorationStatus::Ongoing{..} = self.snapshot.status() { true } else { false };
				self.client.tick(snapshot_restoration)
			},
			SNAPSHOT_TICK_TIMER => self.snapshot.tick(),
			_ => warn!("IO service triggered unregistered timer '{}'", timer),
		}
	}

	fn message(&self, _io: &IoContext<ClientIoMessage>, net_message: &ClientIoMessage) {
		use std::thread;

		match *net_message {
			ClientIoMessage::BlockVerified => { self.client.import_verified_blocks(); }
			ClientIoMessage::NewTransactions(ref transactions, peer_id) => {
				self.client.import_queued_transactions(transactions, peer_id);
			}
			ClientIoMessage::BeginRestoration(ref manifest) => {
				if let Err(e) = self.snapshot.init_restore(manifest.clone(), true) {
					warn!("Failed to initialize snapshot restoration: {}", e);
				}
			}
			ClientIoMessage::FeedStateChunk(ref hash, ref chunk) => self.snapshot.feed_state_chunk(*hash, chunk),
			ClientIoMessage::FeedBlockChunk(ref hash, ref chunk) => self.snapshot.feed_block_chunk(*hash, chunk),
			ClientIoMessage::TakeSnapshot(num) => {
				let client = self.client.clone();
				let snapshot = self.snapshot.clone();

				let res = thread::Builder::new().name("Periodic Snapshot".into()).spawn(move || {
					if let Err(e) = snapshot.take_snapshot(&*client, num) {
						warn!("Failed to take snapshot at block #{}: {}", num, e);
					}
				});

				if let Err(e) = res {
					debug!(target: "snapshot", "Failed to initialize periodic snapshot thread: {:?}", e);
				}
			},
			ClientIoMessage::NewMessage(ref message) => if let Err(e) = self.client.engine().handle_message(message) {
				trace!(target: "poa", "Invalid message received: {}", e);
			},
			_ => {} // ignore other messages
		}
	}
}

#[cfg(test)]
mod tests {
	use std::{time, thread};
	use super::*;
	use tests::helpers::*;
	use client::ClientConfig;
	use std::sync::Arc;
	use miner::Miner;
	use tempdir::TempDir;

	#[test]
	fn it_can_be_started() {
		let tempdir = TempDir::new("").unwrap();
		let client_path = tempdir.path().join("client");
		let snapshot_path = tempdir.path().join("snapshot");

		let spec = get_test_spec();
		let service = ClientService::start(
			ClientConfig::default(),
			&spec,
			&client_path,
			&snapshot_path,
			tempdir.path(),
			Arc::new(Miner::with_spec(&spec)),
		);
		assert!(service.is_ok());
		drop(service.unwrap());
		thread::park_timeout(time::Duration::from_millis(100));
	}
}
