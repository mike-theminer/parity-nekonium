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

//! Blockchain block.

use std::cmp;
use std::sync::Arc;
use std::collections::HashSet;
use hash::{keccak, KECCAK_NULL_RLP, KECCAK_EMPTY_LIST_RLP};
use triehash::ordered_trie_root;

use rlp::{UntrustedRlp, RlpStream, Encodable, Decodable, DecoderError};
use ethereum_types::{H256, U256, Address, Bloom};
use bytes::Bytes;
use unexpected::{Mismatch, OutOfBounds};

use vm::{EnvInfo, LastHashes};
use engines::EthEngine;
use error::{Error, BlockError};
use factory::Factories;
use header::{Header, Seal};
use receipt::{Receipt, TransactionOutcome};
use state::State;
use state_db::StateDB;
use trace::Tracing;
use transaction::{UnverifiedTransaction, SignedTransaction, Error as TransactionError};
use verification::PreverifiedBlock;
use views::BlockView;

/// A block, encoded as it is on the block chain.
#[derive(Default, Debug, Clone, PartialEq)]
pub struct Block {
	/// The header of this block.
	pub header: Header,
	/// The transactions in this block.
	pub transactions: Vec<UnverifiedTransaction>,
	/// The uncles of this block.
	pub uncles: Vec<Header>,
}

impl Block {
	/// Returns true if the given bytes form a valid encoding of a block in RLP.
	pub fn is_good(b: &[u8]) -> bool {
		UntrustedRlp::new(b).as_val::<Block>().is_ok()
	}

	/// Get the RLP-encoding of the block with or without the seal.
	pub fn rlp_bytes(&self, seal: Seal) -> Bytes {
		let mut block_rlp = RlpStream::new_list(3);
		self.header.stream_rlp(&mut block_rlp, seal);
		block_rlp.append_list(&self.transactions);
		block_rlp.append_list(&self.uncles);
		block_rlp.out()
	}
}


impl Decodable for Block {
	fn decode(rlp: &UntrustedRlp) -> Result<Self, DecoderError> {
		if rlp.as_raw().len() != rlp.payload_info()?.total() {
			return Err(DecoderError::RlpIsTooBig);
		}
		if rlp.item_count()? != 3 {
			return Err(DecoderError::RlpIncorrectListLen);
		}
		Ok(Block {
			header: rlp.val_at(0)?,
			transactions: rlp.list_at(1)?,
			uncles: rlp.list_at(2)?,
		})
	}
}

/// An internal type for a block's common elements.
#[derive(Clone)]
pub struct ExecutedBlock {
	header: Header,
	transactions: Vec<SignedTransaction>,
	uncles: Vec<Header>,
	receipts: Vec<Receipt>,
	transactions_set: HashSet<H256>,
	state: State<StateDB>,
	traces: Tracing,
	last_hashes: Arc<LastHashes>,
}

impl ExecutedBlock {
	/// Create a new block from the given `state`.
	fn new(state: State<StateDB>, last_hashes: Arc<LastHashes>, tracing: bool) -> ExecutedBlock {
		ExecutedBlock {
			header: Default::default(),
			transactions: Default::default(),
			uncles: Default::default(),
			receipts: Default::default(),
			transactions_set: Default::default(),
			state: state,
			traces: if tracing {
				Tracing::enabled()
			} else {
				Tracing::Disabled
			},
			last_hashes: last_hashes,
		}
	}

	/// Get the environment info concerning this block.
	pub fn env_info(&self) -> EnvInfo {
		// TODO: memoise.
		EnvInfo {
			number: self.header.number(),
			author: self.header.author().clone(),
			timestamp: self.header.timestamp(),
			difficulty: self.header.difficulty().clone(),
			last_hashes: self.last_hashes.clone(),
			gas_used: self.receipts.last().map_or(U256::zero(), |r| r.gas_used),
			gas_limit: self.header.gas_limit().clone(),
		}
	}

	/// Get mutable access to a state.
	pub fn state_mut(&mut self) -> &mut State<StateDB> {
		&mut self.state
	}

	/// Get mutable reference to traces.
	pub fn traces_mut(&mut self) -> &mut Tracing {
		&mut self.traces
	}
}

/// Trait for a object that is a `ExecutedBlock`.
pub trait IsBlock {
	/// Get the `ExecutedBlock` associated with this object.
	fn block(&self) -> &ExecutedBlock;

	/// Get the base `Block` object associated with this.
	fn to_base(&self) -> Block {
		Block {
			header: self.header().clone(),
			transactions: self.transactions().iter().cloned().map(Into::into).collect(),
			uncles: self.uncles().to_vec(),
		}
	}

	/// Get the header associated with this object's block.
	fn header(&self) -> &Header { &self.block().header }

	/// Get the final state associated with this object's block.
	fn state(&self) -> &State<StateDB> { &self.block().state }

	/// Get all information on transactions in this block.
	fn transactions(&self) -> &[SignedTransaction] { &self.block().transactions }

	/// Get all information on receipts in this block.
	fn receipts(&self) -> &[Receipt] { &self.block().receipts }

	/// Get all information concerning transaction tracing in this block.
	fn traces(&self) -> &Tracing { &self.block().traces }

	/// Get all uncles in this block.
	fn uncles(&self) -> &[Header] { &self.block().uncles }

	/// Get tracing enabled flag for this block.
	fn tracing_enabled(&self) -> bool { self.block().traces.is_enabled() }
}

/// Trait for a object that has a state database.
pub trait Drain {
	/// Drop this object and return the underlying database.
	fn drain(self) -> StateDB;
}

impl IsBlock for ExecutedBlock {
	fn block(&self) -> &ExecutedBlock { self }
}

impl ::parity_machine::LiveBlock for ExecutedBlock {
	type Header = Header;

	fn header(&self) -> &Header {
		&self.header
	}

	fn uncles(&self) -> &[Header] {
		&self.uncles
	}
}

impl ::parity_machine::Transactions for ExecutedBlock {
	type Transaction = SignedTransaction;

	fn transactions(&self) -> &[SignedTransaction] {
		&self.transactions
	}
}

/// Block that is ready for transactions to be added.
///
/// It's a bit like a Vec<Transaction>, except that whenever a transaction is pushed, we execute it and
/// maintain the system `state()`. We also archive execution receipts in preparation for later block creation.
pub struct OpenBlock<'x> {
	block: ExecutedBlock,
	engine: &'x EthEngine,
}

/// Just like `OpenBlock`, except that we've applied `Engine::on_close_block`, finished up the non-seal header fields,
/// and collected the uncles.
///
/// There is no function available to push a transaction.
#[derive(Clone)]
pub struct ClosedBlock {
	block: ExecutedBlock,
	uncle_bytes: Bytes,
	unclosed_state: State<StateDB>,
}

/// Just like `ClosedBlock` except that we can't reopen it and it's faster.
///
/// We actually store the post-`Engine::on_close_block` state, unlike in `ClosedBlock` where it's the pre.
#[derive(Clone)]
pub struct LockedBlock {
	block: ExecutedBlock,
	uncle_bytes: Bytes,
}

/// A block that has a valid seal.
///
/// The block's header has valid seal arguments. The block cannot be reversed into a `ClosedBlock` or `OpenBlock`.
pub struct SealedBlock {
	block: ExecutedBlock,
	uncle_bytes: Bytes,
}

impl<'x> OpenBlock<'x> {
	/// Create a new `OpenBlock` ready for transaction pushing.
	pub fn new(
		engine: &'x EthEngine,
		factories: Factories,
		tracing: bool,
		db: StateDB,
		parent: &Header,
		last_hashes: Arc<LastHashes>,
		author: Address,
		gas_range_target: (U256, U256),
		extra_data: Bytes,
		is_epoch_begin: bool,
	) -> Result<Self, Error> {
		let number = parent.number() + 1;
		let state = State::from_existing(db, parent.state_root().clone(), engine.account_start_nonce(number), factories)?;
		let mut r = OpenBlock {
			block: ExecutedBlock::new(state, last_hashes, tracing),
			engine: engine,
		};

		r.block.header.set_parent_hash(parent.hash());
		r.block.header.set_number(number);
		r.block.header.set_author(author);
		r.block.header.set_timestamp_now(parent.timestamp());
		r.block.header.set_extra_data(extra_data);
		r.block.header.note_dirty();

		let gas_floor_target = cmp::max(gas_range_target.0, engine.params().min_gas_limit);
		let gas_ceil_target = cmp::max(gas_range_target.1, gas_floor_target);

		engine.machine().populate_from_parent(&mut r.block.header, parent, gas_floor_target, gas_ceil_target);
		engine.populate_from_parent(&mut r.block.header, parent);

		engine.machine().on_new_block(&mut r.block)?;
		engine.on_new_block(&mut r.block, is_epoch_begin)?;

		Ok(r)
	}

	/// Alter the author for the block.
	pub fn set_author(&mut self, author: Address) { self.block.header.set_author(author); }

	/// Alter the timestamp of the block.
	pub fn set_timestamp(&mut self, timestamp: u64) { self.block.header.set_timestamp(timestamp); }

	/// Alter the difficulty for the block.
	pub fn set_difficulty(&mut self, a: U256) { self.block.header.set_difficulty(a); }

	/// Alter the gas limit for the block.
	pub fn set_gas_limit(&mut self, a: U256) { self.block.header.set_gas_limit(a); }

	/// Alter the gas limit for the block.
	pub fn set_gas_used(&mut self, a: U256) { self.block.header.set_gas_used(a); }

	/// Alter the uncles hash the block.
	pub fn set_uncles_hash(&mut self, h: H256) { self.block.header.set_uncles_hash(h); }

	/// Alter transactions root for the block.
	pub fn set_transactions_root(&mut self, h: H256) { self.block.header.set_transactions_root(h); }

	/// Alter the receipts root for the block.
	pub fn set_receipts_root(&mut self, h: H256) { self.block.header.set_receipts_root(h); }

	/// Alter the extra_data for the block.
	pub fn set_extra_data(&mut self, extra_data: Bytes) -> Result<(), BlockError> {
		if extra_data.len() > self.engine.maximum_extra_data_size() {
			Err(BlockError::ExtraDataOutOfBounds(OutOfBounds{min: None, max: Some(self.engine.maximum_extra_data_size()), found: extra_data.len()}))
		} else {
			self.block.header.set_extra_data(extra_data);
			Ok(())
		}
	}

	/// Add an uncle to the block, if possible.
	///
	/// NOTE Will check chain constraints and the uncle number but will NOT check
	/// that the header itself is actually valid.
	pub fn push_uncle(&mut self, valid_uncle_header: Header) -> Result<(), BlockError> {
		let max_uncles = self.engine.maximum_uncle_count(self.block.header().number());
		if self.block.uncles.len() + 1 > max_uncles {
			return Err(BlockError::TooManyUncles(OutOfBounds{
				min: None,
				max: Some(max_uncles),
				found: self.block.uncles.len() + 1,
			}));
		}
		// TODO: check number
		// TODO: check not a direct ancestor (use last_hashes for that)
		self.block.uncles.push(valid_uncle_header);
		Ok(())
	}

	/// Get the environment info concerning this block.
	pub fn env_info(&self) -> EnvInfo {
		self.block.env_info()
	}

	/// Push a transaction into the block.
	///
	/// If valid, it will be executed, and archived together with the receipt.
	pub fn push_transaction(&mut self, t: SignedTransaction, h: Option<H256>) -> Result<&Receipt, Error> {
		if self.block.transactions_set.contains(&t.hash()) {
			return Err(From::from(TransactionError::AlreadyImported));
		}

		let env_info = self.env_info();
//		info!("env_info says gas_used={}", env_info.gas_used);
		match self.block.state.apply(&env_info, self.engine.machine(), &t, self.block.traces.is_enabled()) {
			Ok(outcome) => {
				self.block.transactions_set.insert(h.unwrap_or_else(||t.hash()));
				self.block.transactions.push(t.into());
				let t = outcome.trace;
				if let Tracing::Enabled(ref mut traces) = self.block.traces {
					traces.push(t.into());
				}
				self.block.receipts.push(outcome.receipt);
				Ok(self.block.receipts.last().expect("receipt just pushed; qed"))
			}
			Err(x) => Err(From::from(x))
		}
	}

	/// Push transactions onto the block.
	pub fn push_transactions(&mut self, transactions: &[SignedTransaction]) -> Result<(), Error> {
		push_transactions(self, transactions)
	}

	/// Populate self from a header.
	pub fn populate_from(&mut self, header: &Header) {
		self.set_difficulty(*header.difficulty());
		self.set_gas_limit(*header.gas_limit());
		self.set_timestamp(header.timestamp());
		self.set_author(header.author().clone());
		self.set_extra_data(header.extra_data().clone()).unwrap_or_else(|e| warn!("Couldn't set extradata: {}. Ignoring.", e));
		self.set_uncles_hash(header.uncles_hash().clone());
		self.set_transactions_root(header.transactions_root().clone());
	}

	/// Turn this into a `ClosedBlock`.
	pub fn close(self) -> ClosedBlock {
		let mut s = self;

		let unclosed_state = s.block.state.clone();

		if let Err(e) = s.engine.on_close_block(&mut s.block) {
			warn!("Encountered error on closing the block: {}", e);
		}

		if let Err(e) = s.block.state.commit() {
			warn!("Encountered error on state commit: {}", e);
		}
		s.block.header.set_transactions_root(ordered_trie_root(s.block.transactions.iter().map(|e| e.rlp_bytes())));
		let uncle_bytes = s.block.uncles.iter().fold(RlpStream::new_list(s.block.uncles.len()), |mut s, u| {s.append_raw(&u.rlp(Seal::With), 1); s} ).out();
		s.block.header.set_uncles_hash(keccak(&uncle_bytes));
		s.block.header.set_state_root(s.block.state.root().clone());
		s.block.header.set_receipts_root(ordered_trie_root(s.block.receipts.iter().map(|r| r.rlp_bytes())));
		s.block.header.set_log_bloom(s.block.receipts.iter().fold(Bloom::zero(), |mut b, r| {b = &b | &r.log_bloom; b})); //TODO: use |= operator
		s.block.header.set_gas_used(s.block.receipts.last().map_or(U256::zero(), |r| r.gas_used));

		ClosedBlock {
			block: s.block,
			uncle_bytes: uncle_bytes,
			unclosed_state: unclosed_state,
		}
	}

	/// Turn this into a `LockedBlock`.
	pub fn close_and_lock(self) -> LockedBlock {
		let mut s = self;

		if let Err(e) = s.engine.on_close_block(&mut s.block) {
			warn!("Encountered error on closing the block: {}", e);
		}

		if let Err(e) = s.block.state.commit() {
			warn!("Encountered error on state commit: {}", e);
		}
		if s.block.header.transactions_root().is_zero() || s.block.header.transactions_root() == &KECCAK_NULL_RLP {
			s.block.header.set_transactions_root(ordered_trie_root(s.block.transactions.iter().map(|e| e.rlp_bytes())));
		}
		let uncle_bytes = s.block.uncles.iter().fold(RlpStream::new_list(s.block.uncles.len()), |mut s, u| {s.append_raw(&u.rlp(Seal::With), 1); s} ).out();
		if s.block.header.uncles_hash().is_zero() || s.block.header.uncles_hash() == &KECCAK_EMPTY_LIST_RLP {
			s.block.header.set_uncles_hash(keccak(&uncle_bytes));
		}
		if s.block.header.receipts_root().is_zero() || s.block.header.receipts_root() == &KECCAK_NULL_RLP {
			s.block.header.set_receipts_root(ordered_trie_root(s.block.receipts.iter().map(|r| r.rlp_bytes())));
		}

		s.block.header.set_state_root(s.block.state.root().clone());
		s.block.header.set_log_bloom(s.block.receipts.iter().fold(Bloom::zero(), |mut b, r| {b = &b | &r.log_bloom; b})); //TODO: use |= operator
		s.block.header.set_gas_used(s.block.receipts.last().map_or(U256::zero(), |r| r.gas_used));

		LockedBlock {
			block: s.block,
			uncle_bytes: uncle_bytes,
		}
	}

	#[cfg(test)]
	/// Return mutable block reference. To be used in tests only.
	pub fn block_mut(&mut self) -> &mut ExecutedBlock { &mut self.block }
}

impl<'x> IsBlock for OpenBlock<'x> {
	fn block(&self) -> &ExecutedBlock { &self.block }
}

impl<'x> IsBlock for ClosedBlock {
	fn block(&self) -> &ExecutedBlock { &self.block }
}

impl<'x> IsBlock for LockedBlock {
	fn block(&self) -> &ExecutedBlock { &self.block }
}

impl ClosedBlock {
	/// Get the hash of the header without seal arguments.
	pub fn hash(&self) -> H256 { self.header().rlp_keccak(Seal::Without) }

	/// Turn this into a `LockedBlock`, unable to be reopened again.
	pub fn lock(self) -> LockedBlock {
		LockedBlock {
			block: self.block,
			uncle_bytes: self.uncle_bytes,
		}
	}

	/// Given an engine reference, reopen the `ClosedBlock` into an `OpenBlock`.
	pub fn reopen(self, engine: &EthEngine) -> OpenBlock {
		// revert rewards (i.e. set state back at last transaction's state).
		let mut block = self.block;
		block.state = self.unclosed_state;
		OpenBlock {
			block: block,
			engine: engine,
		}
	}
}

impl LockedBlock {
	/// Get the hash of the header without seal arguments.
	pub fn hash(&self) -> H256 { self.header().rlp_keccak(Seal::Without) }

	/// Provide a valid seal in order to turn this into a `SealedBlock`.
	///
	/// NOTE: This does not check the validity of `seal` with the engine.
	pub fn seal(self, engine: &EthEngine, seal: Vec<Bytes>) -> Result<SealedBlock, BlockError> {
		let expected_seal_fields = engine.seal_fields(self.header());
		let mut s = self;
		if seal.len() != expected_seal_fields {
			return Err(BlockError::InvalidSealArity(
				Mismatch { expected: expected_seal_fields, found: seal.len() }));
		}
		s.block.header.set_seal(seal);
		Ok(SealedBlock { block: s.block, uncle_bytes: s.uncle_bytes })
	}

	/// Provide a valid seal in order to turn this into a `SealedBlock`.
	/// This does check the validity of `seal` with the engine.
	/// Returns the `ClosedBlock` back again if the seal is no good.
	pub fn try_seal(
		self,
		engine: &EthEngine,
		seal: Vec<Bytes>,
	) -> Result<SealedBlock, (Error, LockedBlock)> {
		let mut s = self;
		s.block.header.set_seal(seal);

		// TODO: passing state context to avoid engines owning it?
		match engine.verify_local_seal(&s.block.header) {
			Err(e) => Err((e, s)),
			_ => Ok(SealedBlock { block: s.block, uncle_bytes: s.uncle_bytes }),
		}
	}

	/// Remove state root from transaction receipts to make them EIP-98 compatible.
	pub fn strip_receipts(self) -> LockedBlock {
		let mut block = self;
		for receipt in &mut block.block.receipts {
			receipt.outcome = TransactionOutcome::Unknown;
		}
		block.block.header.set_receipts_root(ordered_trie_root(block.block.receipts.iter().map(|r| r.rlp_bytes())));
		block
	}
}

impl Drain for LockedBlock {
	/// Drop this object and return the underlieing database.
	fn drain(self) -> StateDB {
		self.block.state.drop().1
	}
}

impl SealedBlock {
	/// Get the RLP-encoding of the block.
	pub fn rlp_bytes(&self) -> Bytes {
		let mut block_rlp = RlpStream::new_list(3);
		self.block.header.stream_rlp(&mut block_rlp, Seal::With);
		block_rlp.append_list(&self.block.transactions);
		block_rlp.append_raw(&self.uncle_bytes, 1);
		block_rlp.out()
	}
}

impl Drain for SealedBlock {
	/// Drop this object and return the underlieing database.
	fn drain(self) -> StateDB {
		self.block.state.drop().1
	}
}

impl IsBlock for SealedBlock {
	fn block(&self) -> &ExecutedBlock { &self.block }
}

/// Enact the block given by block header, transactions and uncles
pub fn enact(
	header: &Header,
	transactions: &[SignedTransaction],
	uncles: &[Header],
	engine: &EthEngine,
	tracing: bool,
	db: StateDB,
	parent: &Header,
	last_hashes: Arc<LastHashes>,
	factories: Factories,
	is_epoch_begin: bool,
) -> Result<LockedBlock, Error> {
	{
		if ::log::max_log_level() >= ::log::LogLevel::Trace {
			let s = State::from_existing(db.boxed_clone(), parent.state_root().clone(), engine.account_start_nonce(parent.number() + 1), factories.clone())?;
			trace!(target: "enact", "num={}, root={}, author={}, author_balance={}\n",
				header.number(), s.root(), header.author(), s.balance(&header.author())?);
		}
	}

	let mut b = OpenBlock::new(
		engine,
		factories,
		tracing,
		db,
		parent,
		last_hashes,
		Address::new(),
		(3141562.into(), 31415620.into()),
		vec![],
		is_epoch_begin,
	)?;

	b.populate_from(header);
	b.push_transactions(transactions)?;

	for u in uncles {
		b.push_uncle(u.clone())?;
	}

	Ok(b.close_and_lock())
}

#[inline]
#[cfg(not(feature = "slow-blocks"))]
fn push_transactions(block: &mut OpenBlock, transactions: &[SignedTransaction]) -> Result<(), Error> {
	for t in transactions {
		block.push_transaction(t.clone(), None)?;
	}
	Ok(())
}

#[cfg(feature = "slow-blocks")]
fn push_transactions(block: &mut OpenBlock, transactions: &[SignedTransaction]) -> Result<(), Error> {
	use std::time;

	let slow_tx = option_env!("SLOW_TX_DURATION").and_then(|v| v.parse().ok()).unwrap_or(100);
	for t in transactions {
		let hash = t.hash();
		let start = time::Instant::now();
		block.push_transaction(t.clone(), None)?;
		let took = start.elapsed();
		let took_ms = took.as_secs() * 1000 + took.subsec_nanos() as u64 / 1000000;
		if took > time::Duration::from_millis(slow_tx) {
			warn!("Heavy ({} ms) transaction in block {:?}: {:?}", took_ms, block.header().number(), hash);
		}
		debug!(target: "tx", "Transaction {:?} took: {} ms", hash, took_ms);
	}
	Ok(())
}

// TODO [ToDr] Pass `PreverifiedBlock` by move, this will avoid unecessary allocation
/// Enact the block given by `block_bytes` using `engine` on the database `db` with given `parent` block header
pub fn enact_verified(
	block: &PreverifiedBlock,
	engine: &EthEngine,
	tracing: bool,
	db: StateDB,
	parent: &Header,
	last_hashes: Arc<LastHashes>,
	factories: Factories,
	is_epoch_begin: bool,
) -> Result<LockedBlock, Error> {
	let view = BlockView::new(&block.bytes);

	enact(
		&block.header,
		&block.transactions,
		&view.uncles(),
		engine,
		tracing,
		db,
		parent,
		last_hashes,
		factories,
		is_epoch_begin,
	)
}

#[cfg(test)]
mod tests {
	use tests::helpers::*;
	use super::*;
	use engines::EthEngine;
	use vm::LastHashes;
	use error::Error;
	use header::Header;
	use factory::Factories;
	use state_db::StateDB;
	use views::BlockView;
	use ethereum_types::Address;
	use std::sync::Arc;
	use transaction::SignedTransaction;

	/// Enact the block given by `block_bytes` using `engine` on the database `db` with given `parent` block header
	fn enact_bytes(
		block_bytes: &[u8],
		engine: &EthEngine,
		tracing: bool,
		db: StateDB,
		parent: &Header,
		last_hashes: Arc<LastHashes>,
		factories: Factories,
	) -> Result<LockedBlock, Error> {
		let block = BlockView::new(block_bytes);
		let header = block.header();
		let transactions: Result<Vec<_>, Error> = block
			.transactions()
			.into_iter()
			.map(SignedTransaction::new)
			.map(|r| r.map_err(Into::into))
			.collect();
		let transactions = transactions?;

		{
			if ::log::max_log_level() >= ::log::LogLevel::Trace {
				let s = State::from_existing(db.boxed_clone(), parent.state_root().clone(), engine.account_start_nonce(parent.number() + 1), factories.clone())?;
				trace!(target: "enact", "num={}, root={}, author={}, author_balance={}\n",
					header.number(), s.root(), header.author(), s.balance(&header.author())?);
			}
		}

		let mut b = OpenBlock::new(
			engine,
			factories,
			tracing,
			db,
			parent,
			last_hashes,
			Address::new(),
			(3141562.into(), 31415620.into()),
			vec![],
			false,
		)?;

		b.populate_from(&header);
		b.push_transactions(&transactions)?;

		for u in &block.uncles() {
			b.push_uncle(u.clone())?;
		}

		Ok(b.close_and_lock())
	}

	/// Enact the block given by `block_bytes` using `engine` on the database `db` with given `parent` block header. Seal the block aferwards
	fn enact_and_seal(
		block_bytes: &[u8],
		engine: &EthEngine,
		tracing: bool,
		db: StateDB,
		parent: &Header,
		last_hashes: Arc<LastHashes>,
		factories: Factories,
	) -> Result<SealedBlock, Error> {
		let header = BlockView::new(block_bytes).header_view();
		Ok(enact_bytes(block_bytes, engine, tracing, db, parent, last_hashes, factories)?.seal(engine, header.seal())?)
	}

	#[test]
	fn open_block() {
		use spec::*;
		let spec = Spec::new_test();
		let genesis_header = spec.genesis_header();
		let db = spec.ensure_db_good(get_temp_state_db(), &Default::default()).unwrap();
		let last_hashes = Arc::new(vec![genesis_header.hash()]);
		let b = OpenBlock::new(&*spec.engine, Default::default(), false, db, &genesis_header, last_hashes, Address::zero(), (3141562.into(), 31415620.into()), vec![], false).unwrap();
		let b = b.close_and_lock();
		let _ = b.seal(&*spec.engine, vec![]);
	}

	#[test]
	fn enact_block() {
		use spec::*;
		let spec = Spec::new_test();
		let engine = &*spec.engine;
		let genesis_header = spec.genesis_header();

		let db = spec.ensure_db_good(get_temp_state_db(), &Default::default()).unwrap();
		let last_hashes = Arc::new(vec![genesis_header.hash()]);
		let b = OpenBlock::new(engine, Default::default(), false, db, &genesis_header, last_hashes.clone(), Address::zero(), (3141562.into(), 31415620.into()), vec![], false).unwrap()
			.close_and_lock().seal(engine, vec![]).unwrap();
		let orig_bytes = b.rlp_bytes();
		let orig_db = b.drain();

		let db = spec.ensure_db_good(get_temp_state_db(), &Default::default()).unwrap();
		let e = enact_and_seal(&orig_bytes, engine, false, db, &genesis_header, last_hashes, Default::default()).unwrap();

		assert_eq!(e.rlp_bytes(), orig_bytes);

		let db = e.drain();
		assert_eq!(orig_db.journal_db().keys(), db.journal_db().keys());
		assert!(orig_db.journal_db().keys().iter().filter(|k| orig_db.journal_db().get(k.0) != db.journal_db().get(k.0)).next() == None);
	}

	#[test]
	fn enact_block_with_uncle() {
		use spec::*;
		let spec = Spec::new_test();
		let engine = &*spec.engine;
		let genesis_header = spec.genesis_header();

		let db = spec.ensure_db_good(get_temp_state_db(), &Default::default()).unwrap();
		let last_hashes = Arc::new(vec![genesis_header.hash()]);
		let mut open_block = OpenBlock::new(engine, Default::default(), false, db, &genesis_header, last_hashes.clone(), Address::zero(), (3141562.into(), 31415620.into()), vec![], false).unwrap();
		let mut uncle1_header = Header::new();
		uncle1_header.set_extra_data(b"uncle1".to_vec());
		let mut uncle2_header = Header::new();
		uncle2_header.set_extra_data(b"uncle2".to_vec());
		open_block.push_uncle(uncle1_header).unwrap();
		open_block.push_uncle(uncle2_header).unwrap();
		let b = open_block.close_and_lock().seal(engine, vec![]).unwrap();

		let orig_bytes = b.rlp_bytes();
		let orig_db = b.drain();

		let db = spec.ensure_db_good(get_temp_state_db(), &Default::default()).unwrap();
		let e = enact_and_seal(&orig_bytes, engine, false, db, &genesis_header, last_hashes, Default::default()).unwrap();

		let bytes = e.rlp_bytes();
		assert_eq!(bytes, orig_bytes);
		let uncles = BlockView::new(&bytes).uncles();
		assert_eq!(uncles[1].extra_data(), b"uncle2");

		let db = e.drain();
		assert_eq!(orig_db.journal_db().keys(), db.journal_db().keys());
		assert!(orig_db.journal_db().keys().iter().filter(|k| orig_db.journal_db().get(k.0) != db.journal_db().get(k.0)).next() == None);
	}
}
