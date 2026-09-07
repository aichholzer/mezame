# Wire protocol

Mezame speaks JSON text frames to the browser over `/ws`, plus three plain HTTP
endpoints. This document is the catalogue of those messages, for contributors
and for anyone plumbing a new client. The README covers the higher-level
architecture.

Eight server events, four client commands. Nothing outside those two sets is
sent or accepted.

## Browser to Mezame (JSON text frames over `/ws`)

```json
{ "type": "prompt", "blocks": [{ "type": "text", "text": "look at this" },
                               { "type": "image", "mimeType": "image/png", "data": "iVBOR..." }] }
{ "type": "cancel" }
{ "type": "permission_response", "id": <the id from the request>, "optionId": "allow_once" }
{ "type": "set_model", "modelId": "claude-sonnet-4.5" }
```

Required fields, and what happens without them:

| Command | Requires |
| --- | --- |
| `prompt` | `blocks`, a JSON array |
| `permission_response` | `id`, a string or a number, and `optionId`, a string |
| `cancel` | nothing beyond `type` |
| `set_model` | `modelId`, a string |

A frame that is not a JSON object, whose `type` is absent, is not a string, or
names no command above, or that names a command with a required field absent or
of the wrong JSON type, is discarded. Nothing is emitted in response, nothing
is invoked, and the connection stays open. One line goes to Mezame's stderr per
discarded frame. The four faults are indistinguishable to a client on purpose:
an error frame would raise a notice for something the user never composed, and
closing the connection would evict a browser over one bad frame during a
version skew.

A field the set does not declare for its `type` is ignored, so a client one
version ahead is tolerated.

An empty `blocks` array is accepted and does nothing. A `prompt` arriving while
a turn is already in flight on that session is discarded, so a client should
keep its composer locked between the user echo and `prompt_done`.

### The `prompt` block vocabulary

```json
{ "type": "text", "text": "..." }
{ "type": "image", "mimeType": "image/png", "data": "<base64>" }
{ "type": "resource", "resource": { "uri": "file:///x", "mimeType": "text/plain", "text": "..." } }
{ "type": "resource", "resource": { "uri": "file:///x", "mimeType": "image/png", "blob": "<base64>" } }
```

A `resource` nests `uri`, an optional `mimeType`, and exactly one of `text` or
`blob`. Mezame forwards every member of the array unchecked: this vocabulary
describes what a client sends, not a server-side validation step.

## Mezame to browser

```json
{ "type": "ready", "sessionId": "<32 hex characters>", "resumed": true, "busy": false,
  "cwd": "<absolute path>", "buildId": "<token>",
  "promptCapabilities": { "image": true, "audio": false, "embeddedContext": true } }
{ "type": "session_info", "info": { "models": { "currentModelId": "...", "availableModels": [...] } } }
{ "type": "append", "role": "user" | "agent" | "sys", "text": "..." }
{ "type": "thought", "text": "..." }
{ "type": "tool_call", "toolCallId": "...", "title": "...",
  "status": "pending" | "in_progress" | "completed" | "failed",
  "kind": "..." | null, "rawInput": {...}, "content": [...] | null, "locations": [...] | null }
{ "type": "permission_request", "id": <minted id>, "title": "...", "options": [...] }
{ "type": "prompt_done" }
{ "type": "error", "message": "..." }
```

Each event's field list is closed. `append` declares exactly `type`, `role` and
`text`. `thought` declares exactly `type` and `text`, though nothing streams one
in this release. `error` declares exactly `type` and `message`. `prompt_done`
declares nothing beyond `type`, and in particular no usage figures yet.
`session_info.info` declares exactly one key, `models`.

`permission_request` may carry one extra field, `_target`, holding the internal
id of the connection that started the turn. Mezame drops the frame on every
other connection, so a client only ever sees a card it was asked to answer. The
field arrives on the forwarded frame and a client ignores it.

Details:

- **`ready`** is the first frame on every connection, before any other. Fields:
  - `sessionId`, the id Mezame bound this connection to. Minted when the
    upgrade carried no `session` query parameter; otherwise the value supplied.
    A client persists it and sends it back as `?session=<id>` on reconnect.
  - `resumed`, always `true`. A connection is always a join to a session that
    already exists. A client reads it as "clear any stale local log and seed
    from `/history`", and should do so only on the first `ready` after a page
    load: a transient reconnect would otherwise rebuild the log underneath the
    user.
  - `busy`, whether a turn is in flight on this session at this moment. When
    `true` the composer opens read-only, and Mezame guarantees that this
    connection also receives that turn's `prompt_done`, which is what unlocks
    it again. When `false` the composer opens editable.
  - `cwd`, the absolute path of the directory the Mezame process runs in.
    Display only.
  - `promptCapabilities`, exactly
    `{"image": true, "audio": false, "embeddedContext": true}`. The composer
    gates its paste, drop and upload affordances on it.
  - `buildId`, a non-empty token fixed at compile time. A client compares it
    against its own to detect a stale cached bundle.
- **`session_info`** arrives immediately after `ready` when a model set is
  known, and again after every successful `set_model`. `info.models` carries
  `currentModelId`, a string, and `availableModels`, an array whose members
  carry `modelId` and optionally `name` and `description`. Drives the model
  picker.
- **`append`** with `role` `user` is the echo Mezame broadcasts when any
  connected browser sends a prompt, and Mezame is its only producer. It is what
  tells every other browser a turn has started; a client appends a user turn to
  its log from this frame or from a history entry, never from its own send. The
  echo text is the `text` fields of the prompt's text blocks in order, joined by
  one newline, prefixed once with `> `, and followed by one newline. `image` and
  `resource` blocks contribute nothing, so a prompt with no text block echoes
  `"> \n"`.
- **`append`** with `role` `agent` or `sys` is the streaming path for a turn and
  for a notice. A failed `set_model` arrives as a `sys` append and no
  `session_info` follows it.
- **`thought`** carries reasoning text. A client merges consecutive frames into
  one collapsible block and closes it on `prompt_done` or `error`.
- **`tool_call`** carries the whole payload. Frames for one `toolCallId` merge
  into the existing row rather than appending a new one, and a field holding
  JSON null means "no change": a client keeps the value it already has for
  `kind`, `rawInput`, `content` and `locations`. `content`, when present, is an
  array whose members carry `type` `text` and a `text` string. `locations`, when
  present, is an array whose members carry a `path` string and optionally a
  `line` integer of zero or more.
- **`permission_request`** renders an inline card, one button per option. An
  option carries `optionId` and optionally `name` and `kind`. The `id` is minted
  by Mezame and unique for the life of the session. The user's click returns a
  `permission_response` naming the matching `optionId`. The first answer for an
  id wins; later answers for it, including from another browser, are dropped in
  silence.
- **`prompt_done`** ends a turn. Every event the turn produced is broadcast
  before it, and exactly one arrives per turn, after a failure as well as a
  success. It is what unlocks every composer on the session.
- **`error`** precedes `prompt_done` when a turn failed. `message` holds the
  failure text.

Nothing streams a `thought`, a `tool_call` or a `permission_request` in the
0.14.0-alpha.1 release: it answers every prompt with an echo. Their shapes are
fixed here so a client written now keeps working when the provider loop lands.

## Session history

`GET /history?session=<id>` returns the session's transcript.

```json
{ "entries": [
  { "role": "user", "text": "hello", "timestamp": 1730000000000 },
  { "role": "agent", "text": "hello", "timestamp": 1730000000000 },
  { "role": "tool_call", "toolCallId": "...", "title": "...", "status": "completed",
    "kind": null, "rawInput": {...}, "content": null, "locations": null,
    "timestamp": 1730000000000 }
] }
```

A text entry carries `role`, one of `user`, `agent`, `sys` or `thought`, a
`text` string, and a `timestamp` in milliseconds since the Unix epoch. A `user`
entry holds neither the `> ` prefix nor the trailing newline of the echo: a
client adds both when it renders, so the rendered line matches the live echo
byte for byte.

A tool-call entry is the `tool_call` event with `type` renamed to `role` and a
`timestamp` added, with the same nullability in every field.

Entries come back in recorded order, with no pagination. The transcript is
bounded: the shipped Backend retains at most 16 MiB of entry text and 10,000
entries, evicting the oldest turn first and always keeping the newest, so a
long conversation returns its most recent window. An absent or empty `session`
answers 400 with a plain-text body. An id with no live
session answers 200 with an empty array, and creates nothing; that covers a
value holding `/`, `\` or `..`, since no such value is ever bound to a session.
The endpoint answers 200 or 400 and nothing else. It reads no file and never
consults `HOME`.

A transcript lives as long as its session, which outlives the last browser by
the grace window and no longer. A reload inside that window shows the
conversation; one after it shows an empty log.

## Limits

These ceilings bound what a peer can put into Mezame's memory. None of them is
reachable from the composer in ordinary use.

- One inbound WebSocket message is at most 32 MiB. The browser allows 20 MB of
  attachments per prompt, which base64 renders as a little under 27 MiB, and
  the rest is headroom for the text and the JSON around it. A frame announcing
  more ends the connection before its payload is read; the browser reconnects
  and the session is untouched.
- The text of one prompt, the text blocks joined, is at most 1 MiB. A prompt
  past that is answered with an `error` frame delivered to its sender alone,
  and nothing else happens: no echo, no turn, no Backend call. Attachments are
  not text; the message ceiling bounds them.
- A transcript retains at most 16 MiB of entry text and 10,000 entries, the
  oldest turn evicted first and the newest always kept.
- At most 128 sessions are live at once, a session counting from its first
  attach to the end of its 30-second grace window. An upgrade for a new session
  past that is answered 503 with `Retry-After: 30` before the handshake; an
  upgrade naming a live session is unaffected, and `/history` creates nothing.
  A peer sustaining about four handshakes a second can hold the cap and deny
  new sessions for as long as it keeps going; it cannot touch live ones.
- For each attached browser at most 256 frames wait to be written, and one
  frame may wait 60 seconds for the browser's TCP stack to accept it. A browser
  that lets the queue fill, or accepts no frame for a minute, has stopped
  reading: its connection is closed, the session is untouched, and its
  reconnect seeds from `/history`. A live browser on a slow link receiving a
  large echo can trip the minute; it reconnects the same way.
- A connection that sends no complete request head within 30 seconds, or that
  stays idle between requests for 30 seconds, is closed. An upgraded WebSocket
  and a streaming `/state/events` response are not subject to this; the timer
  runs only while a request head is awaited.

## Request checks

Two checks run ahead of every route. A request that fails one is answered with
a plain-text body naming the value refused, and reaches no handler.

- `Host` must name this server: an IP literal, `localhost`, a `.localhost` or
  `.local` name, the host part of the bind address, or a hostname listed under
  `hosts` in the transport config. Anything else is answered 421. An IP
  literal cannot be rebound through DNS; a name can, so names are allowlisted.
- On a WebSocket upgrade and on any request whose method is not `GET` or
  `HEAD`, an `Origin` header, when present, must name the host and port the
  request was sent to (the port ignored when `Host` names none), or a hostname
  listed under `hosts`. Anything else, `null` included, is answered 403. A
  request without `Origin` passes: a browser sends one on every request this
  check covers, so its absence means a client that is not a browser.

A plain `GET` carries no `Origin` check. The browser withholds a cross-origin
response on its own, and the `Host` check covers the rebound page that would
otherwise read it as same-origin.

## Session identity

A session id is exactly 32 lowercase hexadecimal characters: the form `/ws`
mints, and the only form it accepts.

`GET /ws` with no `session` query parameter mints one: 16 bytes of operating
system entropy as 32 lowercase hexadecimal characters. With a `session`
parameter whose trimmed value is of that form, that value is used and nothing
is minted. With a trimmed value that is non-empty and of any other form, a
name a user typed or the minted form in upper case included, the upgrade is
refused with a 400 before the handshake: no WebSocket is established and no
session is created.

A `cwd` query parameter is not read. A session always runs in Mezame's own
working directory.

## Cross-device UI state

`GET` and `PUT /state` persist the open-tabs list, the recently-closed history,
the active tab, and the numeric label counter. The backing file is
`~/.mezame/state.json`. Any browser reaching this Mezame sees the same list,
which is what keeps your phone and your laptop in step. Mezame does not
interpret the contents; it stores labels, session ids, and a `settings` object
the client owns for app-wide preferences such as the theme and the notification
choice.

A `GET` with no file present, or with a file that does not parse as JSON,
answers 200 with `{}`. A `PUT` with a body that parses as JSON writes a sibling
temporary file and renames it over the target, so a reader never sees a partial
file, and answers 204. A failed write leaves any existing file alone and fires
no event.

`GET /state/events` is a Server-Sent Events stream that emits one
`state_changed` event per successful `PUT /state`. Browsers read it as a "go
refetch `/state`" signal, so a session opened in another browser shows up with
no manual reload. A keep-alive comment every 15 seconds stops an intermediary
idle-timing out the stream.

Neither endpoint applies per-user scoping and neither requires
authentication, which is the same posture the 0.13 release ships.
