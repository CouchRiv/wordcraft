# Ask Agent: common ArtCraft contract

Version: 1.0-design, 2026-10-13. Applies to WordCraft, GridCraft, DeckCraft and PdfCraft.

This is the normative target, not a claim that the four implementations already conform. MUST, SHOULD and MAY express implementation requirements. D103 authorizes one architecture; the concrete schemas and policies below are design recommendations implementing that decision. The companion `ask-agent-audit-20261013.md` records current evidence and required changes.

## 1. Architecture and compatibility

The application owns selection capture, the queue, authenticated connection ownership, request state, review, document changes and Undo. An external agent owns reasoning. The existing connected MCP bridge translates the two common tools to the application control channel. The application calls no model, stores no model credential and depends on no particular agent harness.

Only `selection` and the native document/review adapter vary by application. The request envelope, action model, states, control methods, MCP tool schemas and UI lifecycle MUST be identical. Native command names and document representation remain application specific.

Keep WordCraft #1392 compatible: preserve `agent.listen`, `agent.reply`, `timeoutMs`, `text`, `done`, `{none:true}`, `{id,shown:true,done}`, and `ui.askAgent`. Keep MCP `timeout_s`. Do not introduce competing `message`, `timeoutMs` in MCP, `insertAfter`, `leased`, or `{request:...}` wire dialects. Add fields rather than removing the existing ones. Existing consumers MUST ignore added output fields.

During migration `waiting` and `answered` MAY remain in the legacy UI inspection `state` field. Add canonical `status` everywhere, with `waiting -> pending` and `answered -> ready`; new consumers use `status`. Do not maintain two independent state machines. Existing `document.language` MAY mirror top-level `language`; it MUST agree when both exist. Legacy action `{id,label}` consumers remain usable when the action gains its other fields.

## 2. Authentication and control transport

Use the existing local control service, not a second Ask Agent server. Bind loopback only. A random application-session control token MUST authenticate every connection before any queue access, presence update or document read. Rotate it on application restart, store any required discovery file with owner-only permissions, and never include it in a request selection, document payload, log or prompt. The bridge MUST send it only to verified loopback destinations, including an explicitly configured local tunnel.

Common control envelope is newline-delimited JSON:

```json
{"id":1,"method":"agent.listen","params":{"timeoutMs":30000,"agent":"editor"}}
```

The outer `id` is transport correlation; `result.id` is the Ask Agent request id. Success is `{id,ok:true,result:...}`. Failure is `{id,ok:false,error:"human readable reason",errorCode:"INVALID_PARAMS"}`. Preserve `error` as a string for old clients. WordCraft's top-level `key` and `auth {key}` are the compatibility reference. Applications using `auth {token}` MUST accept `key` as an alias during convergence; their existing token discovery adapters may remain. Authentication aliases are transport migration, not different Ask Agent contracts. A wrong credential closes the connection. No unauthenticated method is exempt.

### Control methods

| Method | Parameters | Result and rule |
|---|---|---|
| `agent.listen` | `{timeoutMs?: number, agent?: string}` | Oldest eligible request directly in `result`, or `{none:true}`. Atomically claims it for this authenticated connection and sets `status:working`. |
| `agent.reply` | `{id: integer, text?: string, done?: boolean, proposal?: string, format?: "text"\|"markdown", comment?: string, error?: string}` | `{id,shown:true,done,status}`; cancellation/supersession returns `{id,cancelled:true,status:"cancelled"}`. Only the owning connection may answer. |
| `ui.askAgent` | `{}` or exactly one of `{instruction:string}`, `{action:string}`, `{cancel:integer}`, `{accept:integer}`, `{discard:integer}`, `{retry:integer}`, `{close:true}` | Open bubble, enqueue, Stop, Accept, Discard, Retry or close respectively. Application-local UI command runs on the UI thread. The control route is authenticated; externally invoked accept/discard/retry are refused with HUMAN_REVIEW_REQUIRED, as specified below. `{retry:id}` returns `{id:newId,retryOf:id}`. Close never alters request state. |
| `ui.inspect` | `{}` | `askAgent:{available,agent,requests,actions}`. Include per-request id/status, scope summary, identity, progress, review decision and error code; do not expose full selection content automatically. |

Accept, Discard and Retry belong to the person. A control token authenticates an agent connection; it does not authorize review decisions. Keep the existing local `ui.askAgent` command shape for native controls/tests, but gate remote review parameters and indirect UI automation so an agent cannot accept its own result or another agent's result. Remote Stop and close may remain available; they never imply rejection or acceptance. A trusted application-local test adapter may exercise review without exposing that authority through the agent channel.

`agent.listen`: default 30,000 ms, maximum 60,000 ms, 0 means immediate poll. Accept finite nonnegative numbers, floor to integral milliseconds, clamp above the maximum. Reject negative values and wrong types. A long poll MUST wait on a connection worker, never the UI thread. Probe peer liveness at most every 500 ms and immediately before delivery. A headless or unsupported instance returns `NO_UI`, with connection instructions; it never silently edits a separate document.

`agent.reply`: `done` defaults to true; a provided non-boolean is invalid. `text` is the latest complete progress note or answer, not an appended delta. At least one of `text`, `proposal`, `error` is required. `done:false` leaves the attempt working. `done:true` makes a successful complete result ready, not accepted. An empty replacement proposal is valid and means deletion; absence of a proposal is different from an empty proposal.

`proposal` is the entire text so far and replaces its prior preview. Default format is `text`; `markdown` is supported only if present in the request's `proposalFormats`. Each partial update remains unapplied. A request MUST use either proposal mode or native staged-edit mode, never both. `replyOnly` rejects proposals and every mutating command, including indirect UI, scripts, Undo/Redo, document replacement and nested operations.

`error` is a nonempty failure reason. It closes the attempt as failed; it cannot be combined with `proposal`, `comment`, or `done:false`. Retain the previous preview and the original for display, Discard and Retry. Failed results cannot be accepted.

`comment` is a closing summary, at most 2,000 characters, valid only with successful `done:true`. Store it with the request. Anchor it exactly once, under the agent's author, only after a real change is accepted; include it in that request's transaction. Reply-only, no-op, failed, stopped and discarded requests create no document comment. Proposal replies may return `comment:"pending"` until acceptance; once created, inspection reports `{id:n}`. A native adapter without comments retains an attributed completion note in the request review record, explicitly labeled as a note rather than a document comment.

### Errors and cancellation

| Stable `errorCode` | Meaning |
|---|---|
| `UNAUTHORIZED` | Missing or wrong control token. |
| `INVALID_PARAMS` | Invalid schema, type, combination or unsupported format. |
| `NO_UI` / `NO_AGENT` | Connected UI unavailable / no present agent when asking. |
| `QUEUE_FULL` | No safe capacity for a new request. |
| `REQUEST_NOT_FOUND` | Unknown or expired request id. |
| `NOT_OWNER` | Another connection owns this attempt. |
| `REQUEST_CLOSED` | A ready/failed/resolved attempt cannot be changed. |
| `STALE_TARGET` | Document identity, target or original content no longer matches. |
| `OUT_OF_SCOPE` | Proposed/native change exceeds the captured target. |
| `MODE_CONFLICT` / `READ_ONLY_REQUEST` | Mixing modes / mutating a reply-only request. |
| `AGENT_DISCONNECTED` / `AGENT_TIMEOUT` | Lost ownership after work started / expired working lease. |
| `APPLY_FAILED` / `REVIEW_CONFLICT` | Atomic apply failed / targeted review cannot safely resolve. |
| `HUMAN_REVIEW_REQUIRED` | An external client attempted a person-only review decision. |
| `HUMAN_SIGNATURE_REQUIRED` | A connected agent attempted signing. |

Transport/parser errors do not change a valid request into failed. `agent.reply {error}` is a domain failure; its successful acknowledgement is `{id,shown:true,done:true,failed:true,status:"failed"}`. A cancelled/superseded reply is a successful cancellation acknowledgement, not a transport failure. The bridge exposes domain errors as MCP `isError:true`, retaining the code and message. MCP JSON-RPC uses -32700 for malformed JSON, -32600 for an invalid request, -32601 for an unknown method and -32602 for invalid method parameters; these are not Ask Agent lifecycle states.

Stop (`ui.askAgent {cancel:id}`) cancels that request exactly once, revokes ownership and releases its lock. It does not implicitly reject existing staged edits. Those remain separately discardable. Stop cannot preempt an already executing atomic native command; recheck cancellation before committing it. All later writes are refused. Replies from its former owner return cancellation without changing preview or document. Keep bounded cancellation/supersession tombstones so delayed responses cannot become a new request.

## 3. Presence, queue and state

Presence belongs to authenticated live connections, not names. Show an agent while it has an active listen, or a valid listen/reply heartbeat on a still-open connection within the last 60 seconds. Invalid replies do not create presence. Remove presence immediately on observed disconnect. A working request by itself does not imply ten minutes of presence. Agents SHOULD send a short progress note immediately and at least every 30 seconds during long work. A working lease expires after 60 seconds without a valid heartbeat and becomes failed with `AGENT_TIMEOUT`; active listens remain present even at their timeout boundary.

FIFO claiming is atomic; one request has one owner. A connection SHOULD finish its current claim before listening again. Native/MCP calls MUST carry the claimed request id automatically, and the server MUST enforce ownership, not trust this client tagging or `agent`'s display name. Never replay a mutating call or reply after an ambiguous lost response; reconnect and inspect. If delivery failed, or the owner disconnected before any progress, proposal or edit, return the attempt to pending after checking target validity. After any work, disconnect becomes failed with its preview retained; do not silently give partially executed work to another agent.

Retain at most 20 requests. Evict only resolved or explicitly dismissed terminal records. Unreviewed ready/failed/cancelled results containing proposals or staged edits MUST remain reachable; if no record can safely be evicted, refuse the new request with `QUEUE_FULL`. Never remove pending or working requests to make space. The request list MUST make every unresolved request reopenable when the bubble is closed. Durable history across application restart is a separate feature; v1 MUST say if the queue is session-only.

| Status | Meaning | Permitted transitions |
|---|---|---|
| `pending` | Sent, waiting for an owner. | working, cancelled; failed if target invalidated. |
| `working` | Claimed, generation/staged edits in progress. | ready, failed, cancelled; pending only on loss before any work. |
| `ready` | Complete result awaiting explicit person decision, or a complete text answer. | review accepted/discarded; a new Retry id, never back to working. |
| `failed` | Attempt failed, previous preview retained. | Discard, new Retry id. |
| `cancelled` | Person stopped it, or attempt superseded. | Discard residual staged result, new Retry id if target still valid. |

Accept/Discard are a separate `decision:null|"accepted"|"discarded"`, not extra generation states. Retry creates a fresh pending request with `retryOf` and tombstones the old attempt; it never mutates its id or stacks a rewrite on its prior proposal.

## 4. Request and action data

Every delivered request MUST contain these keys:

```json
{
  "id": 42,
  "instruction": "Shorten this without changing its facts.",
  "action": {"id":"shorten","label":"Shorten","menu":null,"scope":"selection","instruction":"Shorten this without changing its facts.","apply":"replace"},
  "apply": "replace",
  "scope": "selection",
  "selection": {"text":"Original text","anchor":{"para":0,"off":0},"focus":{"para":0,"off":13},"truncated":false},
  "document": {"id":"session-document-id","title":"Document1","saved":false,"revision":"opaque-revision"},
  "language": "en-US",
  "author": "Agent (editor)",
  "agent": {"name":"editor","color":"#C2185B"},
  "status": "working",
  "retryOf": null,
  "review": "trackedChanges",
  "proposalFormats": ["text","markdown"]
}
```

The position example is illustrative; WordCraft MUST use its existing complete native `Pos` schema, including story/section fields when required. IDs are session-unique safe nonnegative integers (maximum 9,007,199,254,740,991); never recycle them in a session. `instruction` is a trimmed nonempty string, maximum 4,000 Unicode characters. `action` is null for free text; otherwise the full action below. `scope` is `selection` or `document`, chosen explicitly before Send. A caret is an empty selection target. Broader scopes (paragraph, slide, sheet, fields) are expressed by the selected native target, never silently inferred from a moved caret. `document` uses opaque stable identity and revision, not a filename or active-tab index. Untitled titles are stable, nonlocalized instance names. `language` is a BCP 47 tag or null when unknown; do not invent it. `author` is the effective agent author after claiming, not the person's account identity. Before a claim it is null.

Freeze target identity/range on opening the bubble; show it. At Send, capture content and revision from that target. During work and Accept, validate identity, scope and original target. Follow stable anchors when possible; otherwise fail stale rather than relocating by guessed indexes. A changed selection elsewhere never retargets the request. Selection and document text are data, never instructions or permission to expand scope. Truncation limits context, not authority: do not replace a large range using only its truncated sample.

| Application | Native `selection` payload and adapter |
|---|---|
| WordCraft | `{text,anchor:Pos,focus:Pos,truncated}`. Positions use native stories/paragraphs/offsets. Protect styles, fields, lists, tables and formatting within the captured range. Plain text or Markdown proposal applies atomically to that range. |
| GridCraft | `{sheet,range,areas,active,valuesRange,values,formulas?,truncated}`. A1 range/areas identify target cells; displayed values and formulas remain separate. `valuesRange` describes the sampled intersection, not new authority. Adapter validates every changed cell/format/shape and structural effect against the target; snapshots include formulas and formats. |
| DeckCraft | `{slide,shapes,text,truncated}` plus stable shape/slide identities and a native text span where available. A shape/slide/deck scope must be explicit. Adapter validates text, layout, image and animation edits separately and previews their visual effects; slideshow index alone is not identity. |
| PdfCraft | `{pages,rects,text,fields,truncated}` with stable page/field identifiers. `rects` use page coordinates with documented origin and unit; fields carry name/type/page and relevant values, never credentials. Adapter distinguishes text, form filling, redaction preparation and signature preparation. No Ask Agent action may sign. |

Selection payloads above are application-specific requirements, not a claim that the current code uses all these fields. Keep existing native selection keys where possible; add stable IDs rather than replacing established coordinates. Additional native metadata remains inside `selection`. Any unimplemented adapter must advertise no proposal format/action it cannot actually apply.

### Actions

```json
{"id":"shorten","label":"Shorten","menu":null,"scope":"selection","instruction":"Shorten the selected content. Preserve its facts and language.","apply":"replace"}
```

These six keys are required. `id` is a stable nonempty string; `label` is a localizable base label; `menu` is null or a localizable menu label; `scope` is `selection` or `document`; `instruction` is the full plain-language instruction; `apply` is exactly one of the values below. Menu order, submenu definitions and actions are data. Expose the complete supported action list through inspection; no application-specific dispatch is hidden behind an id that only one agent understands. Unsupported actions are hidden/disabled with a reason, never advertised as working.

| `apply` | Meaning, common to all applications |
|---|---|
| `auto` | Free-text/native-operation request. The agent chooses an operation within explicit scope; it does not gain broader authority. Text proposals default to replacement of the selected text target. |
| `replace` | Replace captured content with the accepted result. Preserve unrelated cells, shapes, pages and formatting. |
| `insertBelow` | Insert after the captured target without deleting it. Word: paragraphs after its last paragraph. Grid: an explicit row/block after the selected block, with downstream effects previewed. Deck: content/slide after the selected target. PDF: after a supported page/text target. Hide if the adapter cannot safely express this placement. Do not rename to insertAfter. |
| `replyOnly` | Show text only. No document, history, file, signature or indirect mutating operation. |

Application-specific actions are allowed (formula explanation, clean data, layout, images, animations, form filling). They use this same data model. A native command requiring a structural effect beyond a selection must request explicit document scope before execution. Do not label a textual answer as a native chart/table/layout that was never created.

## 5. MCP schemas (exact common input schemas)

The tool input schemas below are the required common schemas, including keys, types and defaults. Descriptions may be localized; validations may not vary. Additional input properties are rejected. Adapters validate the same rules when invoked through compact `tool_call`.

`wait_for_request`:

```json
{
  "type":"object",
  "properties":{
    "timeout_s":{"type":"number","minimum":0,"default":30,"description":"Seconds to wait; capped at 60."},
    "agent":{"type":"string","description":"Optional display name; otherwise use the initialized MCP client name."}
  },
  "required":[],
  "additionalProperties":false
}
```

`reply_to_request`:

```json
{
  "type":"object",
  "properties":{
    "id":{"type":"integer","minimum":0,"maximum":9007199254740991},
    "text":{"type":"string","maxLength":4000},
    "proposal":{"type":"string"},
    "format":{"type":"string","enum":["text","markdown"],"default":"text"},
    "comment":{"type":"string","maxLength":2000},
    "error":{"type":"string","minLength":1},
    "done":{"type":"boolean","default":true}
  },
  "required":["id"],
  "anyOf":[{"required":["text"]},{"required":["proposal"]},{"required":["error"]}],
  "additionalProperties":false
}
```

Cross-field validation from section 2 applies after schema validation: nonblank error, allowed proposal format, successful final-only comment and no failure/proposal mixture. Proposal maximum is 200,000 UTF-8 bytes; reject an oversized proposal before acceptance, never silently truncate document replacement. Instructions above limit 4,000 Unicode characters; an oversized UI draft must be rejected or visibly shortened before Send, never altered invisibly.

`wait_for_request` converts seconds to `floor(timeout_s*1000)` ms and clamps to 60,000. Agent identity defaults to the bridge's initialized `clientInfo.name` or explicit bridge name. `reply_to_request` forwards exactly the fields provided, applying documented defaults. `wait_for_request` annotations are readOnlyHint:true, destructiveHint:false, openWorldHint:false; reply is readOnlyHint:false, destructiveHint:false, openWorldHint:false. The hint does not mean a wait is stateless: it claims queue ownership.

Both tool results serialize the control method's `result` as JSON in `content:[{type:"text",text:"<JSON>"}]`, with `isError:false`. Errors use `isError:true` and JSON text `{errorCode,error}`. Output values are exactly the method results in section 2 and the request in section 4; there is no extra `{request:...}` wrapper. Never register names that silently run in a headless session instead of the connected UI.

## 6. Review, safety and one Undo step

Show Original / Proposal / Changes, or their visual native equivalent, before Accept. Disable Accept until `status:ready`, a complete applicable result exists and validation succeeds. Failed or streaming previews cannot be accepted. A reply-only answer has no Accept button.

Accept makes the request's result final in both modes. It applies a proposal once at the original target and resolves any revisions created by that application inside the same atomic transaction, or resolves only this request's staged native edits. Keeping an accepted proposal as an additional unreviewed tracked-change step is not the common v1 behavior. WordCraft uses `review.accept {revisions:[...]}`; Discard uses `review.reject {revisions:[...]}`. Validate stable revision IDs, request ownership and author; do not use Accept All, Reject All or author-wide resolution, since one author may own multiple requests. Other applications use a request-bound transaction/patch group with equivalent targeted accept/reject. A whole-document snapshot may support immediate Undo, but MUST NOT overwrite intervening human or other-agent changes to implement delayed Discard.

Discard cancels an open generation first, then drops its unapplied proposal or rejects exactly its staged group. It adds no closing comment. Retry keeps instruction, action, apply, scope, original selection/document/language and a preview of the previous attempt; the previous preview is never accept-enabled while the new attempt streams. Native edits must first be safely rejected as their own group, then retry from the original. If that cannot be done without overwriting unrelated changes, return `REVIEW_CONFLICT` and keep the old result reachable. Never regenerate from already transformed content.

A request's effective document change, including any final comment, is one Undo entry. Repeated streamed proposals do not enter document history. Consecutive native edits accumulate in one request transaction; unrelated human/agent changes must remain separate. If history cannot support safely interleaved transactions, stop further writes for that request and report a conflict. Accept/reject of a revision group is one Undo entry. Atomic apply failure restores document, selection and Undo together. Undo is independent of generation Stop.

### Identity

`agent.listen {agent}` names an agent but grants no permission. Clean display names by removing control characters, folding whitespace and keeping at most 40 Unicode characters. Fallback display name is `Agent`; author is `Agent (name)` for named agents, otherwise `Agent`. Preserve the person's author and tracking preferences. A named agent has a stable color derived from its cleaned name with one common palette/algorithm; reuse WordCraft (d)'s `agent_color` implementation across adapters. Precisely: trim the cleaned name, Unicode-lowercase its characters, encode their UTF-8 bytes, then FNV-1a32 (offset 0x811C9DC5, XOR each byte, wrapping multiplication by 0x01000193). Index hash modulo 8 into [#C2185B, #1565C0, #2E7D32, #D84315, #6A1B9A, #00838F, #9E7700, #455A64]. Serialize colors as uppercase `#RRGGBB`; unnamed pending requests use neutral UI color and `agent:null`. Colors appear on its queue row, bubble, range marker, revisions and comments. Always display the name as well; color alone does not identify ownership. Color is display metadata, never a lock key.

### PdfCraft: the agent prepares, the person signs

An agent may explain, fill supported fields, prepare a signature location and prepare redaction proposals within explicit scope. It MUST NOT cryptographically sign, submit, use a reusable signature image, access a private key/password, or turn Accept into a signature operation. The person signs through a distinct native user interaction. Refuse signing for every connected agent before any claim and on parallel connections, at both MCP and control/native dispatch boundaries, including compact and nested calls and synthetic UI commands/clicks/keys, with `HUMAN_SIGNATURE_REQUIRED`. Never include secret key paths, key material or signature images in selection/action/request data.

Redaction preparation is reversible preview. Permanent redaction, flattening or save/export that destroys originals requires a separately identified native human confirmation with a preview. Stop/Discard of preparation never performs irreversible redaction. Preserve the original file and verify saved output separately when that workflow is implemented.

## 7. Common UI

| Interaction | Required behavior |
|---|---|
| Selection chip | Display beside a meaningful selected target only when an agent is present, within the viewport and without covering the selected content. A single selected cell/shape/field qualifies. Hide during drag or higher-priority overlays. |
| Bubble | Anchor to captured target, show its scope, instruction/action, name/color, status, elapsed time and progress. Reposition inside the viewport. Expose queue/reopen even after closing. |
| Enter | In instruction field: Send nonempty instruction. Shift+Enter inserts newline. In review: activates Accept only when its button has deliberately acquired focus. Completion never steals typing focus or turns a typing Enter into acceptance. |
| Escape / close | Close submenu first, then bubble; return focus to the document. Never Stop, Accept, reject edits or discard a result. Preserve unsent draft for the session. |
| Ctrl+R / Cmd+R | Retry a ready or failed attempt only when its review bubble/control has focus. Never intercept globally, in the instruction editor, or while working; Stop is explicit. |
| Resume document typing | Close the floating bubble, preserve draft/result/request and keep ongoing work visible in the queue. Do not cancel generation or reject edits. |
| Stop | Explicit named control for pending/working requests; separate from close and Discard. |
| Accessibility | Keyboard reachable chip/actions/buttons, visible focus, accessible names and translated labels. Status is expressed in text, not color alone. Avoid covered focus and support large text/reduced motion. |

Menus, fonts and native rendering may follow existing application UI systems. UI semantics and shortcuts above do not vary. Suggested actions and previews must reflect actual capabilities, not aspirational native objects.

## 8. Minimum conformance checks

1. Same MCP schemas and same control parameters/results in all four apps; normal and compact calls validate identically.
2. Wrong token creates no presence or queue access; only the claiming connection can write/reply.
3. Long poll never blocks UI; disconnect before/after progress and heartbeat expiry follow section 3.
4. Queue saturation preserves all active and unresolved requests. Stop and late/superseded replies cannot mutate results.
5. Move selection/switch documents/edit target before reply and Accept: no other range/document is changed.
6. Stream multiple proposals; no document edit before Accept; an empty replacement is deletable; oversize replacement is rejected.
7. Two requests from the same agent plus human edits: Discard/Retry/Undo affects only the intended group.
8. Escape/typing preserve draft and result; Enter cannot accept after a focus-stealing completion; Ctrl+R is review-only.
9. Comment is created once after an effective accepted change, never on Discard or replyOnly.
10. Every PDF signing path, including compact/nested calls, is denied to connected agents; native human signing stays separate.

## Design provenance

Internal architecture decision D103 (2026-10-13), decision table; its full local citation is in the companion audit. Compatibility baseline: WordCraft `origin/feat/ask-agent-selection` 301b893 and `origin/feat/ask-agent-quick-actions` 5442d7a, `docs/control-protocol.md` Ask Agent, `crates/ui-egui/src/agent_actions.rs:10-47`. Proposal/review baseline: `wc-ask-c2` WIP on 681153e, `ask_agent.rs:127-224,1022-1138`, `tools.rs:155-177`. Identity/comment baseline: `wc-ask-d` WIP on cd8f6ec, `agent_queue.rs:48-54,135-166`, `control_server.rs:139-207`. UI rationale: local Canvas comparison, sections “Avis détaillé sur Échap, Entrée et Ctrl+R” and “Transposition aux autres Craft”. New stable error codes, strict MCP schemas, heartbeat lease, canonical status and stable document metadata are this contract's proposed convergence rules, not assertions about existing code. See the dated audit for complete source inventory and limitations.
