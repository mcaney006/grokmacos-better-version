# ADR 0006: WebSocket keepalive + receive watchdog + connect/send timeouts

## Status

Accepted (2026-05).

## Context

Voice mode uses a long-lived WebSocket to `wss://api.x.ai/v1/realtime`.
Real-world failure modes we encountered:

1. **Stateful NATs / corporate proxies** silently drop idle WebSocket
   connections after ~60 seconds. From our side, the socket appears
   open until the next send; the user notices "voice just stopped"
   with no error.
2. **Server stops sending** (process restart, half-open TCP after a
   network blip). Reads pend forever; nothing surfaces.
3. **Server stops reading** (back-pressured load balancer). Writes
   fill the kernel send buffer and `sink.send().await` blocks until
   TCP keepalive fires — typically 2+ hours on Linux defaults.
4. **TLS handshake never completes** (attacker, broken LB). The
   `connect_async` future never resolves and the spawned voice-open
   task leaks for the process lifetime.

## Decision

Four independent controls, each addressing exactly one of the failure
modes above:

| Control | Constant | Mode | Mitigates |
|---|---|---|---|
| WebSocket connect timeout | `WS_CONNECT_TIMEOUT_SECS = 10` | `tokio::time::timeout` around `connect_async` | #4 |
| Keepalive ping | `WS_PING_INTERVAL_SECS = 30` | `tokio::time::interval` in the uplink loop | #1 |
| Receive watchdog | `WS_RECV_WATCHDOG_INTERVAL_SECS = 60`, `WS_RECV_DEADLINE_SECS = 90` | separate `tokio::spawn` task comparing `last_recv: Arc<AtomicI64>` against `epoch_secs()` | #2 |
| Per-send timeout | `WS_SEND_TIMEOUT_SECS = 15` | `tokio::time::timeout` around every `sink.send()` (audio frame + ping) | #3 |

Failures from any control emit a `VoiceEvent::Error` AND set
`closed = true` in the UI drain handler so the session tears down
cleanly. (See ADR 0004 pattern: the error variant carries enough
context for a toast and a teardown.)

## Consequences

- Voice sessions cannot silently die. Every failure mode produces a
  user-visible event within ≤ 90 seconds (worst case: receive
  watchdog with no audio activity to trigger send-side checks
  earlier).
- Tests in `services::voice::tests::ws_keepalive_send_failure_emits_error`
  and `ws_open_with_url_times_out_on_hung_handshake` drive real
  loopback TCP listeners and assert finite-time error surfacing —
  no `tokio::time::pause` shortcuts.
- The 15s send timeout is generous (no healthy provider takes more
  than tens of milliseconds to ACK), but tight enough that a
  permanently-stalled connection surfaces within seconds rather than
  hours.

## Rejected alternatives

- **Rely on OS TCP keepalive.** Linux defaults are 7200s before the
  first probe; even tuned, the surface is the same: silent for
  minutes-to-hours.
- **Treat every send as fire-and-forget without bounding.**  Hides
  the back-pressure problem in #3.
- **Use a heartbeat reply check** (verify that a Pong follows each
  Ping). Adds round-trip dependency for liveness; the watchdog
  already covers this implicitly because Pong frames update
  `last_recv`.
