# Agent teams

Each visible conversation is a level-0 agent and owns one runtime team. An agent may use
`spawn_agent` to create a direct child at the next level. Children load the same team tool
extension, so they may create another level while the configured depth allows it.

The spawning agent chooses the child's `name`, `role`, and `task`. It may also choose a
configured `provider` and `model`. If neither is supplied, the child inherits the caller's
current provider and model. A running child cannot switch models through the team tools.

## Configuration

General settings exposes:

- **Maximum depth**: highest permitted subagent level. Level 0 is the visible owner.
- **Maximum agents per level**: one shared limit applied independently to every configured
  level across the whole team, not per parent. Completed, failed, and interrupted agents no
  longer consume capacity.

Depth is bounded to 8 and each level to 64 active agents. Quota checking and reservation
happen under one short supervisor lock, so concurrent creation cannot exceed a limit.

## Routing policy

`team_id`, `parent_id`, `parent_session_id`, and `level` come from the authenticated
supervisor identity and cannot be supplied by a model. Every agent also has a stable
`session_id`; the level-0 value is the same UUID shown in the sidebar for that JSONL session.

| Sender and target relationship | Allowed behavior |
| --- | --- |
| Two level-0 sessions, including different teams | Peer message or persistent offline delivery |
| Subagents in the same team, same level, same direct parent | Peer message |
| Direct parent and child | Direct notification |
| Different levels without a direct parent-child edge | Denied |
| Same level with different parents | Denied |
| Different teams below level 0 | Denied without revealing the target |

An agent can list only itself, its siblings, its direct parent, and its direct children. Names
must be unique among active siblings. `list_agents` returns both the runtime `agent_id` and
the stable `session_id`; a same-level peer can be addressed by either ID. Level-0 sessions form
the user-visible coordination plane and can exchange messages across runtime teams. Active roots
receive immediately; completed roots use a bounded persistent mailbox and receive queued messages
the next time that session resumes. Agents below level 0 remain isolated by team and parent.

`read_session` accepts any known session ID and is deliberately read-only. It can inspect
same-team agents outside the caller's messaging neighborhood and sessions from another team.
Level-0 callers may message another root; subagents do not gain cross-team access. By default,
the tool returns only every user input and the final agent report for each turn, excluding
intermediate reasoning and tool activity. `range: "last_turn"` returns only the latest input and
report. `start_turn` and `end_turn` select an inclusive 1-based turn range, while
`detail: "full"` includes retained assistant steps and selected fields for that range.
`start_entry_id` and `end_entry_id` narrow the result to an inclusive
entry range. When the bounded response is truncated, its `next_entry_id` can be passed back as
`start_entry_id` to continue. The response includes selection and truncation metadata.

`detail: "full"` is filtered by default: it includes tool calls, tool results, and peer events,
but omits thinking, usage, and provider/model metadata. Pass `include` with any of `thinking`,
`tool_calls`, `tool_results`, `usage`, `metadata`, or `peer_events` to request specific fields.

Session read failures use stable error codes: `session_not_found` means the ID is unknown,
`session_expired` means a bounded child snapshot or historical file is no longer retained, and
`session_forbidden` means the session exists but its history cannot be read by this process.

Root conversations are read lazily from their bounded JSONL history; child transcripts stay in
a bounded supervisor catalog and never enter the sidebar. Full child transcripts are captured
from bounded JSON events without creating child session files.

## Model-facing tools

- `spawn_agent`: asynchronously starts a specified direct child and returns its ID.
- `send_message`: sends an authorized peer message by session ID, direct notification, or a
  bounded `target: "all_children"` broadcast.
- `list_agents`: lists the caller's authorized coordination neighborhood; defaults to active
  agents and accepts a status filter such as `completed` or `all`.
- `list_sessions`: discovers retained sessions, including historical level-0 JSONL sessions and
  bounded child snapshots. It is paginated with `offset` and `limit`; `status: "active"` is an
  alias for starting or running agents.
- `search_sessions`: performs a case-insensitive, bounded search across retained task text,
  conversation content, and selected entry details. It returns session IDs, entry IDs, roles,
  turns, and snippets so the caller can follow up with `read_session`.
- `read_messages`: consumes queued messages and notifications.
- `read_session`: reads a session by stable ID with compact reports by default, optional last-turn
  or inclusive turn-range selection, and an explicit full-detail mode.
- `wait_agent`: waits for a specific direct child's entire subtree to finish, receives a direct
  notification, or reaches a bounded timeout. It returns that child's queued messages plus the
  outcomes of all descendants, leaving unrelated child messages available for their own
  wait/read call.
- `interrupt_agent`: stops a direct child and cascades to all descendants.
- `grep` and `find`: Pi's native ripgrep and fd-backed project search tools. They are available
  to child agents only when their effective `enabled_tools` policy includes the corresponding
  tool name; read-only agents may use both. Search paths resolve using Pi's normal `~`, `@`, and
  `file://` handling, then must remain under the canonical project root (including symlink targets).
- `read`: reads through the project-scoped Rust file coordinator. Small files and explicit
  `offset`/`limit` ranges return exact source text; larger reads may return a location-preserving
  adaptive view. When a result is truncated, the model-facing text ends with a compact
  `<read_metadata>` JSON block containing `snapshot_id`, `next_cursor`, and omitted ranges; pass
  the `next_cursor` string back as `cursor` to continue. Full `segments` and queue metadata remain
  available in the structured tool details for UI/audit consumers.
- `write`: creates or fully replaces a file through the same coordinator. A supplied or
  previously observed revision prevents a stale agent from silently overwriting a newer change.
- `edit`: applies unique exact `oldText`/`newText` replacements. Unaffected queued changes can
  rebase; overlapping anchors return `file_conflict` with the preceding agent/session and diff
  summary.

File paths are restricted to the project root, including symlink targets. File lanes are scoped
to a canonical project root: different files run in parallel, same-file reads run together, and
queued writers run FIFO before later reads. `read`, `write`, and `edit` are Rust coordinator
overrides of Pi's built-ins; `bash` remains intentionally available and can bypass this policy.
Use the file tools for coordinated modifications.

## Concurrency and lifecycle

Child model execution never holds the topology lock. Spawns reserve capacity atomically,
then launch outside the lock. Each inbox is an O(1) queue, and waits use condition-variable
wakeup rather than polling. Tool definitions opt into Pi's parallel execution mode.

Mailboxes retain at most 256 messages per agent or offline root session, session transcripts retain 64 bounded entries,
messages are limited to 64 KiB, and Bash output capture is bounded to 256 KiB per process. The
`read_process` response returns the tail of that bounded buffer, reports `output_truncated`, and
labels the stream as `stdout_stderr_combined`; process summaries expose the same truncation bit.
Completed process history is retained for 15 minutes and is also capped at 128 entries. Bash is
non-interactive (`stdin` is closed), while command filtering and the configured timeout still
apply. The catalog retains at most 256 session snapshots. These bounds prevent a busy team from
growing control-plane memory without limit.

Starting, switching, forking, or reloading a level-0 session resets its runtime team and
interrupts the previous descendants. Stopping the root RPC process also stops the supervisor
and all children, even when stopping the root reports an error.

A parent agent finishing normally does not interrupt descendants that are still running. Explicit
`interrupt_agent`, a level-0 team reset, or application shutdown are the only lifecycle operations
that cascade termination through a subtree.

Subagents are deliberately ephemeral (`--no-session`). Their final outputs remain in the
level-0 tool result, while no child conversation is indexed or shown in sidebar history.
