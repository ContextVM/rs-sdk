# Open-Stream Guide (CEP-41)

Some MCP tools do not produce a single result. They produce output as they work:
log lines, partial results, incremental progress the caller wants to see right
away rather than after the tool finishes. CEP-41 open-ended streaming carries
that case. A server tool emits an ordered, unbounded sequence of `chunk`
fragments back to the client while a `tools/call` request is still in flight, and
the client consumes them incrementally as an async `Stream`. The stream
supplements the call rather than replacing it: one `tools/call` produces two
outputs, a live stream of chunks and the normal final `CallToolResult` that still
concludes the request.

Frames ride inside MCP `notifications/progress` notifications on the existing
ContextVM message kind (25910), discriminated by `params.cvm.type == "open-stream"`.
The stream id is the request `progressToken`, so chunks correlate to the call
that produced them. This is transparent to peers that do not support CEP-41: they
see one extra discovery tag and nothing else.

## CEP-22 and CEP-41 are different profiles

CEP-22 (oversized transfer) and CEP-41 (open stream) share the same
`notifications/progress` envelope and the same kind 25910, but they are not
interchangeable. CEP-22 is bounded reassembly of a single oversized message: a
message too large for one relay event is split into ordered frames, reassembled
by the receiver, validated by byte length and SHA-256, and surfaced as one
ordinary message that replaces the final response. CEP-41 is an unbounded live
stream consumed incrementally, and it supplements the final response instead of
replacing it. Each profile carries its own `cvm.type` discriminant
(`oversized-transfer` versus `open-stream`), so a peer routes each frame to the
right engine. Use CEP-22 when you have one large result to deliver atomically;
use CEP-41 when you have a progression of outputs to deliver as they happen. See
[oversized-transfer.md](oversized-transfer.md) for CEP-22.

## Enabling open-stream

Open-stream is disabled by default on both transports (opt-in, matching the
TypeScript SDK). Turn it on with `with_open_stream` on either transport config:

```rust
use contextvm_sdk::transport::open_stream::OpenStreamConfig;

let server_config = NostrServerTransportConfig::default()
    .with_open_stream(OpenStreamConfig::enabled());

let client_config = NostrClientTransportConfig::default()
    .with_open_stream(OpenStreamConfig::enabled());
```

`OpenStreamConfig` lives in `contextvm_sdk::transport::open_stream`, not at the
crate root. `OpenStreamConfig::enabled()` is the same as
`OpenStreamConfig::default().with_enabled(true)`: enabled with every other knob at
its default. Once enabled the capability is safe for non-CEP-41 peers, because the
server activates a stream only for clients that advertised support, and injects a
writer only when a request carries a `progressToken`.

## Server side: emitting a stream from a tool

When open-stream is enabled and an incoming `tools/call` carries a
`progressToken`, the transport constructs an `OpenStreamWriter` for that request
and inserts it into the rmcp request extensions before the handler runs. A tool
handler that wants to stream retrieves the writer from `ctx.extensions`:

```rust
use contextvm_sdk::transport::open_stream::OpenStreamWriter;
use rmcp::service::RequestContext;
use rmcp::RoleServer;

#[tool(description = "Stream three chunks then complete")]
async fn stream_demo(
    &self,
    Parameters(_params): Parameters<MyParams>,
    ctx: RequestContext<RoleServer>,
) -> Result<CallToolResult, ErrorData> {
    if let Some(writer) = ctx.extensions.get::<OpenStreamWriter>().cloned() {
        let _ = writer.write("first".to_string()).await;
        let _ = writer.write("second".to_string()).await;
        let _ = writer.write("third".to_string()).await;
        let _ = writer.close().await;
    }
    Ok(CallToolResult::success(vec![Content::text("done")]))
}
```

`OpenStreamWriter` lives in `contextvm_sdk::transport::open_stream`. The writer is
`Clone` and `Arc`-backed, so it can be moved into spawned tasks. Calls to `write`
are serialized internally, so call order equals wire order. The `start` frame is
published lazily on the first `write`, or explicitly with `writer.start().await`.
Always finish the stream: call `writer.close().await` for a normal end, or
`writer.abort(reason).await` to terminate early. After the stream closes, the
handler returns its `CallToolResult` as usual and the transport delivers that
final response.

Retrieving the writer is optional. If `ctx.extensions.get::<OpenStreamWriter>()`
returns `None`, the request did not carry a `progressToken` or open-stream is not
active for this client; the handler should still return a normal result.

## Client side: consuming a stream

The client needs a `ClientOpenStreamHandle`, which binds an inbound stream to the
call that produced it. Capture it from the transport before `serve()` consumes the
transport:

```rust
use contextvm_sdk::{call_tool_stream, ClientOpenStreamHandle};
use futures::StreamExt;
use rmcp::model::CallToolRequestParams;

// Capture the handle BEFORE the transport is moved into `serve`.
let handle: ClientOpenStreamHandle = client_transport.open_stream_handle();
let client = MyClientHandler.serve(client_transport).await?;

let mut call = call_tool_stream(
    client.peer(),
    &handle,
    CallToolRequestParams::new("stream_demo"),
)
.await?;

// Consume chunks as they arrive.
while let Some(item) = call.stream.next().await {
    match item {
        Ok(chunk) => println!("chunk: {chunk}"),
        Err(error) => {
            eprintln!("stream error: {error}");
            break;
        }
    }
}

// The final result resolves after the stream closes.
let result = call.result.await?;
println!("final: {result:?}");
```

`call_tool_stream` returns a `ToolStreamCall` with four parts: `progress_token`
(the stringified token that correlates the call and its stream), `stream` (an
async `Stream` of `Result<String, OpenStreamError>` chunks), `result` (a future
resolving to the final `CallToolResult` after the stream closes), and an `abort`
method that cancels the call. `call_tool_stream` and `ClientOpenStreamHandle` are
re-exported at the crate root; the chunk error type `OpenStreamError` lives in
`contextvm_sdk::transport::open_stream`.

To cancel from the consumer side, call
`call.abort(Some("reason".to_string())).await`. That publishes an `abort` frame to
the server so its writer stops, finalizes the local stream, and frees the reader
slot.

## The timeout model

A reader protects itself against a stream that goes silent with three timers, all
configurable on `OpenStreamConfig`. The idle timeout (`idle_timeout_ms`, default
30000) is how long the reader waits without any frame before it probes the peer
with a `ping`. Every inbound frame resets it, so a live stream never trips it. The
probe timeout (`probe_timeout_ms`, default 20000) is how long the reader then
waits for a `pong` before it gives up and aborts the stream. The close grace
period (`close_grace_period_ms`, default 5000) applies after a `close` arrives
with buffered gaps still unresolved: the reader waits this long for the missing
chunks before aborting.

There is no hard lifetime cap by default; an open stream may legitimately run for
a long time. Set `max_total_timeout_ms` to `Some(ms)` if you want one.
`call_tool_stream` also derives the rmcp request timeout from these values,
summing idle, probe, and close-grace so the rmcp request is never failed before
the keepalive logic would have aborted a genuinely dead stream, and it re-arms
that timeout on every forwarded frame.

## Known limitation: keep the final response small

The final `CallToolResult` of a streamed call is delivered on a deferred path that
publishes it as a single relay event. That path does not apply CEP-22
fragmentation. A normal, non-streamed response at or above the oversized threshold
(48000 bytes by default) is split into frames and reassembled, but the deferred
final response of a streamed call is not, so it must fit within a single relay
event (the same single-event ceiling described in the oversized transfer guide,
roughly 64 KiB on the wire). Keep the final result small and let the bulk of the
payload ride the stream as chunks. The streaming tools in
`tests/open_stream_e2e.rs` follow this pattern: they stream the data and return
only a short completion string.

## Configuration reference

`OpenStreamConfig` is attached to both transport configs via
`with_open_stream(..)`. All fields:

| Field                            | Default              | Description                                                                 |
|----------------------------------|----------------------|-----------------------------------------------------------------------------|
| `enabled`                        | `false`              | Master gate. When `false` the capability is neither advertised nor activated |
| `max_concurrent_streams`         | `64`                 | Upper bound on concurrently active streams per peer                          |
| `max_buffered_chunks_per_stream` | `64`                 | Upper bound on buffered plus queued chunks held for a single stream          |
| `max_buffered_bytes_per_stream`  | `524288` (512 KiB)   | Upper bound on buffered plus queued payload bytes held for a single stream   |
| `idle_timeout_ms`                | `30000`              | Idle interval after which a reader probes the peer with a `ping`             |
| `probe_timeout_ms`               | `20000`              | Time a reader waits for a `pong` after probing before aborting               |
| `close_grace_period_ms`          | `5000`               | Grace period after a `close` with unresolved gaps before aborting           |
| `max_total_timeout_ms`           | `None`               | Optional hard cap on total stream lifetime; read only by `call_tool_stream`  |

Each field has a `with_*` builder (`with_enabled`,
`with_max_concurrent_streams`, `with_max_buffered_chunks_per_stream`,
`with_max_buffered_bytes_per_stream`, `with_idle_timeout_ms`,
`with_probe_timeout_ms`, `with_close_grace_period_ms`,
`with_max_total_timeout_ms`), so you can start from `OpenStreamConfig::enabled()`
and override individual knobs.
