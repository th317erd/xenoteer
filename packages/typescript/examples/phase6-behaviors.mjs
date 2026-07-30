#!/usr/bin/env node
// SPDX-License-Identifier: Apache-2.0
//
// Executable installed-package qualification for the ten Phase 6 SDK
// behaviors. The artifact gate launches the GTK fixture from the exact derived
// image and supplies the four required XENOTEER_* connection variables.

import { createHash } from 'node:crypto';
import { isAbsolute, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

import { XenoteerClient, XenoteerError } from '../dist/src/index.js';

const BEHAVIORS = Object.freeze([
  'status-capabilities',
  'scoped-lease-fixture-launch',
  'exact-window-element',
  'semantic-invoke',
  'smooth-physical-click-postcondition',
  'unicode-text-strategy',
  'screenshot-on-failure',
  'reconnect-known-command',
  'stale-reference-restart',
  'view-only-browser-ticket',
]);
const GTK_TITLE = 'Xenoteer GTK3 Fixture — Main';
const XMESSAGE_TITLE = 'xmessage';
const VIEWER_ORIGIN = 'https://viewer.example';
const UNICODE_TEXT = 'Xenoteer — العربية — 中文 — e\u0301 — 😀';
const TRANSPORT_REQUEST_TIMEOUT_MILLISECONDS = 35_000;
const SERVER_LONG_POLL_TIMEOUT_MILLISECONDS = 30_000;
// JavaScript promises do not provide structured cancellation. A Promise.race
// would report a timeout while an in-flight mutation or cleanup kept running.
// The package gate therefore owns the honest whole-process deadline.
const EXTERNAL_PROCESS_TIMEOUT_MILLISECONDS = 120_000;

function required(name) {
  const value = process.env[name];
  if (typeof value !== 'string' || value.length === 0)
    throw new Error(`required environment is missing: ${name}`);
  return value;
}

function requireCondition(condition, message) {
  if (!condition) throw new Error(message);
}

function verifyInstalledOrigin() {
  const expected = resolve(required('XENOTEER_EXPECTED_INSTALL_ROOT'));
  const installed = resolve(fileURLToPath(new URL('../dist/src/index.js', import.meta.url)));
  const within = relative(expected, installed);
  requireCondition(
    within === '' || (!within.startsWith('..') && !isAbsolute(within)),
    'TypeScript SDK resolved outside the staged npm installation',
  );
}

function marker(language, behavior) {
  console.log(`quickstart-ok language=${language} behavior=${behavior}`);
}

function windowSelector(title) {
  return {
    type: 'predicate',
    predicate: {
      type: 'text',
      field: 'title',
      matcher: { type: 'exact', value: title, case_sensitive: true },
    },
  };
}

function elementSelector(name) {
  return {
    scope: { type: 'desktop' },
    predicates: [
      {
        type: 'name',
        matcher: { type: 'exact', value: name, case_sensitive: true },
      },
    ],
    order: 'object_path_ascending',
    result_index: null,
  };
}

function accessibilitySpec(component = false) {
  return {
    expansion: {
      actions: false,
      value: false,
      text_metadata: false,
      text_content: false,
      attributes: false,
      relations: false,
      component,
    },
    limits: {
      max_visited_nodes: 25_000,
      max_depth: 64,
      max_matches: 10,
      timeout_ms: 10_000,
    },
  };
}

async function waitElement(desktop, name) {
  const selector = elementSelector(name);
  await desktop.accessibility.wait({
    target: { type: 'selector', selector, quantifier: 'exactly_one' },
    predicate: { type: 'exists' },
    after_revision: null,
    timeout_ms: SERVER_LONG_POLL_TIMEOUT_MILLISECONDS,
    allow_poll_fallback: true,
    ...accessibilitySpec(false),
  });
  return await desktop.accessibility.one(selector, accessibilitySpec(true));
}

async function waitWindow(desktop, title) {
  const selector = windowSelector(title);
  await desktop.windows.wait({
    target: { type: 'selector', selector, quantifier: 'exactly_one' },
    predicate: { type: 'exists' },
    after_revision: null,
    timeout_ms: SERVER_LONG_POLL_TIMEOUT_MILLISECONDS,
  });
  return await desktop.windows.one(selector, 'creation_ascending');
}

async function terminal(handle, label) {
  const result = await handle.waitUntilTerminal(20_000);
  requireCondition(result.lifecycle === 'succeeded', `${label} did not succeed`);
  return result;
}

function outcome(result, kind) {
  const value = result.outcome;
  requireCondition(
    value !== null && typeof value === 'object' && value.type === kind,
    `command omitted its ${kind} outcome`,
  );
  return value;
}

async function launchXmessage(desktop, message) {
  const result = await terminal(
    await desktop.applications.launch(
      'xmessage',
      [message],
    ),
    'xmessage launch',
  );
  const processRef = outcome(result, 'application_launched').process;
  requireCondition(
    processRef !== null && typeof processRef === 'object',
    'launch outcome omitted its exact process',
  );
  return processRef;
}

async function terminateProcess(desktop, processRef) {
  const result = await terminal(
    await desktop.applications.terminate(processRef, 2_000),
    'fixture termination',
  );
  const terminated = outcome(result, 'process_terminated').process;
  requireCondition(
    terminated !== null && typeof terminated === 'object' && terminated.state === 'exited',
    'fixture termination did not reap the exact process',
  );
}

async function exerciseConnected(client, language) {
  const desktop = client.desktop();
  const capabilityEntries = client.status.capabilities?.capabilities;
  requireCondition(Array.isArray(capabilityEntries), 'status omitted capabilities');
  const available = new Set(
    capabilityEntries
      .filter((entry) => entry?.status === 'available')
      .map((entry) => entry.id),
  );
  for (const capability of [
    'accessibility.atspi',
    'capture.screenshot',
    'input.pointer.smooth',
    'process.managed.terminate',
    'viewer.novnc.view_only',
    'window.observe.wait',
  ]) {
    requireCondition(
      available.has(capability),
      `fixture capability is unavailable: ${capability}`,
    );
  }
  marker(language, BEHAVIORS[0]);

  await desktop.withControl(60_000, async (lease) => {
    await exerciseScoped(desktop, language, lease);
  });
}

async function exerciseScoped(desktop, language, lease) {
  const message = `Xenoteer SDK Phase 6 — ${language}`;
  let processRef;
  let screenshotArtifact;
  let operationFailed = false;
  let operationFailure;
  try {
    processRef = await launchXmessage(desktop, message);
    marker(language, BEHAVIORS[1]);

    const xmessageWindow = await waitWindow(desktop, XMESSAGE_TITLE);
    const gtkWindow = await waitWindow(desktop, GTK_TITLE);
    const button = await waitElement(desktop, 'Stable Button');
    const entry = await waitElement(desktop, 'Stable Entry');
    requireCondition(
      JSON.stringify(xmessageWindow.identity) !== JSON.stringify(gtkWindow.identity),
      'exact windows aliased',
    );
    requireCondition(
      JSON.stringify(button.identity) !== JSON.stringify(entry.identity),
      'exact elements aliased',
    );
    const correlated = await xmessageWindow.snapshot();
    requireCondition(
      JSON.stringify(correlated.window.snapshot.process.managed_process)
        === JSON.stringify(processRef),
      'exact xmessage window did not correlate to the launched process',
    );
    marker(language, BEHAVIORS[2]);

    const invokeResult = await terminal(
      await button.invoke({ type: 'default' }),
      'semantic invoke',
    );
    const invokeEvidence = outcome(invokeResult, 'element_action').result;
    requireCondition(
      invokeEvidence?.operation === 'invoke'
        && JSON.stringify(invokeEvidence.element) === JSON.stringify(button.identity)
        && invokeEvidence.evidence?.backend_accepted === true,
      'semantic invoke omitted exact-target actor-owned evidence',
    );
    await waitElement(desktop, 'Activation Count 1');
    marker(language, BEHAVIORS[3]);

    const clickResult = await terminal(
      await button.click(gtkWindow.identity, {
        leaseId: lease.id,
        moveDurationMs: 250,
        postcondition: {
          predicate: { type: 'exists' },
          timeout_ms: 3_000,
          allow_poll_fallback: true,
        },
      }),
      'smooth physical click',
    );
    requireCondition(
      outcome(clickResult, 'element_physical_click').result?.pointer_interpolated === true,
      'physical click did not report interpolated pointer motion',
    );
    await waitElement(desktop, 'Activation Count 2');
    marker(language, BEHAVIORS[4]);

    await terminal(await entry.setText(''), 'entry reset');
    const textResult = await terminal(
      await lease.keyboard.insertText(
        UNICODE_TEXT,
        {
          target: 'element',
          element: entry.identity,
          window_fallback: null,
        },
        {
          strategy: 'auto',
          autoPolicy: {
            allowed_strategies: ['semantic'],
            fallback: 'before_effect_only',
          },
          semanticOptions: {
            insertion_point: { kind: 'caret' },
            selection: 'collapse_after',
            verify_length_only: false,
            postcondition: null,
          },
          clipboardOptions: null,
        },
      ),
      'Unicode insertion',
    );
    const textEvidence = outcome(textResult, 'text_inserted').evidence;
    requireCondition(
      textEvidence?.selected_strategy === 'semantic'
        && textEvidence.utf8_bytes === new TextEncoder().encode(UNICODE_TEXT).byteLength
        && textEvidence.unicode_scalars === [...UNICODE_TEXT].length
        && textEvidence.completed_scalars === [...UNICODE_TEXT].length
        && textEvidence.verified_length_only === false,
      'Unicode insertion omitted exact delivery and strategy evidence',
    );
    marker(language, BEHAVIORS[5]);

    const failed = await button.invoke(
      { type: 'default' },
      {
        postcondition: {
          predicate: { type: 'state', state: 'checked', value: true },
          timeout_ms: 750,
          allow_poll_fallback: true,
        },
      },
    );
    const failedResult = await failed.waitUntilTerminal(10_000);
    requireCondition(
      failedResult.lifecycle === 'failed'
        && ['semantic_action_dispatched', 'semantic_state_changed'].includes(
          failedResult.effect_stage,
        ),
      'deliberately failed postcondition omitted visible-effect evidence',
    );
    await waitElement(desktop, 'Activation Count 3');
    const screenshot = await desktop.capture.screenshot({
      target: {
        kind: 'window_visible',
        window: gtkWindow.identity,
        coordinate_space: 'frame',
      },
      format: 'png',
      include_cursor: true,
      region: null,
      scale: null,
      max_bytes: 4 * 1_048_576,
    });
    screenshotArtifact = screenshot.artifact;
    requireCondition(
      screenshotArtifact !== undefined,
      'failure screenshot was not retained as a private artifact',
    );
    const screenshotBytes = await screenshotArtifact.download({
      maxBytes: 4 * 1_048_576,
    });
    const digest = createHash('sha256').update(screenshotBytes).digest('hex');
    requireCondition(
      Buffer.from(screenshotBytes.subarray(0, 8)).equals(
        Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
      )
        && digest === screenshotArtifact.ref.sha256
        && digest === screenshot.result.sha256,
      'failure screenshot bytes did not match their artifact evidence',
    );
    await screenshotArtifact.delete();
    screenshotArtifact = undefined;
    marker(language, BEHAVIORS[6]);

    const probe = desktop.prepareSubmission({ type: 'desktop_probe' });
    const knownCommandId = probe.id;
    const probeResult = await terminal(await probe.send(), 'known-ID probe');
    requireCondition(
      outcome(probeResult, 'probe').ready === true,
      'probe did not report ready',
    );
    const reconnect = await XenoteerClient.connect({
      baseUrl: required('XENOTEER_API_BASE'),
      token: required('XENOTEER_TOKEN'),
      requestTimeoutMs: TRANSPORT_REQUEST_TIMEOUT_MILLISECONDS,
    });
    try {
      const recovered = await reconnect.desktop().command(knownCommandId);
      const recoveredResult = await recovered.waitUntilTerminal(10_000);
      requireCondition(
        recoveredResult.command_id === knownCommandId
          && recoveredResult.lifecycle === 'succeeded',
        'reconnect did not recover the known command ID',
      );
    } finally {
      await reconnect.close();
    }
    marker(language, BEHAVIORS[7]);

    const oldIdentity = xmessageWindow.identity;
    await terminateProcess(desktop, processRef);
    processRef = undefined;
    await desktop.windows.wait({
      target: { type: 'reference', window: oldIdentity },
      predicate: { type: 'gone' },
      after_revision: null,
      timeout_ms: 15_000,
    });
    try {
      await xmessageWindow.snapshot();
    } catch (error) {
      requireCondition(
        error instanceof XenoteerError
          && (error.code === 'stale_reference' || error.problemCode === 'stale_reference'),
        'old window failed with a non-stale error',
      );
    }
    requireCondition(xmessageWindow.stale, 'old window reference remained current after restart');
    processRef = await launchXmessage(desktop, message);
    const newWindow = await waitWindow(desktop, XMESSAGE_TITLE);
    requireCondition(
      JSON.stringify(newWindow.identity) !== JSON.stringify(oldIdentity),
      'restart reused the exact window birth',
    );
    marker(language, BEHAVIORS[8]);

    const ticket = await desktop.viewer.issueTicket(VIEWER_ORIGIN);
    const metadata = ticket.toJSON();
    const secret = ticket.consumeSecret();
    requireCondition(
      metadata.origin === VIEWER_ORIGIN
        && metadata.mode === 'view_only'
        && metadata.audience === 'viewer_websocket'
        && metadata.usePolicy === 'single_use'
        && typeof secret === 'string'
        && secret.length >= 32
        && !ticket.toString().includes(secret),
      'browser ticket was not exact-origin, single-use, and view-only',
    );
    marker(language, BEHAVIORS[9]);
  } catch (error) {
    operationFailed = true;
    operationFailure = error;
  } finally {
    const failures = operationFailed ? [operationFailure] : [];
    if (screenshotArtifact !== undefined) {
      try {
        await screenshotArtifact.delete();
      } catch (error) {
        failures.push(error);
      }
    }
    if (processRef !== undefined) {
      try {
        await terminateProcess(desktop, processRef);
      } catch (error) {
        failures.push(error);
      }
    }
    if (failures.length === 1)
      throw failures[0];
    if (failures.length > 1)
      throw new AggregateError(failures, 'behavior execution and resource cleanup failed');
  }
}

async function exercise() {
  verifyInstalledOrigin();
  requireCondition(
    EXTERNAL_PROCESS_TIMEOUT_MILLISECONDS >= 2 * TRANSPORT_REQUEST_TIMEOUT_MILLISECONDS,
    'external process deadline does not cover an operation plus cleanup',
  );
  const language = required('XENOTEER_QUICKSTART_LANGUAGE');
  requireCondition(/^[a-z-]+$/u.test(language), 'quick-start language label is invalid');
  const expectAuthenticationFailure = required('XENOTEER_EXPECT_AUTH_FAILURE') === '1';
  let client;
  try {
    client = await XenoteerClient.connect({
      baseUrl: required('XENOTEER_API_BASE'),
      token: required('XENOTEER_TOKEN'),
      requestTimeoutMs: TRANSPORT_REQUEST_TIMEOUT_MILLISECONDS,
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
    requireCondition(
      !expectAuthenticationFailure,
      'invalid bearer unexpectedly authenticated',
    );
    requireCondition(
      client.negotiatedProtocol.major === 1
        && client.negotiatedProtocol.minor === 0
        && client.status.desktop.state === 'ready',
      'server did not expose a ready frozen v1.0 desktop',
    );
    await exerciseConnected(client, language);
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
  console.error(`public TypeScript behavior example failed: ${detail}`);
  process.exitCode = 1;
}
