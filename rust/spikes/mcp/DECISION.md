# DECISION — the MCP transport is `rmcp` 3.11 stdio; no hand-rolled JSON-RPC

**Verdict: `rmcp` 3.11.0 can reproduce the TypeScript server's wire behaviour on all four counts.**
It frames on `\n` with no `Content-Length`, it does not gate on `notifications/initialized`, it
carries in-band error envelopes (`isError: true` inside a JSON-RPC *result*), and `tools/list`
returns the raw schema object. The hand-rolled JSON-RPC over `tokio` stdin/stdout lines (≈400 lines)
that `RUST-MIGRATION.md` §8 R2 kept as the fallback is **not needed** and should not be built.

Three findings qualify the verdict without overturning it, and all three land in TASK-034:

1. **Responses may arrive out of request order.** `rmcp` dispatches concurrently: a burst of five
   frames came back as ids `1,5,3,2,4` (and `1,5,2,3,4` on the next run), while the TypeScript
   server answered the same burst `1,2,3,4,5`. That is by construction, not luck: `serve_inner`
   spawns every request's handler as its own task and each one writes its own response through the
   sink proxy (`src/service.rs`, `Event::PeerMessage(… Request …)`). Both behaviours are legal
   JSON-RPC — a client correlates by `id` — but a parity test that compares *arrival order* will
   fail against a conforming Rust server. TASK-034 must key its comparison by request id.
2. **`inputSchema` key order is normalised.** The schema's members survive unchanged; their order
   does not, because `serde_json::Map` is sorted unless the `preserve_order` feature is on. See (d).
3. **`rmcp` does not validate `arguments` against `inputSchema`.** The schema is an opaque object it
   forwards. Argument refusal stays where it is today: in the ported `parseArgs`, answering
   `CallToolResult::error` (see (c)).

## What was built and run

| Path | What it is |
|---|---|
| `rust/spikes/mcp/` | Standalone `rmcp` 3.11 server (`[workspace]` in its `Cargo.toml`, own target dir). One tool, `echo`, whose `inputSchema` is the raw object in `rust/spikes/mcp/echo_schema.json` — no derive macro — plus `capabilities.tools` and an `instructions` string. |
| `scripts/parity/mcp-session.ts` | The recorder (`bun`). Records the TypeScript server, or replays the recording against another binary (`--replay`). |
| `rust/tests/vectors/mcp_session.ndjson` | The evidence: 9 frames, `{"dir":"c2s"\|"s2c","raw":"<exact bytes, no terminator>"}`, one per line. |
| `rust/tests/vectors/mcp_session_ts_only.json` | The TypeScript server's four answers, raw, plus its framing counters. |
| `rust/spikes/mcp/replay_s2c.ndjson` | The spike's answers to the same frames, and to the probes steps 2 and 3 below needed. |

```sh
# record: the TypeScript server, on a temp --data-dir, never the repo's .sangkar/graph.db
~/.bun/bin/bun run scripts/parity/mcp-session.ts
# replay: the same client frames against the spike
~/.bun/bin/bun run scripts/parity/mcp-session.ts --replay rust/spikes/mcp/target/debug/sangkar-spike-mcp
```

Both exit `0`. The recording sends `initialize` (id 1), `notifications/initialized`, `tools/list`
(id 2), `tools/call status {}` (id 3), and `tools/call status {"budget":"many"}` (id 4) — the last
one invalid on purpose, because `status`'s manifest declares `budget` as `{ type: "integer" }`.

## (a) Does the frame carry only `\n` terminators?

**Yes, on both servers, and nothing else is on the wire.**

| | frames | bytes | `\n` | CR bytes | `Content-Length` | bytes left after the last `\n` | exit |
|---|---|---|---|---|---|---|---|
| TypeScript (`@modelcontextprotocol/sdk` 1.32.2) | 4 | 14766 | 4 | 0 | 0 | 0 | 0 |
| Spike (`rmcp` 3.11.0) | 4 | 933 | 4 | 0 | 0 | 0 | 0 |

The recorder counts terminators by splitting its read buffer on `0x0a` and refuses to report success
if a `0x0d` or the string `Content-Length` ever appears in the stream, or if anything is left over
after the final terminator. `rmcp` says the same in its own source — `transport/async_rw.rs` reads
one `\n`-terminated line at a time (`available.iter().position(|b| *b == b'\n')`) and appends
`buf.put_u8(b'\n')` when it writes (`JsonRpcMessageCodec::encode`, same file, line 615).

One framing difference, and it is not the terminator: the TypeScript SDK serialises the envelope as
`{"result":…,"jsonrpc":"2.0","id":N}` while `rmcp` serialises it as `{"jsonrpc":"2.0","id":N,"result":…}`.
JSON-RPC object member order is not significant, so this is a difference only for a byte-diff of the
envelope, never for a client.

## (b) Does the server require `notifications/initialized` before answering?

**No — neither one does.** Both answer `tools/list` sent straight after `initialize`, with no
`initialized` notification in between:

```
no-initialized, c2s: {"jsonrpc":"2.0","id":2,"method":"tools/list"}
no-initialized, s2c: {"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"echo",…}]}}
```

The TypeScript side answers identically (checked by hand: `initialize` then `tools/list` on a pipe
returns both results and exits 0). The `notifications/initialized` frame itself produces no output on
either side — it is a notification, and the session files four frames for five client frames.

`rmcp` also matches the TypeScript SDK on the *reason*: its server loop returns straight after sending
`InitializeResult`, with the comment "This matches the TypeScript SDK behavior: no init gate, no
waiting for initialized" (`src/service/server.rs:721-726`).

A related difference, worth knowing and harmless for real clients: a request sent with **no**
`initialize` at all is answered normally by the TypeScript server, while `rmcp` answers it
`{"error":{"code":-32602,"message":"request _meta is missing or has malformed required fields: …"}}`
and then exits. No MCP client does this — every one initialises first — so this is a note for the
parity suite's edge cases, not a blocker.

## (c) Is a bad argument or an unknown tool answered in-band with `isError:true`?

**Yes, on both, and this is the load-bearing one for TASK-034.**

TypeScript, `status` with a string `budget` (id 4) — a result, not a JSON-RPC error:

```
{"result":{"content":[{"type":"text","text":"status: error\nsummary: status could not answer\n…
error:\n  code: internal\n  message: \"status received an invalid \\\"budget\\\" argument\"\n  hint: …
"}],"isError":true},"jsonrpc":"2.0","id":4}
```

(TypeScript's text part is a TOON envelope; the `…` marks the elided middle. The whole frame is in
`rust/tests/vectors/mcp_session.ndjson`.)

`rmcp`, the same frame replayed (the spike registers no `status`, so id 4 exercises the *unknown
tool* path), and the bad-argument path on `echo` — both in-band:

```
replay id 4:  {"jsonrpc":"2.0","id":4,"result":{"content":[{"type":"text","text":"no tool named \"status\" in this surface; this spike registers: echo"}],"isError":true}}
probe  id 11: {"jsonrpc":"2.0","id":11,"result":{"content":[{"type":"text","text":"echo could not answer: `text` must be a string"}],"isError":true}}
probe  id 10: {"jsonrpc":"2.0","id":10,"result":{"content":[{"type":"text","text":"{\"count\":2,\"text\":\"hello\"}"}],"isError":false}}
```

The `isError:false` line is the `echo` contract holding: the arguments come back as JSON.

Two mechanics behind this, both confirmed by the probe:

- `CallToolResult::error(content)` serialises to a JSON-RPC **result** with `isError: true`; the
  protocol-error path (`Err(McpError)`) was never taken. So the Rust surface can port the TypeScript
  `answer()` — "`isError` is true exactly when the envelope carries an `error`" — verbatim.
- The bad argument only became `isError:true` because the *handler* checked it. `rmcp` passed
  `{"text": 42}` straight through its typed request; nothing validates against `inputSchema`. The
  Rust side must own that check, which is where the ported `parseArgs` already sits.

## (d) Does `tools/list` return the raw schema object unchanged?

**Its content, yes; its key order, no — and one Cargo feature restores the order.**

Declared in `rust/spikes/mcp/echo_schema.json`:

```json
{"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","properties":{"text":{"type":"string","description":"the text to echo"},"count":{"type":"integer","minimum":1}},"required":["text"],"additionalProperties":false}
```

On the wire, from the spike's `tools/list`:

```json
{"$schema":"https://json-schema.org/draft/2020-12/schema","additionalProperties":false,"properties":{"count":{"minimum":1,"type":"integer"},"text":{"description":"the text to echo","type":"string"}},"required":["text"],"type":"object"}
```

Same members, same values, same nesting — every object's keys sorted. The cause is that `Tool.input_schema`
is `Arc<serde_json::Map<String, Value>>` and `serde_json`'s `Map` is a sorted `BTreeMap` unless its
`preserve_order` feature is on. Measured, not assumed: building the same spike with
`serde_json = { version = "1", features = ["preserve_order"] }` puts exactly the declared order on the
wire:

```json
{"$schema":"…","type":"object","properties":{"text":{…},"count":{…}},"required":["text"],"additionalProperties":false}
```

So TASK-034 has a choice, and should make it deliberately: compare `inputSchema` as parsed JSON (what
the replay does — `contentIdentical: true`, `keyOrderPreserved: false`), or enable `preserve_order`
if the parity bar is byte-for-byte. The spike keeps the default feature set, because that is what a
plain `rmcp = "3.11"` dependency gives the product crate.

The TypeScript side is unchanged in both senses: its `inputSchema` is the object `inputSchemaOf`
builds (`src/index.ts:254-274`) — `type`, `properties` in manifest declaration order, `required` only
when a field declares one, and the manifest's `examples` — and the SDK puts it on the wire as is.

## What TASK-034 must therefore do

1. Register the seven tools through `rmcp`'s `ServerHandler` (`list_tools` → `Tool::new`, with the
   `inputSchema` parsed from the manifest's raw JSON), not through a derive macro.
2. Port `answer()` to `CallToolResult::success` / `CallToolResult::error`, so `isError` stays "true
   exactly when the envelope carries an error" — including the unknown-tool and bad-argument paths,
   because `rmcp` will not produce them by itself.
3. Compare against `mcp_session_ts_only.json` **by request id**, as parsed JSON, not by arrival order
   and not by envelope bytes. If the bar is byte equality of `inputSchema`, enable
   `serde_json/features = ["preserve_order"]` first, and expect `resultType` to appear for peers that
   negotiate `2026-07-28` (it is correctly absent for the `2025-11-25` client this vector records).
4. Keep `--data-dir` in every parity test, exactly as this recorder does: `status` is read-only, but a
   test that forgets the flag opens the repository's real `.sangkar/graph.db`.
