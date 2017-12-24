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

const EthereumTx = require('ethereumjs-tx');

const { Middleware } = require('@parity/api/lib/transport');
const { inNumber16 } = require('@parity/api/lib/format/input');
const { randomPhrase } = require('@parity/wordlist');

const accounts = require('./accounts');
const transactions = require('./transactions');
const { phraseToWallet, phraseToAddress, verifySecret } = require('./ethkey');

class LocalAccountsMiddleware extends Middleware {
  constructor (transport) {
    super(transport);

    const NOOP = () => {};
    const register = this.register.bind(this);
    const registerSubscribe = this.registerSubscribe.bind(this);

    register('eth_accounts', () => {
      return accounts.accountAddresses();
    });

    register('eth_coinbase', () => {
      return accounts.lastAddress;
    });

    register('parity_accountsInfo', () => {
      return accounts.map(({ name }) => {
        return { name };
      });
    });

    register('parity_allAccountsInfo', () => {
      return accounts.map(({ name, meta, uuid }) => {
        return { name, meta, uuid };
      });
    });

    register('parity_changePassword', ([address, oldPassword, newPassword]) => {
      const account = accounts.get(address);

      return account
        .decryptPrivateKey(oldPassword)
        .then((privateKey) => {
          if (!privateKey) {
            return false;
          }

          account.changePassword(privateKey, newPassword);

          return true;
        });
    });

    register('parity_checkRequest', ([id]) => {
      return transactions.hash(id) || Promise.resolve(null);
    });

    register('parity_dappsList', () => {
      return [];
    });

    register('parity_defaultAccount', () => {
      return accounts.dappsDefaultAddress;
    });

    registerSubscribe('parity_defaultAccount', (_, callback) => {
      callback(null, accounts.dappsDefaultAddress);

      accounts.on('dappsDefaultAddressChange', (address) => {
        callback(null, accounts.dappsDefaultAddress);
      });
    });

    register('parity_exportAccount', ([address, password]) => {
      const account = accounts.get(address);

      if (!password) {
        password = '';
      }

      return account.isValidPassword(password)
        .then((isValid) => {
          if (!isValid) {
            throw new Error('Invalid password');
          }

          return account.export();
        });
    });

    register('parity_generateSecretPhrase', () => {
      return randomPhrase(12);
    });

    register('parity_getNewDappsAddresses', () => {
      return accounts.accountAddresses();
    });

    register('parity_getNewDappsDefaultAddress', () => {
      return accounts.dappsDefaultAddress;
    });

    register('parity_hardwareAccountsInfo', () => {
      return {};
    });

    registerSubscribe('parity_hardwareAccountsInfo', NOOP);

    register('parity_newAccountFromPhrase', ([phrase, password]) => {
      return phraseToWallet(phrase)
        .then((wallet) => {
          return accounts.create(wallet.secret, password);
        });
    });

    register('parity_newAccountFromSecret', ([secret, password]) => {
      return verifySecret(secret)
        .then((isValid) => {
          if (!isValid) {
            throw new Error('Invalid secret key');
          }

          return accounts.create(secret, password);
        });
    });

    register('parity_newAccountFromWallet', ([json, password]) => {
      if (!password) {
        password = '';
      }

      return accounts.restoreFromWallet(JSON.parse(json), password);
    });

    register('parity_setAccountMeta', ([address, meta]) => {
      accounts.getLazyCreate(address).meta = meta;

      return true;
    });

    register('parity_setAccountName', ([address, name]) => {
      accounts.getLazyCreate(address).name = name;

      return true;
    });

    register('parity_setNewDappsDefaultAddress', ([address]) => {
      accounts.dappsDefaultAddress = address;

      return true;
    });

    register('parity_postTransaction', ([tx]) => {
      if (!tx.from) {
        tx.from = accounts.lastAddress;
      }

      tx.nonce = null;
      tx.condition = null;

      return transactions.add(tx);
    });

    register('parity_phraseToAddress', ([phrase]) => {
      return phraseToAddress(phrase);
    });

    register('parity_useLocalAccounts', () => {
      return true;
    });

    register('parity_listGethAccounts', () => {
      return [];
    });

    register('parity_listOpenedVaults', () => {
      return [];
    });

    register('parity_listRecentDapps', () => {
      return {};
    });

    register('parity_listVaults', () => {
      return [];
    });

    register('parity_lockedHardwareAccountsInfo', () => {
      return [];
    });

    register('parity_hashContent', () => {
      throw new Error('Functionality unavailable on a public wallet.');
    });

    register('parity_killAccount', ([address, password]) => {
      return accounts.remove(address, password);
    });

    register('parity_removeAddress', ([address]) => {
      return accounts.remove(address, null);
    });

    register('parity_testPassword', ([address, password]) => {
      const account = accounts.get(address);

      return account.isValidPassword(password);
    });

    register('parity_upgradeReady', () => {
      return false;
    });

    register('signer_confirmRequest', ([id, modify, password]) => {
      const {
        gasPrice,
        gas: gasLimit,
        from,
        to,
        value,
        data
      } = Object.assign(transactions.get(id), modify);

      transactions.lock(id);

      const account = accounts.get(from);

      return Promise
        .all([
          this.rpcRequest('parity_nextNonce', [from]),
          account.decryptPrivateKey(password)
        ])
        .catch((err) => {
          transactions.unlock(id);

          // transaction got unlocked, can propagate rejection further
          throw err;
        })
        .then(([nonce, privateKey]) => {
          if (!privateKey) {
            transactions.unlock(id);

            throw new Error('Invalid password');
          }

          const tx = new EthereumTx({
            nonce,
            to,
            data,
            gasLimit: inNumber16(gasLimit),
            gasPrice: inNumber16(gasPrice),
            value: inNumber16(value)
          });

          tx.sign(privateKey);

          const serializedTx = `0x${tx.serialize().toString('hex')}`;

          return this.rpcRequest('eth_sendRawTransaction', [serializedTx]);
        })
        .then((hash) => {
          transactions.confirm(id, hash);

          return {};
        });
    });

    register('signer_generateAuthorizationToken', () => {
      return '';
    });

    register('signer_rejectRequest', ([id]) => {
      return transactions.reject(id);
    });

    register('signer_requestsToConfirm', () => {
      return transactions.requestsToConfirm();
    });

    registerSubscribe('signer_subscribePending', (_, callback) => {
      callback(null, transactions.requestsToConfirm());

      transactions.on('update', () => {
        callback(null, transactions.requestsToConfirm());
      });

      return false;
    });
  }
}

module.exports = LocalAccountsMiddleware;
