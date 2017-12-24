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

//! Account management (personal) rpc implementation
use std::sync::Arc;

use ethcore::account_provider::AccountProvider;
use ethcore::transaction::PendingTransaction;

use bigint::prelude::U128;
use util::Address;
use bytes::ToPretty;

use jsonrpc_core::{BoxFuture, Result};
use jsonrpc_core::futures::{future, Future};
use v1::helpers::errors;
use v1::helpers::dispatch::{Dispatcher, SignWith};
use v1::helpers::accounts::unwrap_provider;
use v1::traits::Personal;
use v1::types::{H160 as RpcH160, H256 as RpcH256, U128 as RpcU128, TransactionRequest};
use v1::metadata::Metadata;

/// Account management (personal) rpc implementation.
pub struct PersonalClient<D: Dispatcher> {
	accounts: Option<Arc<AccountProvider>>,
	dispatcher: D,
	allow_perm_unlock: bool,
}

impl<D: Dispatcher> PersonalClient<D> {
	/// Creates new PersonalClient
	pub fn new(accounts: Option<Arc<AccountProvider>>, dispatcher: D, allow_perm_unlock: bool) -> Self {
		PersonalClient {
			accounts,
			dispatcher,
			allow_perm_unlock,
		}
	}

	fn account_provider(&self) -> Result<Arc<AccountProvider>> {
		unwrap_provider(&self.accounts)
	}
}

impl<D: Dispatcher + 'static> Personal for PersonalClient<D> {
	type Metadata = Metadata;

	fn accounts(&self) -> Result<Vec<RpcH160>> {
		let store = self.account_provider()?;
		let accounts = store.accounts().map_err(|e| errors::account("Could not fetch accounts.", e))?;
		Ok(accounts.into_iter().map(Into::into).collect::<Vec<RpcH160>>())
	}

	fn new_account(&self, pass: String) -> Result<RpcH160> {
		let store = self.account_provider()?;

		store.new_account(&pass)
			.map(Into::into)
			.map_err(|e| errors::account("Could not create account.", e))
	}

	fn unlock_account(&self, account: RpcH160, account_pass: String, duration: Option<RpcU128>) -> Result<bool> {
		let account: Address = account.into();
		let store = self.account_provider()?;
		let duration = match duration {
			None => None,
			Some(duration) => {
				let duration: U128 = duration.into();
				let v = duration.low_u64() as u32;
				if duration != v.into() {
					return Err(errors::invalid_params("Duration", "Invalid Number"));
				} else {
					Some(v)
				}
			},
		};

		let r = match (self.allow_perm_unlock, duration) {
			(false, None) => store.unlock_account_temporarily(account, account_pass),
			(false, _) => return Err(errors::unsupported(
				"Time-unlocking is only supported in --geth compatibility mode.",
				Some("Restart your client with --geth flag or use personal_sendTransaction instead."),
			)),
			(true, Some(0)) => store.unlock_account_permanently(account, account_pass),
			(true, Some(d)) => store.unlock_account_timed(account, account_pass, d * 1000),
			(true, None) => store.unlock_account_timed(account, account_pass, 300_000),
		};
		match r {
			Ok(_) => Ok(true),
			Err(err) => Err(errors::account("Unable to unlock the account.", err)),
		}
	}

	fn send_transaction(&self, meta: Metadata, request: TransactionRequest, password: String) -> BoxFuture<RpcH256> {
		let dispatcher = self.dispatcher.clone();
		let accounts = try_bf!(self.account_provider());

		let default = match request.from.as_ref() {
			Some(account) => Ok(account.clone().into()),
			None => accounts
				.dapp_default_address(meta.dapp_id().into())
				.map_err(|e| errors::account("Cannot find default account.", e)),
		};

		let default = match default {
			Ok(default) => default,
			Err(e) => return Box::new(future::err(e)),
		};

		Box::new(dispatcher.fill_optional_fields(request.into(), default, false)
			.and_then(move |filled| {
				let condition = filled.condition.clone().map(Into::into);
				dispatcher.sign(accounts, filled, SignWith::Password(password))
					.map(|tx| tx.into_value())
					.map(move |tx| PendingTransaction::new(tx, condition))
					.map(move |tx| (tx, dispatcher))
			})
			.and_then(|(pending_tx, dispatcher)| {
				let chain_id = pending_tx.chain_id();
				trace!(target: "miner", "send_transaction: dispatching tx: {} for chain ID {:?}",
					::rlp::encode(&*pending_tx).into_vec().pretty(), chain_id);

				dispatcher.dispatch_transaction(pending_tx).map(Into::into)
			}))
	}

	fn sign_and_send_transaction(&self, meta: Metadata, request: TransactionRequest, password: String) -> BoxFuture<RpcH256> {
		warn!("Using deprecated personal_signAndSendTransaction, use personal_sendTransaction instead.");
		self.send_transaction(meta, request, password)
	}
}
