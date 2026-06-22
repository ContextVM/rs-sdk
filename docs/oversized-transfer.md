# Oversized Transfer Guide (CEP-22)

Nostr relays cap event sizes (commonly around 64 KiB on the wire, with a
65,535-byte NIP-44 plaintext ceiling for encrypted payloads), while MCP
results — file contents, tool output, resource reads — routinely exceed that.
CEP-22 closes the gap: a JSON-RPC message whose published form would not fit
in a single event is split into an ordered sequence of frames carried inside
MCP `notifications/progress` events, then reassembled and validated
(byte-length + SHA-256) by the receiver before it surfaces as one ordinary
message. Fragmentation is transparent to MCP consumers in both directions:
client→server requests and server→client responses.

## Enabled by default

Oversized transfer is **enabled by default** (matching the TypeScript SDK).
The negotiation gates make this safe with peers that don't support it:

- the **client** fragments only requests that carry a `progressToken` (rmcp
  stamps one into every outgoing request), and advertises
  `support_oversized_transfer` on its first message;
- the **server** fragments responses only for clients that advertised the
  capability, and advertises it on announcements and its first response.

A peer without CEP-22 support just sees one extra discovery tag. To opt out:

```rust
// Whole-config form:
let config = NostrClientTransportConfig::default()
    .with_oversized_enabled(false);
// Same builder exists on NostrServerTransportConfig.
```

## Configuration

`OversizedTransferConfig` is attached to both transport configs via
`with_oversized_transfer(..)` (or the `with_oversized_enabled(..)` shorthand):

| Field                      | Default      | Description                                                                 |
|----------------------------|--------------|-----------------------------------------------------------------------------|
| `enabled`                  | `true`       | Master gate: advertise + activate the capability                            |
| `threshold`                | `48_000`     | Published byte size at/above which the sender fragments                     |
| `chunk_size`               | `48_000`     | Upper bound on per-chunk payload bytes (shrunk automatically so every published frame stays under `threshold`) |
| `max_transfer_bytes`       | `104_857_600` (100 MiB) | Receiver cap on a reassembled payload                            |
| `max_transfer_chunks`      | `10_000`     | Receiver cap on chunk count                                                  |
| `max_concurrent_transfers` | `64`         | Receiver cap on simultaneously active transfers                              |
| `transfer_timeout_ms`      | `300_000`    | Receiver-side hard deadline per transfer, from admission; `0` disables the watchdog |
| `max_out_of_order_window`  | `21`         | How far ahead of the contiguous frontier a chunk may arrive and still be buffered |
| `max_out_of_order_chunks`  | `42`         | Cap on buffered out-of-order chunks                                          |
| `accept_timeout_ms`        | `30_000`     | How long an uploading client waits for the server's `accept` handshake       |

The decision to fragment is made on the **final published event size**
(signed, JSON-escaped, and gift-wrapped when encryption is on), so `threshold`
is a real wire budget, not a payload-length heuristic.

## The three timers

Three independent timers govern a transfer; knowing who owns each one makes
timeout behavior predictable:

1. **Requester idle timeout** (rmcp, per request — opt-in). Fails the call if
   no progress arrives for `idle`. The transports forward every inbound
   transfer frame to the requester as a plain progress notification, so a
   *live* transfer resets this timer chunk by chunk while a *stalled* one
   fails after `idle`.
2. **Requester max-total timeout** (rmcp, per request — opt-in). Hard cap on
   the whole call regardless of progress — a trickling transfer cannot hold a
   request open forever.
3. **Receiver watchdog** (`transfer_timeout_ms`, transport-owned). A hard
   memory bound on inbound reassembly state, measured from `start` admission
   and never refreshed by activity. Reaping is local-only — no abort frame is
   emitted; the requester's own timers fail the other side. A reaped token is
   re-admittable by a fresh `start`.

The first two exist only when you pass request options — **a plain rmcp
`call_tool` has no timeout at all** (infinite await). That is the main
consumer footgun this SDK papers over:

## Recommended MCP client usage

```rust
use std::time::Duration;
use contextvm_sdk::{progress_aware_options, PeerRequestOptionsExt};

let result = running_client
    .peer()
    .call_tool_with_options(
        params,
        progress_aware_options(Duration::from_secs(60), Duration::from_secs(300)),
    )
    .await?;
```

`progress_aware_options(idle, max_total)` builds
`PeerRequestOptions::with_timeout(idle).reset_timeout_on_progress().with_max_total_timeout(max_total)`.
The defaults `DEFAULT_OVERSIZED_IDLE_TIMEOUT` (60 s) and
`DEFAULT_OVERSIZED_MAX_TOTAL_TIMEOUT` (300 s) intentionally mirror the
transport's numbers (60 s covers the worst-case accept wait; 300 s matches the
receiver watchdog so both sides give up in the same window) — but the peer
layer cannot read transport config, so re-align them manually if you tune
`OversizedTransferConfig`.

Unlike the TypeScript SDK's low-level `client.request()` path, rmcp's timer
reset is not tied to registering an `onprogress` callback — the reset is wired
through request options alone.

For request types beyond `call_tool`, the generic form is two lines on public
rmcp API:

```rust
let handle = peer.send_cancellable_request(request, options).await?;
let response = handle.await_response().await?;
```

### Upload (client→server) caveat

A fragmented *request* receives at most **one** inbound reset: the server's
`accept` handshake frame — and it reaches the rmcp service loop only after the
whole upload send returns. Size `idle` and `max_total` to cover the full
upload duration, not just inter-frame gaps.

## Synthetic progress notifications

Because transfer frames are forwarded to the requester (stripped of their
`cvm` payload, with the request's original `progressToken` restored),
consumers with a custom progress handler observe chunk-granular progress for
oversized responses — usable as transfer-progress UX. Default rmcp handlers
ignore progress for tokens they didn't register, so no action is needed if you
don't want it. Nothing extra goes on the wire; the forwarding is in-process.
