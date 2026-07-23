# WebSocket application protocol v1.0

`GET /v1/ws` uses an authenticated WebSocket upgrade. Browser clients must send
exactly one configured `Origin`; non-browser SDKs may omit it. Version 1 uses
text JSON application messages only.

## Implemented messages

The first application message must be `client.hello` within five seconds. It is
strictly decoded: unknown fields, nil identifiers, control characters in client
metadata, or a protocol range that does not include 1.0 are rejected. No command
may precede `server.welcome`.

Implemented client messages:

| Type | Payload rule | Implemented response |
| --- | --- | --- |
| `client.hello` | `request_id`, v1 minor range, bounded client descriptor, optional resume cursor | `server.welcome` |
| `client.ping` | nonempty bounded `nonce` | `server.pong` echoes request and nonce |
| `command.submit` | complete CommandEnvelope; outer and inner `request_id` must match | `command.accepted` or terminal `command.result`; starts a watch while nonterminal |
| `command.watch` | current desktop/generation and known command ID | immediate `command.progress` or `command.result`; then watched updates |
| `command.unwatch` | same generation-bound command identity | `command.unwatched` with `watching: false` |
| `command.cancel` | same generation-bound command identity | `command.progress` or terminal `command.result` |
| `lease.get` | current desktop/generation | `lease.state` |
| `lease.acquire` | nested LeaseAcquireRequest; outer and inner `request_id` match | `lease.state` |
| `lease.renew` | nested LeaseRenewRequest; outer and inner `request_id` match | `lease.state` |
| `lease.release` | nested LeaseReleaseRequest; outer and inner `request_id` match | `lease.state` after owned-input reset completes |
| `events.subscribe` | current desktop/generation, up to 32 unique exact topics, optional exclusive `since_sequence` | `events.subscribed`, replay outcome, then live `event` messages |
| `events.unsubscribe` | current desktop/generation | `events.unsubscribed` |

Implemented server messages are `server.welcome`, `server.pong`,
`command.accepted`, `command.progress`, `command.result`, `command.unwatched`,
`lease.state`, `events.subscribed`, `events.unsubscribed`, `event`,
`events.replay_complete`, `events.resync_required`, `server.draining`, and
`error`. `command.progress` and ordinary events are droppable under pressure;
terminal results, resync requirements, draining, errors, and close frames use
reserved high-priority capacity. A session supports at most 256 simultaneous
command watches and one replaceable event subscription. Current default welcome
limits are:

```json
{
  "max_message_bytes": 1048576,
  "heartbeat_ms": 15000,
  "normal_outbound_capacity": 1024,
  "reserved_outbound_capacity": 32,
  "max_command_watches": 256
}
```

Terminal `command.result` messages use the same additive trace contract as HTTP.
Submitting `trace_policy: "detailed"` retains at most 16 enum-only, content-free
steps, including honest stopped/failure progress; omitted, `none`, and `normal`
policies omit `trace`. The trace cannot carry protected text, clipboard payloads,
tokens, or arbitrary diagnostic strings.

The hello resume cursor is
`{desktop_id, desktop_generation, event_sequence}`. Omitting it yields
`not_requested`. A complete retained suffix for the exact current generation
yields `replayed` and begins delivery after welcome. A different generation,
sequence ahead of the live edge, or lost retention history yields
`resync_required`. Resume requires `desktop:observe`; denial sends an error and
closes with 1008.

### Event stream and replay

`events.subscribe` requires `desktop:observe`. Explicit `accessibility.*` topic
requests additionally require `accessibility:read`; a catch-all subscription
filters those topics when the principal lacks that grant. `topics` are exact
lowercase identifiers, not globs; an empty list means every topic authorized for
the principal. Topics contain at most 128 UTF-8 bytes, use stable lowercase
alphanumeric segments separated by `.`, `_`, or `-`, and are unique within the
request. `since_sequence` is an exclusive lower bound; `null` starts from the
atomically captured current live edge. A new subscribe replaces the session's
previous event subscription.

The server first emits `events.subscribed`. A complete replay then emits zero or
more `event` messages followed by `events.replay_complete.through_sequence`.
Live delivery continues from the same atomic subscription point, so the replay/
live boundary has neither a subscription race nor duplicated coordinator event.
Sequences are assigned globally before principal/topic filtering; visible
sequences may therefore jump and clients must not mistake a filtered jump for a
gap.

The implemented topics are:

- `command.lifecycle` for admission and terminal ledger transitions;
- `action.lifecycle` when executor action starts;
- `process.exited` after a managed application child has been reaped;
- `accessibility.element_created` for exact cache births/created objects;
- `accessibility.element_changed` for normalized state, property, focus, text,
  value, selection, children, bounds, and visible-data changes;
- `accessibility.element_removed` for exact cache removals/destroyed objects;
- `accessibility.resync_required` when the AT-SPI model itself requires an
  authoritative refresh.

The two command/action payloads have `command_id`, `command_lifecycle`
(`accepted|running|terminal`), `action_state` (`null|started|completed`),
`updated_monotonic_ms`, and `terminal`. Terminal data is either `null` or
`{cause,effect}`, with effect `before_effect|after_effect`; it is lifecycle
evidence, not a replacement for authoritative `command.result`.

The `process.exited` payload is the
[`ProcessExitedEvent`](../../../schemas/v1/process-exited-event.json) shape:
`application`, a terminal `process` view, `termination_requested`, and
`forced_escalation`. Natural exits set both cleanup flags to `false`; a managed
terminate operation sets `termination_requested`, and sets `forced_escalation`
only when its graceful interval ended in SIGKILL. The event is delivered only
to the authenticated principal that launched the process. The owner identity,
termination-requester identity, stdout, and stderr are never placed in the
public event payload. Use `process.exit` as terminal evidence, while retaining
the exact generation/PID/start-ticks/launch-ID reference for correlation.

The accessibility topic payload is the generated
[`AccessibilityEvent`](../../../schemas/v1/accessibility-event.json). It carries
desktop and AT-SPI generations, actor revision/cache sequence, a resolved
`ElementRef` when possible, and raw bus/path evidence. If resolution failed,
`source` is `null`, `source_stale` is true, and clients must not infer a current
identity from the raw path. A model-level `accessibility.resync_required` event
is source-free and carries one of `actor_signal`, `generation_changed`,
`event_gap`, or `event_queue_overflow` in `resync_reason`.

Accessibility text events are content-minimized. They may carry bounded start
and length evidence; protected text has `redacted: true` and `content: null`.
Cache-transition events never contain text bodies. The checked-in
`current-event-accessibility-element-changed.json` example demonstrates the
protected shape without embedding a secret.

Every delivered event nests `{desktop_id, desktop_generation, sequence, topic,
payload}`. The topic-specific payload is capped at 256 KiB before envelope and
transport bounds. Events are principal-scoped in the coordinator before replay.

When continuity cannot be proven, `events.resync_required` uses one of
`generation_changed`, `history_lost`, `sequence_ahead`, `subscriber_lag`, or
`outbound_backpressure`, plus `dropped_through` and `latest_sequence`. It uses
reserved output capacity and ends that event subscription; refresh authoritative
state before subscribing again. The WebSocket itself may remain usable.

Do not confuse the transport message `events.resync_required` with the ordinary
topic `accessibility.resync_required`. The former means global coordinator-stream
continuity is lost and terminates the subscription. The latter is a sequenced
domain event indicating that authoritative AT-SPI model state must be refreshed;
queue overflow can conservatively escalate to the global transport barrier.

The process broker has its own bounded sequence and retention window. If the
daemon cannot bridge a broker disconnect or retention gap, it publishes a
metadata-free global resynchronization barrier into the coordinator stream.
Every subscription that replays or reaches that barrier receives
`events.resync_required` with `history_lost`, even if the lost process belonged
to another principal. This conservative boundary discloses no owner, process,
or application metadata and prevents the server from falsely claiming a
complete replay. Clients should refresh authoritative process status for every
process reference they still track, then subscribe again from the returned live
edge.

On the desktop transition to draining, the server stops event delivery, emits
high-priority `server.draining` with the generation and safe reason code, allows
existing command watches a bounded convergence window, then closes with 1001
`server draining`.

The examples prefixed `current-` in [`examples/ws/`](examples/ws/) are accepted
or emitted by the implementation at the time this document was generated.

## Still planned, not implemented

The broader design mentions `snapshot.request` for bounded small JSON snapshots.
It is not accepted by the v1.0 decoder. Larger snapshots/artifacts remain future
HTTP work. There are no `planned-` example files in this implemented contract.

## Error and close behavior

WebSocket `error` is a compact connection/message-level shape, not the HTTP
RFC 9457 Problem shape. It includes `type`, optional `request_id`, stable `code`,
safe `detail`, and `desktop_generation` only for stale-reference recovery.
Command execution failures instead appear inside terminal `command.result.error`.

- invalid handshake or unsupported negotiation: error, then close 1002;
- invalid application JSON/shape: error, then close 1007;
- binary application message: close 1003;
- per-session message-rate exhaustion: error, then close 1008;
- 45-second application-heartbeat staleness at defaults: close 1001.

RFC Ping/Pong frames do not replace the application `client.ping` heartbeat.
