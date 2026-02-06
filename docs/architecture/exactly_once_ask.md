# Exactly-Once Ask (Session-Scoped, Remote Only)

This document defines kameo's *application-level* exactly-once semantics for remote `ask`.

Scope:

- Remote only (`DistributedActorRef::ask`).
- Session-scoped: dedup state is in-memory and does not survive receiver process restarts.
- No transport changes: this is implemented as message-wrapping + receiver-side dedup/replay.

## Problem Statement (Lost ACK / Socket Stutter)

The motivating failure mode is:

1. Client sends an `ask`.
2. Receiver runs the side effect and produces a reply.
3. The TCP/TLS connection dies before the reply reaches the client.
4. Client retries.

Without idempotency, the side effect can run twice.

## Design Overview

Exactly-once ask is implemented as:

- Client-generated idempotency key: `RequestId`.
- Wire wrapper: `ExactlyOnce<M> { request_id, message }`.
- Receiver-side replay cache: `ExactlyOnceDedup<R>` or `ExactlyOnceBytesDedup`.
- Optional integrity guard: `PayloadFingerprint` (rejects "same id, different payload").

Implementation lives in:

- `src/remote/exactly_once.rs`

## RequestId (Global Uniqueness)

Audit requirement: request ids must not collide across concurrent clients or restarts.

`RequestId::next()` uses:

- `time_micros` (u64)
- `random_nonce` (u64)

This avoids collisions that would occur with `time + process-local counter` during k8s scale-ups
or after process restarts.

## Wire Compatibility And Type Hashing

Remote routing requires a compile-time type hash. The wrapper implements:

- `HasTypeHash for ExactlyOnce<M>` computed as:
  - `TypeHash("ExactlyOnce") ^ M::TYPE_HASH` (combined deterministically)

This avoids downstream orphan-rule issues and keeps the wrapper hash stable.

See:

- `src/remote/type_hash.rs`
- `src/remote/exactly_once.rs`

## Receiver Dedup And Replay

Receiver stores `(RequestId -> reply)` in an in-memory bounded cache.

Two variants:

- `ExactlyOnceDedup<R>`: typed replies, requires `R: Clone`.
- `ExactlyOnceBytesDedup`: replies already in `bytes::Bytes` (preferred to minimize copies on cache hits).

### Bounded Capacity (Eviction)

Dedup caches are capacity-bounded. If a `RequestId` is evicted and later retried, the receiver will
execute the side effect again.

Capacity selection is application-specific:

- must cover your maximum retry horizon
- must cover client retry concurrency (in-flight request ids)

### Payload Fingerprint (Bait-And-Switch)

If you care about integrity (recommended), compute a fingerprint of the *message payload* and use
`resolve_checked`.

This prevents:

- A buggy or malicious client reusing the same `RequestId` for a different payload.

Important invariant:

- If an id exists in the cache with `fingerprint=None` (inserted via unchecked `resolve`),
  then `resolve_checked` returns `PayloadMismatch` for that id.

This closes the "fingerprint bypass" class of bugs.

## Panic Gap Mitigation

`resolve` inserts into the dedup cache *before* the post-compute clone/return.

This reduces the "side effect committed but dedup record not written" window when `Clone` panics.

See:

- `ExactlyOnceDedup::resolve` in `src/remote/exactly_once.rs`

## Client Retry Strategy (Including Zombie Sockets)

Two generals problem note: you cannot get a universal guarantee without additional assumptions.
What we provide is deterministic retry + receiver replay for duplicate request ids.

Client recommendations:

- Always set a timeout on `ask`.
- Retry only on retryable errors (`ConnectionClosed`, `Timeout`).
- Reuse the same `RequestId` across retries.
- On timeout (possible zombie socket), call `refresh_force_new()` before retrying to force a new dial.

See example:

- `docs/examples/remote-exactly-once-ask.mdx`

## Interaction With Socket Termination And Links

If you use remote linking:

- socket termination triggers `on_link_died(..., ActorStopReason::PeerDisconnected)`
- cached distributed refs fail fast after disconnect (they intentionally do not "heal")

Recommended pattern:

- handle reconnection and reference refresh in `on_link_died`

See:

- `docs/architecture/remote-links.mdx`
- `docs/examples/remote-link-lifecycle.mdx`

## What This Guarantees (And What It Does Not)

Guarantees (while the receiver process is alive and the request id is still cached):

- At-most-once side effect execution per `RequestId` at the receiver.
- Reply replay for duplicate deliveries of the same `RequestId`.
- Optional integrity enforcement via payload fingerprinting (`PayloadMismatch`).

Not guaranteed:

- Exactly-once across receiver restarts (no persistence).
- Exactly-once for `tell`.
- Progress without timeouts during full partitions (zombie sockets).

