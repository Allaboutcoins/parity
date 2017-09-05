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

import { flatten, range } from 'lodash';
import BigNumber from 'bignumber.js';

import { hashToImageUrl } from '~/redux/util';
import { sha3 } from '~/api/util/sha3';
import imagesEthereum from '~/../assets/images/contracts/ethereum-black-64x64.png';
import { tokensBalances as tokensBalancesBytecode } from './bytecodes';

export const ETH_TOKEN = {
  address: '',
  format: new BigNumber(10).pow(18),
  id: sha3('eth_native_token').slice(0, 10),
  image: imagesEthereum,
  name: 'Ethereum',
  native: true,
  tag: 'ETH'
};

export function fetchTokenIds (tokenregInstance) {
  return tokenregInstance.tokenCount
    .call()
    .then((numTokens) => {
      const tokenIndexes = range(numTokens.toNumber());

      return tokenIndexes;
    });
}

export function fetchTokensInfo (api, tokenReg, tokenIndexes) {
  const requests = tokenIndexes.map((tokenIndex) => {
    const tokenCalldata = tokenReg.getCallData(tokenReg.instance.token, {}, [tokenIndex]);
    const metaCalldata = tokenReg.getCallData(tokenReg.instance.meta, {}, [tokenIndex, 'IMG']);

    return [
      { to: tokenReg.address, data: tokenCalldata },
      { to: tokenReg.address, data: metaCalldata }
    ];
  });

  return api.parity.call(flatten(requests))
    .then((results) => {
      return tokenIndexes.map((tokenIndex, index) => {
        const [ rawTokenData, rawImage ] = results.slice(index * 2, index * 2 + 2);

        const tokenData = tokenReg.instance.token
          .decodeOutput(rawTokenData)
          .map((t) => t.value);

        const image = tokenReg.instance.meta.decodeOutput(rawImage)[0].value;

        const [ address, tag, format, name ] = tokenData;

        const token = {
          format: format.toString(),
          index: tokenIndex,
          image: hashToImageUrl(image),
          id: sha3(address + tokenIndex).slice(0, 10),
          address,
          name,
          tag
        };

        return token;
      });
    });
}

/**
 * `updates` should be in the shape:
 *   {
 *     [ who ]: [ tokenId ]  // Array of tokens to updates
 *   }
 *
 * Returns a Promise resolved with the balances in the shape:
 *   {
 *     [ who ]: { [ tokenId ]: BigNumber } // The balances of `who`
 *   }
 */
export function fetchAccountsBalances (api, tokens, updates) {
  const accountAddresses = Object.keys(updates);

  // Updates for the ETH balances
  const ethUpdates = accountAddresses
    .map((accountAddress) => {
      return updates[accountAddress].filter((tokenId) => tokenId === ETH_TOKEN.id);
    })
    .reduce((nextUpdates, tokenIds, accountIndex) => {
      if (tokenIds.length > 0) {
        const accountAddress = accountAddresses[accountIndex];

        nextUpdates[accountAddress] = tokenIds;
      }

      return nextUpdates;
    }, {});

  // Updates for Tokens balances
  const tokenUpdates = Object.keys(updates)
    .map((accountAddress) => {
      return updates[accountAddress].filter((tokenId) => tokenId !== ETH_TOKEN.id);
    })
    .reduce((nextUpdates, tokenIds, accountIndex) => {
      if (tokenIds.length > 0) {
        const accountAddress = accountAddresses[accountIndex];

        nextUpdates[accountAddress] = tokenIds;
      }

      return nextUpdates;
    }, {});

  let ethBalances = {};
  let tokensBalances = {};

  const ethPromise = fetchEthBalances(api, Object.keys(ethUpdates))
    .then((_ethBalances) => {
      ethBalances = _ethBalances;
    });

  let tokenPromise = Promise.resolve();

  Object.keys(tokenUpdates)
    .forEach((accountAddress) => {
      const tokenIds = tokenUpdates[accountAddress];
      const updateTokens = tokens
        .filter((t) => tokenIds.includes(t.id));

      tokenPromise = tokenPromise
        .then(() => fetchTokensBalances(api, updateTokens, [ accountAddress ]))
        .then((balances) => {
          tokensBalances[accountAddress] = balances[accountAddress];
        });
    });

  return ethPromise
    .then(() => tokenPromise)
    .then(() => {
      const balances = Object.assign({}, tokensBalances);

      Object.keys(ethBalances).forEach((accountAddress) => {
        if (!balances[accountAddress]) {
          balances[accountAddress] = {};
        }

        balances[accountAddress] = Object.assign(
          {},
          balances[accountAddress],
          ethBalances[accountAddress]
        );
      });

      return balances;
    });
}

function fetchEthBalances (api, accountAddresses) {
  const promises = accountAddresses
    .map((accountAddress) => api.eth.getBalance(accountAddress));

  return Promise.all(promises)
    .then((balancesArray) => {
      return balancesArray.reduce((balances, balance, index) => {
        balances[accountAddresses[index]] = {
          [ETH_TOKEN.id]: balance
        };

        return balances;
      }, {});
    });
}

function fetchTokensBalances (api, tokens, accountAddresses) {
  const tokenAddresses = tokens.map((t) => t.address);
  const tokensBalancesCallData = encode(
    api,
    [ 'address[]', 'address[]' ],
    [ accountAddresses, tokenAddresses ]
  );

  return api.eth
    .call({ data: tokensBalancesBytecode + tokensBalancesCallData })
    .then((result) => {
      const rawBalances = decodeArray(api, 'uint[]', result);
      const balances = {};

      accountAddresses.forEach((accountAddress, accountIndex) => {
        const balance = {};
        const preIndex = accountIndex * tokenAddresses.length;

        tokenAddresses.forEach((tokenAddress, tokenIndex) => {
          const index = preIndex + tokenIndex;
          const token = tokens[tokenIndex];

          balance[token.id] = rawBalances[index];
        });

        balances[accountAddress] = balance;
      });

      return balances;
    });
}

function encode (api, types, values) {
  return api.util.abiEncode(
    null,
    types,
    values
  ).replace('0x', '');
}

function decodeArray (api, type, data) {
  return api.util
    .abiDecode(
      [type],
      [
        '0x',
        (32).toString(16).padStart(64, 0),
        data.replace('0x', '')
      ].join('')
    )[0]
    .map((t) => t.value);
}
