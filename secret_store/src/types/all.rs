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

use std::fmt;
use std::collections::BTreeMap;
use serde_json;

use {ethkey, kvdb, bytes, ethereum_types, key_server_cluster};

/// Node id.
pub type NodeId = ethkey::Public;
/// Server key id. When key is used to encrypt document, it could be document contents hash.
pub type ServerKeyId = ethereum_types::H256;
/// Encrypted document key type.
pub type EncryptedDocumentKey = bytes::Bytes;
/// Message hash.
pub type MessageHash = ethereum_types::H256;
/// Message signature.
pub type EncryptedMessageSignature = bytes::Bytes;
/// Request signature type.
pub type RequestSignature = ethkey::Signature;
/// Public key type.
pub use ethkey::Public;

/// Secret store error
#[derive(Debug, PartialEq)]
pub enum Error {
	/// Bad signature is passed
	BadSignature,
	/// Access to resource is denied
	AccessDenied,
	/// Requested document not found
	DocumentNotFound,
	/// Hyper error
	Hyper(String),
	/// Serialization/deserialization error
	Serde(String),
	/// Database-related error
	Database(String),
	/// Internal error
	Internal(String),
}

/// Secret store configuration
#[derive(Debug, Clone)]
pub struct NodeAddress {
	/// IP address.
	pub address: String,
	/// IP port.
	pub port: u16,
}

/// Contract address.
#[derive(Debug, Clone)]
pub enum ContractAddress {
	/// Address is read from registry.
	Registry,
	/// Address is specified.
	Address(ethkey::Address),
}

/// Secret store configuration
#[derive(Debug)]
pub struct ServiceConfiguration {
	/// HTTP listener address. If None, HTTP API is disabled.
	pub listener_address: Option<NodeAddress>,
	/// Service contract address. If None, service contract API is disabled.
	pub service_contract_address: Option<ContractAddress>,
	/// Is ACL check enabled. If false, everyone has access to all keys. Useful for tests only.
	pub acl_check_enabled: bool,
	/// Data directory path for secret store
	pub data_path: String,
	/// Cluster configuration.
	pub cluster_config: ClusterConfiguration,
}

/// Key server cluster configuration
#[derive(Debug)]
pub struct ClusterConfiguration {
	/// Number of threads reserved by cluster.
	pub threads: usize,
	/// This node address.
	pub listener_address: NodeAddress,
	/// All cluster nodes addresses.
	pub nodes: BTreeMap<ethkey::Public, NodeAddress>,
	/// Allow outbound connections to 'higher' nodes.
	/// This is useful for tests, but slower a bit for production.
	pub allow_connecting_to_higher_nodes: bool,
	/// Administrator public key.
	pub admin_public: Option<Public>,
	/// Should key servers set change session should be started when servers set changes.
	/// This will only work when servers set is configured using KeyServerSet contract.
	pub auto_migrate_enabled: bool,
}

/// Shadow decryption result.
#[derive(Clone, Debug, PartialEq)]
pub struct EncryptedDocumentKeyShadow {
	/// Decrypted secret point. It is partially decrypted if shadow decrpytion was requested.
	pub decrypted_secret: ethkey::Public,
	/// Shared common point.
	pub common_point: Option<ethkey::Public>,
	/// If shadow decryption was requested: shadow decryption coefficients, encrypted with requestor public.
	pub decrypt_shadows: Option<Vec<Vec<u8>>>,
}

impl fmt::Display for Error {
	fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
		match *self {
			Error::BadSignature => write!(f, "Bad signature"),
			Error::AccessDenied => write!(f, "Access dened"),
			Error::DocumentNotFound => write!(f, "Document not found"),
			Error::Hyper(ref msg) => write!(f, "Hyper error: {}", msg),
			Error::Serde(ref msg) => write!(f, "Serialization error: {}", msg),
			Error::Database(ref msg) => write!(f, "Database error: {}", msg),
			Error::Internal(ref msg) => write!(f, "Internal error: {}", msg),
		}
	}
}

impl From<serde_json::Error> for Error {
	fn from(err: serde_json::Error) -> Self {
		Error::Serde(err.to_string())
	}
}

impl From<ethkey::Error> for Error {
	fn from(err: ethkey::Error) -> Self {
		Error::Internal(err.into())
	}
}

impl From<kvdb::Error> for Error {
	fn from(err: kvdb::Error) -> Self {
		Error::Database(err.to_string())
	}
}

impl From<key_server_cluster::Error> for Error {
	fn from(err: key_server_cluster::Error) -> Self {
		match err {
			key_server_cluster::Error::ConsensusUnreachable
				| key_server_cluster::Error::AccessDenied => Error::AccessDenied,
			key_server_cluster::Error::MissingKeyShare => Error::DocumentNotFound,
			_ => Error::Internal(err.into()),
		}
	}
}

impl Into<String> for Error {
	fn into(self) -> String {
		format!("{}", self)
	}
}
