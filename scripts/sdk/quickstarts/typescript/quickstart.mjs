// SPDX-License-Identifier: Apache-2.0

import { isAbsolute, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

import { XenoteerClient, XenoteerError } from '@xenoteer/sdk';

function required(name) {
  const value = process.env[name];
  if (typeof value !== 'string' || value.length === 0)
    throw new Error(`required environment is missing: ${name}`);

  return value;
}

function verifyInstalledOrigin() {
  const expected = resolve(required('XENOTEER_EXPECTED_INSTALL_ROOT'));
  const installed = resolve(fileURLToPath(import.meta.resolve('@xenoteer/sdk')));
  const within = relative(expected, installed);
  if (within === '' || (!within.startsWith('..') && !isAbsolute(within)))
    return;

  throw new Error('TypeScript SDK resolved outside the staged npm installation');
}

async function exercise() {
  verifyInstalledOrigin();
  const language = required('XENOTEER_QUICKSTART_LANGUAGE');
  if (!/^[a-z-]+$/u.test(language))
    throw new Error('quick-start language label is invalid');

  const expectAuthenticationFailure = required('XENOTEER_EXPECT_AUTH_FAILURE') === '1';
  let client;
  try {
    client = await XenoteerClient.connect({
      baseUrl: required('XENOTEER_API_BASE'),
      token: required('XENOTEER_TOKEN'),
      requestTimeoutMs: 5_000,
    });
  } catch (error) {
    if (
      expectAuthenticationFailure
      && error instanceof XenoteerError
      && error.code === 'authentication'
      && error.status === 401
    ) {
      console.log(`quickstart-ok language=${language} mode=auth-failure`);
      return;
    }
    throw error;
  }

  try {
    if (expectAuthenticationFailure)
      throw new Error('invalid bearer unexpectedly authenticated');

    if (
      client.negotiatedProtocol.major !== 1
      || client.negotiatedProtocol.minor !== 0
    )
      throw new Error('server did not negotiate frozen protocol v1.0');

    if (client.status.desktop.state !== 'ready')
      throw new Error('desktop was not ready');

    client.desktop();
    console.log(`quickstart-ok language=${language} mode=success`);
  } finally {
    await client.close();
  }
}

try {
  await exercise();
} catch (error) {
  const detail = error instanceof XenoteerError
    ? `${error.name}[${error.code}]`
    : error instanceof Error
      ? error.message
      : 'unknown safe failure';
  console.error(`public TypeScript quick-start failed: ${detail}`);
  process.exitCode = 1;
}
