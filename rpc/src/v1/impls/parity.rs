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

//! Parity-specific rpc implementation.
use std::sync::Arc;
use std::str::FromStr;
use std::collections::{BTreeMap, HashSet};

use ethereum_types::Address;
use version::version_data;

use crypto::{DEFAULT_MAC, ecies};
use ethkey::{Brain, Generator};
use ethstore::random_phrase;
use ethsync::{SyncProvider, ManageNetwork};
use ethcore::account_provider::AccountProvider;
use ethcore::client::{MiningBlockChainClient};
use ethcore::ids::BlockId;
use ethcore::miner::MinerService;
use ethcore::mode::Mode;
use ethcore_logger::RotatingLogger;
use node_health::{NodeHealth, Health};
use updater::{Service as UpdateService};

use jsonrpc_core::{BoxFuture, Result};
use jsonrpc_core::futures::{future, Future};
use jsonrpc_macros::Trailing;
use v1::helpers::{self, errors, fake_sign, ipfs, SigningQueue, SignerService, NetworkSettings};
use v1::helpers::accounts::unwrap_provider;
use v1::metadata::Metadata;
use v1::traits::Parity;
use v1::types::{
	Bytes, U256, U64, H160, H256, H512, CallRequest,
	Peers, Transaction, RpcSettings, Histogram,
	TransactionStats, LocalTransactionStatus,
	BlockNumber, ConsensusCapability, VersionInfo,
	OperationsInfo, DappId, ChainStatus,
	AccountInfo, HwAccountInfo, RichHeader
};
use Host;

/// Parity implementation.
pub struct ParityClient<C, M, U>  {
	client: Arc<C>,
	miner: Arc<M>,
	updater: Arc<U>,
	sync: Arc<SyncProvider>,
	net: Arc<ManageNetwork>,
	health: NodeHealth,
	accounts: Option<Arc<AccountProvider>>,
	logger: Arc<RotatingLogger>,
	settings: Arc<NetworkSettings>,
	signer: Option<Arc<SignerService>>,
	dapps_address: Option<Host>,
	ws_address: Option<Host>,
	eip86_transition: u64,
}

impl<C, M, U> ParityClient<C, M, U> where
	C: MiningBlockChainClient,
{
	/// Creates new `ParityClient`.
	pub fn new(
		client: Arc<C>,
		miner: Arc<M>,
		sync: Arc<SyncProvider>,
		updater: Arc<U>,
		net: Arc<ManageNetwork>,
		health: NodeHealth,
		accounts: Option<Arc<AccountProvider>>,
		logger: Arc<RotatingLogger>,
		settings: Arc<NetworkSettings>,
		signer: Option<Arc<SignerService>>,
		dapps_address: Option<Host>,
		ws_address: Option<Host>,
	) -> Self {
		let eip86_transition = client.eip86_transition();
		ParityClient {
			client,
			miner,
			sync,
			updater,
			net,
			health,
			accounts,
			logger,
			settings,
			signer,
			dapps_address,
			ws_address,
			eip86_transition,
		}
	}

	/// Attempt to get the `Arc<AccountProvider>`, errors if provider was not
	/// set.
	fn account_provider(&self) -> Result<Arc<AccountProvider>> {
		unwrap_provider(&self.accounts)
	}
}

impl<C, M, U> Parity for ParityClient<C, M, U> where
	C: MiningBlockChainClient + 'static,
	M: MinerService + 'static,
	U: UpdateService + 'static,
{
	type Metadata = Metadata;

	fn accounts_info(&self, dapp: Trailing<DappId>) -> Result<BTreeMap<H160, AccountInfo>> {
		let dapp = dapp.unwrap_or_default();

		let store = self.account_provider()?;
		let dapp_accounts = store
			.note_dapp_used(dapp.clone().into())
			.and_then(|_| store.dapp_addresses(dapp.into()))
			.map_err(|e| errors::account("Could not fetch accounts.", e))?
			.into_iter().collect::<HashSet<_>>();

		let info = store.accounts_info().map_err(|e| errors::account("Could not fetch account info.", e))?;
		let other = store.addresses_info();

		Ok(info
			.into_iter()
			.chain(other.into_iter())
			.filter(|&(ref a, _)| dapp_accounts.contains(a))
			.map(|(a, v)| (H160::from(a), AccountInfo { name: v.name }))
			.collect()
		)
	}

	fn hardware_accounts_info(&self) -> Result<BTreeMap<H160, HwAccountInfo>> {
		let store = self.account_provider()?;
		let info = store.hardware_accounts_info().map_err(|e| errors::account("Could not fetch account info.", e))?;
		Ok(info
			.into_iter()
			.map(|(a, v)| (H160::from(a), HwAccountInfo { name: v.name, manufacturer: v.meta }))
			.collect()
		)
	}

	fn locked_hardware_accounts_info(&self) -> Result<Vec<String>> {
		let store = self.account_provider()?;
		Ok(store.locked_hardware_accounts().map_err(|e| errors::account("Error communicating with hardware wallet.", e))?)
	}

	fn default_account(&self, meta: Self::Metadata) -> Result<H160> {
		let dapp_id = meta.dapp_id();

		Ok(self.account_provider()?
			.dapp_default_address(dapp_id.into())
			.map(Into::into)
			.ok()
			.unwrap_or_default())
	}

	fn transactions_limit(&self) -> Result<usize> {
		Ok(self.miner.transactions_limit())
	}

	fn min_gas_price(&self) -> Result<U256> {
		Ok(U256::from(self.miner.minimal_gas_price()))
	}

	fn extra_data(&self) -> Result<Bytes> {
		Ok(Bytes::new(self.miner.extra_data()))
	}

	fn gas_floor_target(&self) -> Result<U256> {
		Ok(U256::from(self.miner.gas_floor_target()))
	}

	fn gas_ceil_target(&self) -> Result<U256> {
		Ok(U256::from(self.miner.gas_ceil_target()))
	}

	fn dev_logs(&self) -> Result<Vec<String>> {
		let logs = self.logger.logs();
		Ok(logs.as_slice().to_owned())
	}

	fn dev_logs_levels(&self) -> Result<String> {
		Ok(self.logger.levels().to_owned())
	}

	fn net_chain(&self) -> Result<String> {
		Ok(self.settings.chain.clone())
	}

	fn chain_id(&self) -> Result<Option<U64>> {
		Ok(self.client.signing_chain_id().map(U64::from))
	}

	fn chain(&self) -> Result<String> {
		Ok(self.client.spec_name())
	}

	fn net_peers(&self) -> Result<Peers> {
		let sync_status = self.sync.status();
		let net_config = self.net.network_config();
		let peers = self.sync.peers().into_iter().map(Into::into).collect();

		Ok(Peers {
			active: sync_status.num_active_peers,
			connected: sync_status.num_peers,
			max: sync_status.current_max_peers(net_config.min_peers, net_config.max_peers),
			peers: peers
		})
	}

	fn net_port(&self) -> Result<u16> {
		Ok(self.settings.network_port)
	}

	fn node_name(&self) -> Result<String> {
		Ok(self.settings.name.clone())
	}

	fn registry_address(&self) -> Result<Option<H160>> {
		Ok(
			self.client
				.additional_params()
				.get("registrar")
				.and_then(|s| Address::from_str(s).ok())
				.map(|s| H160::from(s))
		)
	}

	fn rpc_settings(&self) -> Result<RpcSettings> {
		Ok(RpcSettings {
			enabled: self.settings.rpc_enabled,
			interface: self.settings.rpc_interface.clone(),
			port: self.settings.rpc_port as u64,
		})
	}

	fn default_extra_data(&self) -> Result<Bytes> {
		Ok(Bytes::new(version_data()))
	}

	fn gas_price_histogram(&self) -> BoxFuture<Histogram> {
		Box::new(future::done(self.client
			.gas_price_corpus(100)
			.histogram(10)
			.ok_or_else(errors::not_enough_data)
			.map(Into::into)
		))
	}

	fn unsigned_transactions_count(&self) -> Result<usize> {
		match self.signer {
			None => Err(errors::signer_disabled()),
			Some(ref signer) => Ok(signer.len()),
		}
	}

	fn generate_secret_phrase(&self) -> Result<String> {
		Ok(random_phrase(12))
	}

	fn phrase_to_address(&self, phrase: String) -> Result<H160> {
		Ok(Brain::new(phrase).generate().unwrap().address().into())
	}

	fn list_accounts(&self, count: u64, after: Option<H160>, block_number: Trailing<BlockNumber>) -> Result<Option<Vec<H160>>> {
		Ok(self.client
			.list_accounts(block_number.unwrap_or_default().into(), after.map(Into::into).as_ref(), count)
			.map(|a| a.into_iter().map(Into::into).collect()))
	}

	fn list_storage_keys(&self, address: H160, count: u64, after: Option<H256>, block_number: Trailing<BlockNumber>) -> Result<Option<Vec<H256>>> {
		Ok(self.client
			.list_storage(block_number.unwrap_or_default().into(), &address.into(), after.map(Into::into).as_ref(), count)
			.map(|a| a.into_iter().map(Into::into).collect()))
	}

	fn encrypt_message(&self, key: H512, phrase: Bytes) -> Result<Bytes> {
		ecies::encrypt(&key.into(), &DEFAULT_MAC, &phrase.0)
			.map_err(errors::encryption)
			.map(Into::into)
	}

	fn pending_transactions(&self) -> Result<Vec<Transaction>> {
		let block_number = self.client.chain_info().best_block_number;
		Ok(self.miner.pending_transactions().into_iter().map(|t| Transaction::from_pending(t, block_number, self.eip86_transition)).collect::<Vec<_>>())
	}

	fn future_transactions(&self) -> Result<Vec<Transaction>> {
		let block_number = self.client.chain_info().best_block_number;
		Ok(self.miner.future_transactions().into_iter().map(|t| Transaction::from_pending(t, block_number, self.eip86_transition)).collect::<Vec<_>>())
	}

	fn pending_transactions_stats(&self) -> Result<BTreeMap<H256, TransactionStats>> {
		let stats = self.sync.transactions_stats();
		Ok(stats.into_iter()
		   .map(|(hash, stats)| (hash.into(), stats.into()))
		   .collect()
		)
	}

	fn local_transactions(&self) -> Result<BTreeMap<H256, LocalTransactionStatus>> {
		// Return nothing if accounts are disabled (running as public node)
		if self.accounts.is_none() {
			return Ok(BTreeMap::new());
		}

		let transactions = self.miner.local_transactions();
		let block_number = self.client.chain_info().best_block_number;
		Ok(transactions
		   .into_iter()
		   .map(|(hash, status)| (hash.into(), LocalTransactionStatus::from(status, block_number, self.eip86_transition)))
		   .collect()
		)
	}

	fn dapps_url(&self) -> Result<String> {
		helpers::to_url(&self.dapps_address)
			.ok_or_else(|| errors::dapps_disabled())
	}

	fn ws_url(&self) -> Result<String> {
		helpers::to_url(&self.ws_address)
			.ok_or_else(|| errors::ws_disabled())
	}

	fn next_nonce(&self, address: H160) -> BoxFuture<U256> {
		let address: Address = address.into();

		Box::new(future::ok(self.miner.last_nonce(&address)
			.map(|n| n + 1.into())
			.unwrap_or_else(|| self.client.latest_nonce(&address))
			.into()
		))
	}

	fn mode(&self) -> Result<String> {
		Ok(match self.client.mode() {
			Mode::Off => "offline",
			Mode::Dark(..) => "dark",
			Mode::Passive(..) => "passive",
			Mode::Active => "active",
		}.into())
	}

	fn enode(&self) -> Result<String> {
		self.sync.enode().ok_or_else(errors::network_disabled)
	}

	fn consensus_capability(&self) -> Result<ConsensusCapability> {
		Ok(self.updater.capability().into())
	}

	fn version_info(&self) -> Result<VersionInfo> {
		Ok(self.updater.version_info().into())
	}

	fn releases_info(&self) -> Result<Option<OperationsInfo>> {
		Ok(self.updater.info().map(Into::into))
	}

	fn chain_status(&self) -> Result<ChainStatus> {
		let chain_info = self.client.chain_info();

		let gap = chain_info.ancient_block_number.map(|x| U256::from(x + 1))
			.and_then(|first| chain_info.first_block_number.map(|last| (first, U256::from(last))));

		Ok(ChainStatus {
			block_gap: gap.map(|(x, y)| (x.into(), y.into())),
		})
	}

	fn node_kind(&self) -> Result<::v1::types::NodeKind> {
		use ::v1::types::{NodeKind, Availability, Capability};

		let availability = match self.accounts {
			Some(_) => Availability::Personal,
			None => Availability::Public
		};

		Ok(NodeKind {
			availability: availability,
			capability: Capability::Full,
		})
	}

	fn block_header(&self, number: Trailing<BlockNumber>) -> BoxFuture<RichHeader> {
		const EXTRA_INFO_PROOF: &'static str = "Object exists in in blockchain (fetched earlier), extra_info is always available if object exists; qed";

		let id: BlockId = number.unwrap_or_default().into();
		let encoded = match self.client.block_header(id.clone()) {
			Some(encoded) => encoded,
			None => return Box::new(future::err(errors::unknown_block())),
		};

		Box::new(future::ok(RichHeader {
			inner: encoded.into(),
			extra_info: self.client.block_extra_info(id).expect(EXTRA_INFO_PROOF),
		}))
	}

	fn ipfs_cid(&self, content: Bytes) -> Result<String> {
		ipfs::cid(content)
	}

	fn call(&self, meta: Self::Metadata, requests: Vec<CallRequest>, block: Trailing<BlockNumber>) -> Result<Vec<Bytes>> {
		let requests = requests
			.into_iter()
			.map(|request| Ok((
				fake_sign::sign_call(request.into(), meta.is_dapp())?,
				Default::default()
			)))
			.collect::<Result<Vec<_>>>()?;

		let block = block.unwrap_or_default();

		self.client.call_many(&requests, block.into())
				.map(|res| res.into_iter().map(|res| res.output.into()).collect())
				.map_err(errors::call)
	}

	fn node_health(&self) -> BoxFuture<Health> {
		Box::new(self.health.health()
			.map_err(|err| errors::internal("Health API failure.", err)))
	}
}
