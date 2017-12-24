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

/// Type of EVM to use.
#[derive(Debug, PartialEq, Clone)]
pub enum VMType {
	/// JIT EVM
	#[cfg(feature = "jit")]
	Jit,
	/// RUST EVM
	Interpreter
}

impl fmt::Display for VMType {
	#[cfg(feature="jit")]
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		write!(f, "{}", match *self {
			VMType::Jit => "JIT",
			VMType::Interpreter => "INT"
		})
	}
	#[cfg(not(feature="jit"))]
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		write!(f, "{}", match *self {
			VMType::Interpreter => "INT"
		})
	}
}

impl Default for VMType {
	fn default() -> Self {
		VMType::Interpreter
	}
}

impl VMType {
	/// Return all possible VMs (JIT, Interpreter)
	#[cfg(feature = "jit")]
	pub fn all() -> Vec<VMType> {
		vec![VMType::Jit, VMType::Interpreter]
	}

	/// Return all possible VMs (Interpreter)
	#[cfg(not(feature = "jit"))]
	pub fn all() -> Vec<VMType> {
		vec![VMType::Interpreter]
	}

	/// Return new jit if it's possible
	#[cfg(not(feature = "jit"))]
	pub fn jit() -> Option<Self> {
		None
	}

	/// Return new jit if it's possible
	#[cfg(feature = "jit")]
	pub fn jit() -> Option<Self> {
		Some(VMType::Jit)
	}
}
