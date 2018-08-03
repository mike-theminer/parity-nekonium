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

//! Encryption schemes supported by RPC layer.

use ethereum_types::H256;
use ethkey::{self, Public, Secret};
use ring::aead::{self, AES_256_GCM, SealingKey, OpeningKey};

/// Length of AES key
pub const AES_KEY_LEN: usize = 32;
/// Length of AES nonce (IV)
pub const AES_NONCE_LEN: usize = 12;

// nonce used for encryption when broadcasting
const BROADCAST_IV: [u8; AES_NONCE_LEN] = [0xff; AES_NONCE_LEN];

// how to encode aes key/nonce.
enum AesEncode {
	AppendedNonce, // receiver known, random nonce appended.
	OnTopics(Vec<H256>), // receiver knows topics but not key. nonce global.
}

enum EncryptionInner {
	AES([u8; AES_KEY_LEN], [u8; AES_NONCE_LEN], AesEncode),
	ECIES(Public),
}

/// Encryption good for single usage.
pub struct EncryptionInstance(EncryptionInner);

impl EncryptionInstance {
	/// ECIES encryption using public key. Fails if invalid public key.
	pub fn ecies(public: Public) -> Result<Self, &'static str> {
		if !ethkey::public_is_valid(&public) {
			return Err("Invalid public key");
		}

		Ok(EncryptionInstance(EncryptionInner::ECIES(public)))
	}

	/// 256-bit AES GCM encryption with given nonce.
	/// It is extremely insecure to reuse nonces.
	///
	/// If generating nonces with a secure RNG, limit uses such that
	/// the chance of collision is negligible.
	pub fn aes(key: [u8; AES_KEY_LEN], nonce: [u8; AES_NONCE_LEN]) -> Self {
		EncryptionInstance(EncryptionInner::AES(key, nonce, AesEncode::AppendedNonce))
	}

	/// Broadcast encryption for the message based on the given topics.
	///
	/// Key reuse here is extremely dangerous. It should be randomly generated
	/// with a secure RNG.
	pub fn broadcast(key: [u8; AES_KEY_LEN], topics: Vec<H256>) -> Self {
		EncryptionInstance(EncryptionInner::AES(key, BROADCAST_IV, AesEncode::OnTopics(topics)))
	}

	/// Encrypt the supplied plaintext
	pub fn encrypt(self, plain: &[u8]) -> Vec<u8> {
		match self.0 {
			EncryptionInner::AES(key, nonce, encode) => {
				let sealing_key = SealingKey::new(&AES_256_GCM, &key)
					.expect("key is of correct len; qed");

				let encrypt_plain = move |buf: &mut Vec<u8>| {
					let out_suffix_capacity = AES_256_GCM.tag_len();

					let prepend_len = buf.len();
					buf.extend(plain);

					buf.resize(prepend_len + plain.len() + out_suffix_capacity, 0);

					let out_size = aead::seal_in_place(
						&sealing_key,
						&nonce,
						&[], // no authenticated data.
						&mut buf[prepend_len..],
						out_suffix_capacity,
					).expect("key, nonce, buf are valid and out suffix large enough; qed");

					// truncate to the output size and return.
					buf.truncate(prepend_len + out_size);
				};

				match encode {
					AesEncode::AppendedNonce => {
						let mut buf = Vec::new();
						encrypt_plain(&mut buf);
						buf.extend(&nonce[..]);
						buf
					}
					AesEncode::OnTopics(topics) => {
						let mut buf = Vec::new();
						let key = H256(key);

						for topic in topics {
							buf.extend(&*(topic ^ key));
						}

						encrypt_plain(&mut buf);
						buf
					}
				}
			}
			EncryptionInner::ECIES(valid_public) => {
				::ethcrypto::ecies::encrypt(&valid_public, &[], plain)
					.expect("validity of public key an invariant of the type; qed")
			}
		}
	}
}

enum AesExtract {
	AppendedNonce([u8; AES_KEY_LEN]), // extract appended nonce.
	OnTopics(usize, usize, H256), // number of topics, index we know, topic we know.
}

enum DecryptionInner {
	AES(AesExtract),
	ECIES(Secret),
}

/// Decryption instance good for single usage.
pub struct DecryptionInstance(DecryptionInner);

impl DecryptionInstance {
	/// ECIES decryption using secret key. Fails if invalid secret.
	pub fn ecies(secret: Secret) -> Result<Self, &'static str> {
		secret.check_validity().map_err(|_| "Invalid secret key")?;

		Ok(DecryptionInstance(DecryptionInner::ECIES(secret)))
	}

	/// 256-bit AES GCM decryption with appended nonce.
	pub fn aes(key: [u8; AES_KEY_LEN]) -> Self {
		DecryptionInstance(DecryptionInner::AES(AesExtract::AppendedNonce(key)))
	}

	/// Decode broadcast based on number of topics and known topic.
	/// Known topic index may not be larger than num topics - 1.
	pub fn broadcast(num_topics: usize, topic_idx: usize, known_topic: H256) -> Result<Self, &'static str> {
		if topic_idx >= num_topics { return Err("topic index out of bounds") }

		Ok(DecryptionInstance(DecryptionInner::AES(AesExtract::OnTopics(num_topics, topic_idx, known_topic))))
	}

	/// Decrypt ciphertext. Fails if it's an invalid message.
	pub fn decrypt(self, ciphertext: &[u8]) -> Option<Vec<u8>> {
		match self.0 {
			DecryptionInner::AES(extract) => {
				let decrypt = |
					key: [u8; AES_KEY_LEN],
					nonce: [u8; AES_NONCE_LEN],
					ciphertext: &[u8]
				| {
					if ciphertext.len() < AES_256_GCM.tag_len() { return None }

					let opening_key = OpeningKey::new(&AES_256_GCM, &key)
						.expect("key length is valid for mode; qed");

					let mut buf = ciphertext.to_vec();

					// decrypted plaintext always ends up at the
					// front of the buffer.
					let maybe_decrypted = aead::open_in_place(
						&opening_key,
						&nonce,
						&[], // no authenticated data
						0, // no header.
						&mut buf,
					).ok().map(|plain_slice| plain_slice.len());

					maybe_decrypted.map(move |len| { buf.truncate(len); buf })
				};

				match extract {
					AesExtract::AppendedNonce(key) => {
						if ciphertext.len() < AES_NONCE_LEN { return None }

						// nonce is the suffix of ciphertext.
						let mut nonce = [0; AES_NONCE_LEN];
						let nonce_offset = ciphertext.len() - AES_NONCE_LEN;

						nonce.copy_from_slice(&ciphertext[nonce_offset..]);
						decrypt(key, nonce, &ciphertext[..nonce_offset])
					}
					AesExtract::OnTopics(num_topics, known_index, known_topic) => {
						if ciphertext.len() < num_topics * 32 { return None }

						let mut salted_topic = H256::new();
						salted_topic.copy_from_slice(&ciphertext[(known_index * 32)..][..32]);

						let key = (salted_topic ^ known_topic).0;

						let offset = num_topics * 32;
						decrypt(key, BROADCAST_IV, &ciphertext[offset..])
					}
				}
			}
			DecryptionInner::ECIES(secret) => {
				// secret is checked for validity, so only fails on invalid message.
				::ethcrypto::ecies::decrypt(&secret, &[], ciphertext).ok()
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn aes_key_len_should_be_equal_to_constant() {
		assert_eq!(::ring::aead::AES_256_GCM.key_len(), AES_KEY_LEN);
	}

	#[test]
	fn aes_nonce_len_should_be_equal_to_constant() {
		assert_eq!(::ring::aead::AES_256_GCM.nonce_len(), AES_NONCE_LEN);
	}

	#[test]
	fn encrypt_asymmetric() {
		use ethkey::{Generator, Random};

		let key_pair = Random.generate().unwrap();
		let test_message = move |message: &[u8]| {
			let instance = EncryptionInstance::ecies(key_pair.public().clone()).unwrap();
			let ciphertext = instance.encrypt(&message);

			if !message.is_empty() {
				assert!(&ciphertext[..message.len()] != message)
			}

			let instance = DecryptionInstance::ecies(key_pair.secret().clone()).unwrap();
			let decrypted = instance.decrypt(&ciphertext).unwrap();

			assert_eq!(message, &decrypted[..])
		};

		test_message(&[1, 2, 3, 4, 5]);
		test_message(&[]);
		test_message(&[255; 512]);
	}

	#[test]
	fn encrypt_symmetric() {
		use rand::{Rng, OsRng};

		let mut rng = OsRng::new().unwrap();
		let mut test_message = move |message: &[u8]| {
			let key = rng.gen();

			let instance = EncryptionInstance::aes(key, rng.gen());
			let ciphertext = instance.encrypt(message);

			if !message.is_empty() {
				assert!(&ciphertext[..message.len()] != message)
			}

			let instance = DecryptionInstance::aes(key);
			let decrypted = instance.decrypt(&ciphertext).unwrap();

			assert_eq!(message, &decrypted[..])
		};

		test_message(&[1, 2, 3, 4, 5]);
		test_message(&[]);
		test_message(&[255; 512]);
	}

	#[test]
	fn encrypt_broadcast() {
		use rand::{Rng, OsRng};

		let mut rng = OsRng::new().unwrap();

		let mut test_message = move |message: &[u8]| {
			let all_topics = (0..5).map(|_| rng.gen()).collect::<Vec<_>>();
			let known_idx = 2;
			let known_topic = all_topics[2];
			let key = rng.gen();

			let instance = EncryptionInstance::broadcast(key, all_topics);
			let ciphertext = instance.encrypt(message);

			if !message.is_empty() {
				assert!(&ciphertext[..message.len()] != message)
			}

			let instance = DecryptionInstance::broadcast(5, known_idx, known_topic).unwrap();

			let decrypted = instance.decrypt(&ciphertext).unwrap();

			assert_eq!(message, &decrypted[..])
		};

		test_message(&[1, 2, 3, 4, 5]);
		test_message(&[]);
		test_message(&[255; 512]);
	}
}
