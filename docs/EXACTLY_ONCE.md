# Exactly-Once Ask Semantics (Session-Scoped)

This document is written for external audit: it describes the exact guarantees, failure modes,
and the relevant code paths in `kameo` that implement optional exactly-once semantics for remote
`ask` (request/response).

Non-goals:

- Receiver crash/restart persistence: dedup is in-memory only (session-scoped).
- Exactly-once for `tell` (fire-and-forget) is not provided.

## Threat Model

The design targets these real-world failure modes:

- Lost ACK ("socket stutter"): receiver processes request, but reply never reaches client, so the
  client retries.
- Duplicate delivery: client retries the same logical request multiple times.
- Malicious/buggy client "bait-and-switch": reuses the same request id with a different payload.
- Mid-flight disconnects: peer socket is terminated (FIN/RST/TLS EOF) while client still holds a
  cached `DistributedActorRef` clone.

## High-Level Architecture

Exactly-once is implemented as application-level idempotency with replay:

1. Client generates a globally unique `RequestId` for a logical request.
2. Client wraps the request payload in `ExactlyOnce<M> { request_id, message }` and sends it via
   `ask`.
3. Receiver stores (request_id -> reply) in an in-memory cache owned by the actor instance.
4. If the same request id is received again, the receiver replays the cached reply without
   re-running business logic.
5. If the same request id is reused with a different payload fingerprint, receiver rejects with
   `PayloadMismatch`.

Core implementation:

- `src/remote/exactly_once.rs`

## Code: Client-Generated Request Id

Audit requirement: request ids must be unique across processes, restarts, and concurrent clients.

Implementation (excerpt from `src/remote/exactly_once.rs`):

```rust
/// 128-bit request identifier used for deduplication.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Archive, RSerialize, RDeserialize)]
pub struct RequestId(pub u128);

impl RequestId {
    /// Best-effort unique request id generator (fast, non-cryptographic).
    ///
    /// Composition: [time_micros:64 | random_nonce:64].
    pub fn next() -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros() as u64;
        let nonce = rand::random::<u64>();
        Self(((now as u128) << 64) | (nonce as u128))
    }
}
```

Properties:

- No global/shared counter is used, so k8s scale-outs and process restarts do not collide.
- The nonce is required to avoid collisions across processes that share a timestamp window.

## Code: Dedup Cache And Replay

Receiver-side dedup state is intended to be held inside an actor instance, and accessed from its
`handle` method.

Typed replies:

```rust
#[derive(Debug)]
pub struct ExactlyOnceDedup<R> {
    capacity: usize,
    order: VecDeque<RequestId>,
    cached: HashMap<RequestId, (Option<PayloadFingerprint>, R)>,
}

impl<R> ExactlyOnceDedup<R>
where
    R: Clone,
{
    pub async fn resolve<F, Fut>(&mut self, id: RequestId, f: F) -> R
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = R>,
    {
        if let Some((_, value)) = self.cached.get(&id) {
            return value.clone();
        }

        let value = f().await;
        // Insert before cloning for the return value, so that if `Clone` panics
        // the dedup record still exists for retries.
        self.insert(id, None, value);
        self.cached.get(&id).expect("just inserted").1.clone()
    }
}
```

Key audit note: the implementation inserts the result into the cache before performing the
post-compute `clone()` for the return path. This closes the "panic gap" where the business logic
commits but the dedup record is not written.

## Code: Payload Fingerprint Verification (Bait-And-Switch)

Audit requirement: if an id is retried with a different payload, the receiver must not return a
cached reply or process the new payload.

Implementation (excerpt from `resolve_checked`):

```rust
pub async fn resolve_checked<F, Fut>(
    &mut self,
    id: RequestId,
    fingerprint: PayloadFingerprint,
    f: F,
) -> Result<R, ExactlyOnceError>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = R>,
{
    if let Some((existing, value)) = self.cached.get(&id) {
        // If a caller requests checked semantics, a previous unchecked insert
        // must not bypass payload integrity checks.
        return match existing {
            Some(existing_fp) if *existing_fp == fingerprint => Ok(value.clone()),
            _ => Err(ExactlyOnceError::PayloadMismatch),
        };
    }

    let value = f().await;
    self.insert(id, Some(fingerprint), value);
    Ok(self.cached.get(&id).expect("just inserted").1.clone())
}
```

Audit consequence:

- If an unchecked `resolve` path inserted `(None, value)`, then a subsequent `resolve_checked`
  with the same id is rejected (`PayloadMismatch`). This prevents the "fingerprint bypass".

## Socket Termination: Stale Ref Fast-Fail Without ArcSwap

Requirement: avoid adding ArcSwap indirection to the hot path, but ensure that stale
`DistributedActorRef` clones do not continue sending on dead sockets silently.

Approach:

- Each `DistributedActorRef` obtained via `lookup()` registers a shared liveness token
  `Arc<AtomicBool>` keyed by peer addr (and optionally PeerId).
- On peer socket disconnect, kameo invalidates all tokens for that peer before dispatching
  `on_link_died`.
- Each `tell/ask` performs a single `AtomicBool::load(Ordering::Relaxed)` to fast-fail if the
  cached connection is known-dead.

Relevant code:

- Liveness cache: `src/remote/handle_cache.rs`
- Disconnect invalidation: `src/remote/v2_bootstrap.rs`
- Hot path check: `src/remote/distributed_actor_ref.rs`

What this does and does not do:

- It does not transparently swap the underlying `ConnectionHandle` behind existing clones
  (that would require an extra indirection like ArcSwap).
- It does make stale clones fail immediately after socket death, independent of pool eviction
  timing, with one relaxed atomic load per send.

## Zombie Sockets (Idle Partitions)

This system does not attempt to solve the Two Generals' Problem: an `ask` cannot be made
universally "instant" under arbitrary partitions without additional protocol assumptions.

Supported mitigation:

- Use timeouts (`.timeout(...)`) on `ask` and retry with the same `RequestId`.
- On timeouts or suspected partitions, use `refresh_force_new()` to force a new connection before
  retrying (escapes "same dead socket" loops).

## Tests

Receiver-side dedup invariants:

- `src/remote/exactly_once.rs` contains unit tests for:
  - uniqueness of `RequestId::next()`
  - checked vs unchecked cache behavior (`PayloadMismatch` on mismatch or missing fingerprint)
  - panic-gap mitigation (insert before clone)

Transport-level adversarial framing:

- `kameo_remote/tests/bad_client_stress.rs` exercises:
  - 1-byte TCP fragmentation
  - truncated frames / EOF mid-frame
  - unknown message types

## Summary Of Guarantees

Given a live receiver process (no restart) and a receiver actor that performs the side effect
inside `resolve_checked` (no detached background side effects), the system provides:

- At-most-once side effect execution per `RequestId` at the receiver.
- Replay of the original reply on retries.
- Detection of request id reuse with a different payload (PayloadMismatch).
