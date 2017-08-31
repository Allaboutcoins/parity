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

import { uniq, chunk } from 'lodash';

import Contracts from '~/contracts';
import { LOG_KEYS, getLogger } from '~/config';
import { fetchTokenIds, fetchTokensInfo } from '~/util/tokens';

import { updateTokensFilter } from './balancesActions';
import { setAddressImage } from './imagesActions';

const log = getLogger(LOG_KEYS.Balances);

export function setTokens (tokens) {
  return {
    type: 'setTokens',
    tokens
  };
}

export function loadTokens (options = {}) {
  log.debug('loading tokens', Object.keys(options).length ? options : '');

  return (dispatch, getState) => {
    const { tokenReg } = Contracts.get();

    tokenReg.getInstance()
      .then((tokenRegInstance) => {
        return fetchTokenIds(tokenRegInstance);
      })
      .then((tokenIndexes) => dispatch(fetchTokens(tokenIndexes, options)))
      .catch((error) => {
        console.warn('tokens::loadTokens', error);
      });
  };
}

export function fetchTokens (_tokenIndexes, options = {}) {
  const tokenIndexes = uniq(_tokenIndexes || []);
  const tokenChunks = chunk(tokenIndexes, 64);

  return (dispatch, getState) => {
    const { api, images } = getState();
    const { tokenReg } = Contracts.get();

    const processChunk = (chunk) => {
      if (!chunk || chunk.length === 0) {
        return Promise.resolve(true);
      }

      let tokens = {};

      return tokenReg.getContract()
        .then((tokenReg) => {
          return fetchTokensInfo(api, tokenReg, chunk);
        })
        .then((results) => {
          tokens = results
            .filter((token) => {
              return token.name && token.address && !/^(0x)?0*$/.test(token.address);
            })
            .reduce((tokens, token) => {
              const { id, image, address } = token;

              // dispatch only the changed images
              if (images[address] !== image) {
                dispatch(setAddressImage(address, image, true));
              }

              tokens[id] = token;
              return tokens;
            }, tokens);

          log.debug('fetched token', tokens);

          dispatch(setTokens(tokens));
          dispatch(updateTokensFilter(null, null, options));
        })
        .catch((error) => {
          console.warn('tokens::fetchTokens', error);
        });
    };
    let promise = Promise.resolve(true);

    while (tokenChunks.length) {
      const chunk = tokenChunks.shift();

      promise = promise.then(() => processChunk(chunk));
    }

    return promise;
  };
}
