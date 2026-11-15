# MCP 2026-07-28 and sessions: research

Date: 2026-11-15. Sources were read on that date; `rmcp` claims were checked against the 3.9.0 source.

Question: does protocol 2026-07-28 remove Streamable HTTP sessions, what replaces them, can a server still keep
per-client state, what happens on stdio, which Rust SDKs keep sessions for modern clients, and can an rmcp server
refuse 2026-07-28 to keep sessions.

## Success criteria
1. SEP-2567 text and status read from the modelcontextprotocol org (not blogs).
2. 2026-07-28 spec pages quoted for state mechanisms, stdio, and legacy interop.
3. Rust SDK versions and era support taken from crates.io plus each repo's own README or changelog.
4. rmcp claims checked against rmcp-3.9.0 source on disk.
5. Client behaviour (Claude Code) taken from its own changelog, not summaries.

## Hypothesis tree
- H1: sessions are fully removed from 2026-07-28 and no opt-in survives. CONFIRMED (spec + SEP + changelog).
- H2: something protocol-level replaces them (token, header, extension). REFUTED: the replacement is a tool-design
  pattern (explicit handles as ordinary tool arguments); no wire construct. Related stateless mechanisms:
  `requestState` (MRTR), `subscriptions/listen`, Tasks extension handles.
- H3: sessions only deprecated, still negotiable on 2026-07-28. REFUTED: "clean break ... no deprecation window";
  sessions survive only in the legacy revisions (<= 2025-11-25) via `initialize`.

## Findings with sources

### Q1 Removal and SEP-2567 status
- Spec changelog (2026-07-28), Major change 1: "Remove protocol-level sessions and the `Mcp-Session-Id` header from
  the Streamable HTTP transport. List endpoints ... no longer vary per-connection. Servers that need cross-call state
  use explicit, server-minted handles passed as ordinary tool arguments (SEP-2567)."
  https://modelcontextprotocol.io/specification/2026-07-28/changelog.md
- Streamable HTTP page note: "Revision 2026-07-28 changed the behavior of Streamable HTTP ... Removal of the GET
  stream endpoint. Removal of protocol-level sessions."
  https://modelcontextprotocol.io/specification/2026-07-28/basic/transports/streamable-http.md
- SEP-2567 "Sessionless MCP via Explicit State Handles": Status Final, Standards Track, created 2026-03-11, author
  Peter Alexander. https://modelcontextprotocol.io/seps/2567-sessionless-mcp.md
- PR #2567 (GitHub API): merged 2026-05-07T17:34:23Z, labels SEP, final, transport.
  https://github.com/modelcontextprotocol/modelcontextprotocol/pull/2567
- Release tag `2026-07-28` published 2026-07-28T16:47:49Z (RC 2026-05-29).
  https://github.com/modelcontextprotocol/modelcontextprotocol/releases
- Versioning page: "The current protocol version is 2026-07-28." https://modelcontextprotocol.io/specification/versioning.md
- SEP-2575 (Make MCP Stateless, removes `initialize`) also Final. https://modelcontextprotocol.io/seps/2575-stateless-mcp.md
- SEP text on rollout: "Rollout is a clean break: sessions are removed in the next spec version, with no deprecation
  window. Servers that currently rely on session-scoped state stay on the current protocol version until they have
  migrated to explicit handles."
- Official blog: "we've officially retired the `initialize`/`initialized` exchange along with the `Mcp-Session-Id`
  header ... Dropping the protocol-level session doesn't force your application to be stateless."
  https://blog.modelcontextprotocol.io/posts/2026-07-28/

### Q2 What the spec offers for cross-request state
- Statelessness section (basic/index): "State that needs to span multiple requests (e.g., long-running tasks,
  application-level handles) MUST be referenced by an explicit identifier the client passes on each request."
  Also "Servers MUST NOT rely on prior requests over the same connection to establish context (e.g., capabilities,
  protocol version, client identity)."
  https://modelcontextprotocol.io/specification/2026-07-28/basic/index.md
- Explicit handles (SEP-2567): "From the protocol's perspective a handle is a string in a tool result and a string in
  a tool argument ... a tool-design pattern". Guidance (non-normative): opaque handles, validate (handle, auth_context)
  on every call, document durability in the create tool description, expired handles return useful errors.
- Security best practices (normative for servers with authorization): "MCP servers that implement authorization MUST
  verify all inbound requests. MCP servers MUST NOT treat possession of a state handle as authentication." and bind
  the handle to the verified token principal.
  https://modelcontextprotocol.io/docs/2026-07-28/tutorials/security/security_best_practices.md
- MRTR `requestState`: "An opaque string meaningful only to the server. Clients MUST NOT inspect, parse, modify" and
  "allows servers to request additional information without maintaining any server-side state" (scope: within one
  request's retry loop, not long-lived client state).
  https://modelcontextprotocol.io/specification/2026-07-28/basic/patterns/mrtr.md
- `subscriptions/listen`: replaces `resources/subscribe` and the GET endpoint; per-request long-lived POST stream,
  opt-in filter (toolsListChanged, promptsListChanged, resourcesListChanged, resourceSubscriptions). Subscription state
  is scoped to the listen request, not the connection. Only these notification types ride it; `notifications/message`
  and progress flow only on the originating request's stream.
  https://modelcontextprotocol.io/specification/2026-07-28/basic/patterns/subscriptions.md
- Tasks: official extension `io.modelcontextprotocol/tasks` (SEP-2663, Final). Task handles are returned by the server;
  SEP says tasks "should be bound to some sort of authorization context". It is async-work state, not client identity.
  https://modelcontextprotocol.io/seps/2663-tasks-extension.md
- No per-client or per-conversation identifier exists on the wire. `_meta` reserved keys are protocolVersion,
  clientInfo (name/version only, optional "SHOULD"), clientCapabilities, logLevel, subscriptionId, progressToken,
  traceparent/tracestate/baggage. `clientInfo` is not unique per instance.
- Tool-parameter mirroring: `x-mcp-header` copies a tool argument into an `Mcp-Param-*` header (routing aid, not state).

### Q3 Per-client server-side state under 2026-07-28
- Allowed: the SEP explicitly keeps auth-principal scoping: lists "can vary by the authorization presented on the
  request: credentials are carried on each request ... relying on per-request input, not connection state". The
  migration table maps session-keyed application state to "explicit handles or auth principal", telemetry keys to
  "the authenticated principal (bearer token subject, API key) or a request-level correlation ID", and session->user
  pinning to "token subject". So keying server-side state by verified token subject / OAuth client identity is
  legitimate; keying by connection, process or `clientInfo` is not endorsed.
- Spec constraint: servers "MUST NOT rely on prior requests over the same connection to establish context" and
  list endpoints MUST NOT vary per connection or as a side effect of other requests.
- Without auth (local unauthenticated HTTP) no spec-sanctioned per-client identity exists; the only option is a
  client-carried value (handle or cursor passed as a tool argument), which the model must thread.
- Inference (mine, not in spec): "show a notice once per client" maps best to (a) a server event log with a monotonic
  id plus a client-carried `since` cursor returned in tool results, or (b) principal-keyed "last seen" state when
  authenticated. Exactly-once is not achievable against clients that do not echo the cursor or that share a token.

### Q4 stdio
- stdio page: process lifecycle only; "Because the protocol is stateless, any in-flight requests are simply lost and
  the client can retry them against the fresh process." Subscriptions: "the server holds no subscription state across
  reconnections." https://modelcontextprotocol.io/specification/2026-07-28/basic/transports/stdio.md
- Statelessness note: "an open connection, such as a STDIO process, is not a conversation or session ... a server
  must not treat connection or process identity as a proxy for conversation or session continuity." Clients "SHOULD
  NOT use an individual task, thread, or conversation as the lifetime boundary for the stdio process."
- SEP-2567 backward-compat: stdio servers using process-lifetime state "are not broken ... the process lifetime still
  exists", but "SHOULD NOT rely on process-lifetime state and SHOULD migrate to explicit handles".
- Dual-era server: "An `initialize` request selects legacy semantics, scoped to the stdio process (stdio) or the
  session (HTTP)" (versioning page). So legacy stdio keeps a process-scoped session; modern stdio does not.
- Verdict: no, 2026-07-28 does not define connection/session semantics for stdio; process-scoped state still works in
  practice (one process per spawning client) but is discouraged and may span many conversations.

### Q5 Rust SDKs (crates.io API read 2026-11-15)
| crate | latest | era(s) | sessions for 2026-07-28 clients |
|---|---|---|---|
| rmcp (official, modelcontextprotocol/rust-sdk) | 3.10.0 (2026-11-12); 3.9.0 on disk | all KNOWN_VERSIONS incl. 2026-07-28 and legacy | No. Legacy sessions only for `initialize` clients |
| rust-mcp-sdk (rust-mcp-stack) | 2.0.0 (2026-08-27) | 2026-07-28 only; 1.x LTS = 2025-11-25 | No sessions on 2.x (README: "2026-07-28 (stateless)") |
| pmcp (paiml) | 2.23.0 (2026-10-28) | dual: 2025-11-25 and 2026-07-28 per request, default accept-list is v1-only | v1 clients get sessions; v2 clients none ("a v1 client with sessions and a v2 client without them, concurrently") |
| turbomcp (Epistates) | 3.6.0 (2026-10-01) | 2025-06-18, 2025-11-25, 2026-07-28 | sessions for earlier revisions (handshake); modern stateless |
| mcpkit | 0.7.0 (2026-07-27) | README names 2025-11-25 only | n/a |
| mcp-attr | 0.0.7 (2025-05-29) | 2025-06-18 | stale, pre-2026 |
- No library can keep protocol sessions for a client that negotiates 2026-07-28: the spec removes the header and
  says a server that supports only this revision "SHOULD ... ignore" an `Mcp-Session-Id` header and "do not mint or
  echo session IDs". A spec-compliant SDK can only offer sessions on the legacy revisions. A non-compliant private
  session layer (own header, own cookie) is possible but is not MCP.
- Sources: https://crates.io/api/v1/crates/{rmcp,rust-mcp-sdk,pmcp,turbomcp,mcpkit,mcp-attr};
  https://github.com/rust-mcp-stack/rust-mcp-sdk (README version matrix, CHANGELOG 2.0.0);
  https://github.com/paiml/pmcp (README "Protocol Versions");
  https://github.com/Epistates/turbomcp (README).
- Also noted: rmcp 3.9.0 release notes and 3.6.0 mention 2026-07-28 handling (WebFetch paraphrase, medium trust).

### Q6 rmcp downgrade, and is it advisable
- Verified in rmcp-3.9.0 source:
  - `StreamableHttpServerConfig::legacy_session_mode` (default true): "Only applies to legacy protocol versions
    (< 2026-07-28) ... requests negotiating that version are always served statelessly regardless of this setting."
    (tower.rs 224-231).
  - `ServerHandler::supported_protocol_versions()` defaults to `ProtocolVersion::KNOWN_VERSIONS`; "Override it to narrow
    the set ... advertised by `discover`, bounds what `initialize` negotiation may agree to, and is what per-request
    versions are validated against." `ProtocolVersion::known_up_to(&V_2025_11_25)` is the documented helper
    (handler/server.rs 437-450; model.rs 238-268).
  - Per-request version not in the list yields `unsupported_protocol_version` (-32022) with the supported list
    (handler/server.rs 93-101; service/server.rs 641-659). `initialize` naming 2026-07-28 falls back to the newest
    legacy version the server supports (`negotiate_protocol_version`, service/server.rs 478-509; tower.rs 550-560).
  - So a narrowed list is spec-conformant: versioning page says a server that "has chosen not to support" a known
    version MUST answer `UnsupportedProtocolVersionError` listing supported versions.
- Client reaction (the real risk):
  - Official TS SDK v2 client in `auto` mode: `-32022` whose `supported` list is legacy-only is a "definitive legacy
    signal" and falls back to `initialize` (probeClassifier.ts, `classifyRpcError`). Dual-era clients per the spec
    matrix also fall back. https://github.com/modelcontextprotocol/typescript-sdk (packages/client/src/client/probeClassifier.ts)
  - Claude Code changelog (raw, CHANGELOG.md): 2.1.274 HTTP servers, 2.1.292 stdio, 2.1.295 claude.ai connectors,
    2.1.299 cloud sessions all negotiate 2026-07-28 by default; `MCP_PROTOCOL_NEGOTIATION=legacy` opts out. 2.1.318:
    a stdio server that dies on the version probe is "pinned to the legacy handshake for 7 days". 2.1.323: the v1
    MCP client is removed; `MCP_SDK_GENERATION=v1` now runs the v2 client "in a compatibility mode (earlier
    handshake ...)". Latest seen 2.1.325. https://github.com/anthropics/claude-code/blob/main/CHANGELOG.md
    Not verified: that Claude Code's v2 client falls back from a legacy-only `-32022` the way the TS SDK does.
  - Claude Desktop (Reddit, 2026-08-10, unverified secondary): still on the `initialize` handshake, no `server/discover`.
- Advisability (judgement): works today for clients that keep a legacy path, and the SEP itself says session-reliant
  servers "stay on the current protocol version until they have migrated". Costs: modern-only clients fail to connect
  (spec matrix: modern client + legacy server "Fails"); depends on clients keeping legacy support indefinitely
  (legacy removal timing is not specified; deprecation policy minimum is 12 months for deprecated features, but
  legacy eras are not formally listed there as deprecated - not verified); legacy sessions were already unreliable
  (ChatGPT reportedly one session per tool call per SEP-2567 motivation), so "exactly once per client" is weak even
  there. Recommendation: do not build the feature on protocol sessions.

## Confidence
- Q1 removal and SEP-2567 Final and in released spec: high (spec, SEP page, PR label/merge, release tag all agree).
- Q2 mechanisms and quotes: high (read directly from spec pages).
- Q3 principal-keyed state is legitimate: medium-high (SEP prose and security page; no single normative sentence
  saying "you MAY key state on the token"). Absence of any per-client id on the wire: high.
- Q4 stdio: high for statements quoted; "process-scoped state still works in practice": inference.
- Q5 table: high for versions and dates (crates.io API), medium for era claims (READMEs/changelogs, no code audit
  of pmcp/turbomcp).
- Q6 rmcp mechanics: high (source read). Claude Code fallback on legacy-only -32022: medium-low (changelog hints
  only). Longevity of legacy client support: unknown.

## Gaps
- Did not run Claude Code against a narrowed rmcp server; a quick empirical test is the way to close this.
- Did not read rmcp issue #1326 / PR threads or rust-sdk discussions on sessions.
- Did not audit pmcp/turbomcp code for how they route legacy sessions.
- No evidence found of any plan to add a protocol-level session or per-client id back (roadmap page not read).
- Release notes for rmcp 3.10.0 not checked beyond a WebFetch summary (docs-only per summary).
