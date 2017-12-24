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

//! Cross-platform open url in default browser

#[cfg(windows)]
mod shell {
	extern crate winapi;

	use self::winapi::*;
	extern "system" {
		pub fn ShellExecuteA(
			hwnd: HWND, lpOperation: LPCSTR, lpFile: LPCSTR, lpParameters: LPCSTR, lpDirectory: LPCSTR,
			nShowCmd: c_int
		) -> HINSTANCE;
	}

	pub use self::winapi::SW_SHOWNORMAL as Normal;
}

#[cfg(windows)]
pub fn open(url: &str) {
	use std::ffi::CString;
	use std::ptr;

	unsafe {
		shell::ShellExecuteA(ptr::null_mut(),
			CString::new("open").unwrap().as_ptr(),
			CString::new(url.to_owned().replace("\n", "%0A")).unwrap().as_ptr(),
			ptr::null(),
			ptr::null(),
			shell::Normal);
	}
}

#[cfg(any(target_os="macos", target_os="freebsd"))]
pub fn open(url: &str) {
	use std;
	let _ = std::process::Command::new("open").arg(url).spawn();
}

#[cfg(target_os="linux")]
pub fn open(url: &str) {
	use std;
	let _ = std::process::Command::new("xdg-open").arg(url).spawn();
}
