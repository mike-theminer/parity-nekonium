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

//! Hash-addressed content resolver & fetcher.

#![warn(missing_docs)]

#[macro_use]
extern crate log;

extern crate ethcore_util as util;
extern crate ethcore_bigint as bigint;
extern crate ethcore_bytes as bytes;
extern crate futures;
extern crate keccak_hash as hash;
extern crate mime;
extern crate mime_guess;
extern crate native_contracts;
extern crate parity_reactor;
extern crate rand;
extern crate rustc_hex;

pub extern crate fetch;

#[cfg(test)]
extern crate parking_lot;
#[cfg(test)]
extern crate ethabi;

mod client;

pub mod urlhint;

pub use client::{HashFetch, Client, Error};
