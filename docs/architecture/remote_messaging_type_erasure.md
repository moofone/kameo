# Remote Messaging: Type Erasure And Routing

Remote messaging is fundamentally "type erased": the network carries bytes, not Rust types.

This document explains how kameo restores type information at the receiver without runtime type
inspection, and what constraints that imposes on your API and deployments.

## The Core Problem

At the call site you write:

- `remote.tell(Msg)`
- `remote.ask(Msg)`

But the transport can only send:

- `[type_hash][payload_bytes]`

So the receiver must answer:

1. Which message handler should receive these bytes?
2. How do we safely deserialize the bytes into the expected type?

## How kameo Routes Remote Messages

Kameo uses a compile-time type hash:

- Trait: `kameo::remote::type_hash::HasTypeHash`
- Macro: `#[derive(kameo::RemoteMessage)]` (recommended)

The sender puts `M::TYPE_HASH` on the wire. The receiver uses that hash to select the correct
handler for `Message<M>`.

This avoids dynamic dispatch and reflection:

- routing is O(1) map lookup on `type_hash`
- handler code is monomorphized by `M` on both sides

## What "Type Erasure" Means Operationally

Because the wire is bytes:

- Sender and receiver must agree on the message type for a given `type_hash`.
- If you deploy mismatched binaries (old sender, new receiver) you can get:
  - hash mismatch (message rejected)
  - deserialize errors
  - silent semantic drift if you intentionally keep hash stable while changing meaning

Treat remote message schemas as a compatibility surface.

## ExactlyOnce Wrapper And Type Hash Composition

Exactly-once ask uses a wrapper:

- `ExactlyOnce<M> { request_id, message }`

It still participates in routing, so it has its own type hash derived from `M`:

- `TypeHash("ExactlyOnce")` combined with `M::TYPE_HASH`

This ensures:

- `ExactlyOnce<Foo>` and `ExactlyOnce<Bar>` are routed to different handlers
- you do not need to implement `HasTypeHash` for a foreign wrapper type in downstream crates

See:

- `docs/architecture/exactly_once_ask.md`

## Why This Is Not "Dynamic Any"

Kameo does not attempt to send Rust type names over the wire and then downcast at runtime.
That approach is:

- slower (string handling + dynamic dispatch)
- fragile across crate paths/renames
- harder to audit for compatibility

Instead, kameo uses small, fixed-size hashes and compile-time constraints.

## Implications For Public APIs

When you publish a distributed actor API, define:

- message types (with `RemoteMessage`)
- reply types (must be rkyv-compatible too)

Then treat those message/reply types as a versioned wire API.

