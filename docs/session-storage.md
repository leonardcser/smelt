# Session storage

Every saved session lives in one SQLite database, `<state>/smelt.db`, defined by
`crates/store/src/schema.sql`. Sessions are flat rows keyed by session
and position. A save replaces only the changed suffix of each sequence, so its cost
follows the new content, not the session size.

## Layout

| Table | Contents |
| --- | --- |
| `sessions` | Identity, parent, metadata, and the running `history_len`, `record_count` and `text_bytes` totals |
| `session_values` | Small named values such as the first user message and the active checkpoint |
| `history` | One row per model history item, with the cumulative user ordinal, mode and note columns that indexed queries need |
| `side_rows` | Turn metadata, metadata snapshots, context snapshots and checkpoint events, keyed by history position |
| `transcript` | One row per transcript block, with cumulative row estimates at six widths, the compact summary a sparse transcript keeps in memory, its search text and one payload of block, origin and tool state |
| `blobs` | Large JSON string contents, once each across the whole database, keyed by SHA-256 |
| `blob_refs` | Which history, transcript and request rows hold which blobs |
| `search_text` | Conversation text of each history item that has any: user input, assistant replies and compaction summaries |
| `search` | FTS5 trigram index over `search_text`, kept in step by triggers |
| `requests` | Request audit entries with `body`, `response` and `error` payloads |
| `prompts` | Submitted prompts for input history recall |
| `usage` | Tokens and cost per model call; calls before yesterday fold into one row per day and model |

Files a session owns, such as Claude Code plans and its native session link, live in
`<state>/artifacts/<id>/`.

History bodies, transcript payloads and search text, and request payloads are
documents (`crates/store/src/document.rs`). A document is JSON whose strings of
8 KiB or more live in `blobs`: the row keeps the rest, compressed with zstd when
that saves at least an eighth, and the offsets to splice the strings back. The
same tool output read twice, the history and transcript copies of one output, a
repeated image and every row a fork copies share one blob. A save drops the
references of the rows it replaces and deletes the blobs no row holds anymore,
in the same transaction. Text and tool output shrink about five times; base64
images and encrypted reasoning barely compress and dominate what real sessions
keep on disk.

A record's preview is the first 512 bytes of its search text. The WAL is
checkpointed at 1 MiB and truncated back to that size. Readers query rows by
position or by indexed columns and decode only the rows they return.

Each session has an exclusive writer lock at `<state>/locks/<id>.lock`. A
second process that opens the same session gets `StoreError::Owned` and opens it
read-only. Readers never take the lock.

Deleting a session frees the blobs only it held and runs `PRAGMA
incremental_vacuum`, so the file shrinks in bounded steps. Pages freed by suffix
rewrites, such as a rewind, are reused by later saves.

## Saves

`crates/tui/src/app/session_document.rs` tracks what changed since the last durable
save and captures a `SessionCommit`: new metadata, an optional suffix for history,
each side table and the transcript, and optional named values. The capture copies
only the changed rows.

`crates/tui/src/persist.rs` owns the writer thread. It commits saves in capture
order, one SQLite transaction each. If a commit fails, the saves captured after it
no longer apply; the writer skips them until the document sends a resync save that
rewrites everything after the last durable head. A failed save blocks automatic
saves until the user retries (`smelt.session.retry_persistence()`).

Live model history stays in memory, so turns dispatch at once and never wait for
storage. Only `@file` input preparation and writer startup can reject an accepted
submission. A crash mid-turn loses at most the unsaved suffix; the next writable
start marks the turn `interrupted`. smelt never resends it automatically.

Forks copy the source rows with `INSERT ... SELECT` in one transaction. The copies
refer to the source's blobs, so a fork costs the rows without their large strings.
A fork that preserves unsaved edits then commits those edits as a suffix of the copy.

`SessionWriter::batch` applies several commits as one atomic save, so an import
writes a session of any size in bounded chunks.

## Reads

Resume reads the session header, side rows, the model context from the active
checkpoint on, and the transcript tail that fills the viewport. Older transcript
blocks load on demand as the user scrolls, and evicted blocks leave memory again.
Transcript search reads the session's search text in block order and stops at its
page limit. A record whose stored JSON lacks the JSON form of the query is skipped
without decoding it. Finding sessions by their conversation text
(`smelt.session.search`, used by `/resume`) queries the `search` index instead of
reading sessions; queries need at least three characters.

## Session operations

Rewind browsing reads pages of at most 128 user profiles, using incrementally
maintained live navigation and the stored transcript index. It does not scan
assistant history or hydrate transcript bodies. Each page captures its request
once; pages covered by live navigation do not open storage. Stored pages use at
most two physical readers. Lua task cancellation stops waiting requests and is
checked between indexed profile reads in one snapshot. Running readers keep their
capacity until the physical worker exits, and cancelled results are not published.
Independent page requests do not supersede one another.

Selection uses one preparation plan for input, mode and the viewport transcript
boundary. It applies directly only when all dependencies are live. Otherwise a
worker prepares them in one consistent read snapshot, including live targets
that depend on stored mode or transcript data. The UI applies the result only if
its request, session, history epoch, history length, cancellation generation and
draft identity are still current.

Resume and rewind share a bounded operation scheduler, with independent
cancellation lanes. Each lane has two workers and one replaceable waiting slot.
A new request can pass one stalled read. Workers check supersession between
reads and suppress cancelled results; cancellation does not preempt an individual
read. Dropping a scheduler signals shutdown without joining workers on the UI.

Asynchronous resume keeps the source document and writer owned while exact
cancellation commands settle, then starts a pollable close. It sends finalization
permission only after the captured generation is durable, then waits for physical
release without blocking the UI. The loading
indicator animates while typing and debug/performance panels remain available.
Cancellation before permission resumes the actor; cancellation after permission
waits for release and reopens the source. Superseded loads cannot adopt an old
destination. The initial immutable preparation is retained until release. A
bounded worker then checks the destination's authoritative head and current layout
inputs. Matching head, width and target-row count reuse that preparation; any
change triggers a full reload, including same-session loads whose first snapshot
preceded the final save. `session:resume:prepare` measures full preparations, while
`session:resume:reused` records successful reuse. Source mutations after the close
captures its generation cancel adoption instead of discarding new work.

Lua signal callbacks, deferred commands, window invocations, task-resume payload
delivery and task execution share a nested count/time budget. Callback and payload
queues have batch limits, and ready tasks rotate in FIFO order. Task identity,
ready order, timers and external waits have exact indexes; recurring ready, wakeup
and external-resolution lookups do not scan parked tasks. Each work unit promotes
at most one due timer behind already-ready tasks, so timer bursts stay within the
pump budget. Payload delivery alternates between owner-thread registry values and
cross-thread JSON, preserving each inbox's FIFO order. Remaining work wakes the event loop instead of being
discarded. Starting a command steps only that command, not unrelated tasks. A
single callback, payload conversion or coroutine resume is not preempted by this
budget.

All `session.info()` user counts come from maintained navigation metadata, without
opening storage, enumerating turns or constructing provider messages. Stored
sessions retain one prefix count and global ordinals for live users. Append,
truncation and compaction maintain those counts without re-ranking the retained
suffix. Metadata-only saves preserve the prefix count, and older acknowledgements
cannot erase newer users.

The canonical history index stores cumulative user ordinals. Exact prefix counts
require a bounded lookup without history-body reads, including users without
transcript records and excluding synthetic User messages. Changed suffixes inherit the retained prefix ordinal. Resume captures the initial count,
header, side rows and transcript tail in one SQLite snapshot. Rewind workers capture
the boundary count in their existing snapshot; application does not read storage.
Explicit synchronous history
replacement prepares its count before mutation and fails without appending a suffix
if preparation fails.

These boundaries do not make every session operation nonblocking. Explicit
synchronous replacement helpers keep their bounded close deadline. Full-history
materialization is a cold operation.

## Schema

A smelt database has `PRAGMA application_id` "smlt" and `PRAGMA user_version`
`SCHEMA_VERSION` from `crates/store/src/db.rs`, both checked on open. An empty
file is created with the current schema; any other file is refused without
writing to it.

A later schema change must not rewrite the whole database while every process
waits: add columns and tables, and convert existing rows lazily, per session.

## Scaling

`cargo run --release -p xtask -- bench-store` builds 300 sessions of 60 turns and
one of 4,000, with realistic tool output, reasoning and images, and checks that the
interactions users wait on stay flat. `--state-dir` measures a copy of a real
state directory instead; `--mutate` also forks and deletes its largest session.

| Measurement (synthetic, 2.1 GB of JSON) | Result |
| --- | --- |
| Database size | 417 MiB |
| Save, first tenth of the long session / last tenth (p95) | 19 ms / 26 ms |
| Resume of the long session (p95) | 17 ms |
| Session search: rare / common / absent word (p95) | 0.8 / 28 / 0.4 ms |
| Transcript search over 16,000 records: hit / full miss | 1.8 / 133 ms |
| Fork / delete of the long session | 0.61 / 0.47 s |

On a copy of 391 real sessions (2.7 GB in the previous layout, 1.8 GB now), the
largest session resumes in 59 ms at p95, session search stays below 2 ms, and
forking it takes 0.56 s.

## Maintenance

```sh
smelt session doctor [<SESSION>] [--json]
smelt session backup <OUTPUT>
smelt session migrate
```

`doctor` runs `PRAGMA quick_check` and, for one session, reports its row counts and
stored bytes. `backup` writes a consistent copy of the whole database with
`VACUUM INTO` and never overwrites an existing file. Both are safe while smelt
runs. `migrate` imports smelt 0.7 sessions from `<state>/v0.7/`; smelt also runs
it on its first start. Session bodies, credentials and real-data errors must not appear in
evidence logs.
