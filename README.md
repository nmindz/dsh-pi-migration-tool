# dsh-pi-migration-tool

Converts coding-agent sessions between **DeepSeek Harness** (DSH) and **Oh My Pi** (OMP / pi), in both directions, and imports **Claude Code** sessions into Pi, with a Ratatui browser for picking what to move.

```
dshpi                        # browse and convert interactively
dshpi convert-all dsh        # migrate every DSH session into ~/.omp/agent/sessions
dshpi convert-all claude     # import every Claude Code session Pi does not already hold
dshpi verify                 # prove the conversions lose nothing
```

## Why it exists

The harnesses persist sessions differently and none reads the others:

| | DSH | Pi | Claude Code |
|---|---|---|---|
| Location | `~/.dsh/sessions/<bucket>/<id>/session.v<N>.jsonl.zstd` | `~/.omp/agent/sessions/<bucket>/<ts>_<id>.jsonl` | `~/.claude/projects/<bucket>/<id>.jsonl` |
| Framing | zstd-compressed JSONL, one file per released generation | plain JSONL, 256-byte fixed-width title slot first | plain JSONL, one record per content block |
| Model | flat event log (`seq`, `time`, `type`, `data`) | append-only entry tree (`id`, `parentId`) | record tree (`uuid`, `parentUuid`) |
| Project bucket | `--Users-me-Projects-x--` | `-Projects-x` under home, `--opt-work-x--` elsewhere | `-Users-me-Projects-x`, every non-alphanumeric character as `-` |

Claude Code is a one-way source: its sessions import into Pi, and nothing converts into Claude Code.

## Install

Needs a Rust toolchain. Builds into the shared cache at `~/.rust/target` (see `.cargo/config.toml`).

```sh
cargo build --release
install -m755 ~/.rust/target/release/dshpi ~/.local/bin/dshpi
```

Session roots are discovered from `~/.dsh`, `~/.omp`, and `~/.claude`, overridable with `DSH_HOME`, `OMP_HOME`, `CLAUDE_CONFIG_DIR`, or the direct `DSH_SESSIONS_DIR`, `OMP_SESSIONS_DIR`, `CLAUDE_SESSIONS_DIR`.

## Browser

`dshpi` with no subcommand opens the browser.

| Key | Action |
|---|---|
| `tab`, `1`, `2`, `3` | switch between the DSH, Pi, and Claude Code session lists |
| `j` `k` `↑` `↓`, `g` `G`, `PgUp` `PgDn` | move |
| `space` | toggle selection; on a Claude Code project, every shown session in it |
| `a` / `A` | select everything shown / clear the selection |
| `/` | filter by id, title, or cwd |
| `enter` | preview the transcript (`j`/`k` scroll, `space` pages, `esc` closes); on a project, fold it |
| `←` `→` / `h` `l` | fold / unfold a Claude Code project |
| `x` | convert the selection, or the row under the cursor; on Claude Code, import the checked sessions after a confirmation |
| `m` | write `./exports/<id>.md` |
| `d` | toggle dry-run |
| `f` | allow overwriting an existing destination |
| `e` | show sessions that hold no conversation (hidden by default) |
| `r` | rescan every root |
| `?` / `q` | keys / quit |

The detail pane shows where a session would land and warns when that file already exists.

### Claude Code tab

Sessions are grouped under their working directory. Projects and sessions each carry a checkbox: `[x]` checked, `[ ]` unchecked, and `[-]` on a project whose sessions are partly checked. A session shows its name — a `/rename`, an agent name, or Claude Code's generated title, in that order — and, when it has none, its first typed prompt in italics, or its id.

Each session is judged against what Pi already holds (see [Duplicates](#duplicates)):

| Mark | Meaning | Starts |
|---|---|---|
| `=` | Pi already holds it | unchecked |
| `≈` | some of its prompts are in one Pi session | checked |
| none | nothing in Pi resembles it | checked |

Uncheck the projects or sessions you already moved some other way, then `x` asks for confirmation and imports what remains checked. Imported sessions turn `=` and drop out of the selection.

## Commands

```sh
dshpi list [dsh|pi|claude] [--filter TEXT]

dshpi convert <id-or-path> [--to dsh|pi] [--out DIR] [--dry-run] [--force] [--include-empty] [--include-duplicates]
dshpi convert-all <dsh|pi|claude> [--filter TEXT] [--out DIR] [--dry-run] [--force] [--include-empty] [--include-duplicates]

dshpi prune-empty [--filter TEXT] [--execute]
dshpi verify [dsh|pi|claude] [--filter TEXT] [--all]
dshpi export <id-or-path> [--format md|text] [-o FILE]
```

`<id-or-path>` accepts a session-id prefix or a path to a session file; the direction is inferred from the source. Without `--out`, conversions write into the destination harness's real session root, so the result is immediately listed and resumable there. Claude Code sessions always land in Pi, and `--include-duplicates` imports one even when Pi already holds a copy.

### Empty sessions

A cancelled or never-used launch leaves behind a log holding nothing but startup records — permission preset, sandbox mode, approval policy — and no prompt or reply. Importing those buries the real sessions behind rows of `(no messages)`.

Conversion therefore **skips any session with no conversation message** unless `--include-empty` is passed, and the browser hides them until `e` reveals them. `dshpi prune-empty` cleans up a corpus imported before that rule existed: it reports the empty sessions this tool wrote and deletes them under `--execute`. Native sessions of the destination harness are never candidates, empty or not.

A Claude Code session counts as empty when no model ever replied — it ran only slash commands such as `/model` or `/login`, or its prompts went unanswered — and the same rule applies to each of its subagents. Assistant records Claude Code writes itself, such as API errors, are not replies.

## Mapping

| DSH event | Pi entry |
|---|---|
| `session` header | `session` header + title slot |
| `session/title` | `title_change`, and the title slot |
| `model/selection` | `model_change` |
| `system/message` (first) | `session_init` |
| `developer/message` | `message` role `developer` |
| `user/message` source `user` | `message` role `user` |
| `user/message` other sources | `custom_message` |
| `assistant/message` | `message` role `assistant` (`reasoning`→`thinking`, `tool-call`→`toolCall`) |
| `tool/call` | `custom` `tool_execution_start` |
| `tool/result` | `message` role `toolResult`, `meta`→`details` |
| `compaction/summary` | `compaction` |
| everything else | `custom` `ai.deepseek.dsh.<type>` holding the row verbatim |

DSH generations v2, v3, and v4 all read; the newest generation present in a session directory is the authoritative one. Pre-v4 tool results nest their output inside a `tool-result` block and v4 stores it flat — both shapes convert and restore correctly.

Pi-only entries (`thinking_level_change`, `label`, `reset_boundary`, extension records) become DSH events typed `pi/<entry>` carrying the envelope's `ignorable: true` marker, which DSH retains without refusing the log.

## Claude Code import

| Claude Code record | Pi entry |
|---|---|
| `user` prompt (text, images) | `message` role `user` |
| `user` `tool_result` block | `message` role `toolResult` |
| `user` with `isMeta` (hook feedback, skill bodies, command caveats) | `custom_message` `claude-code.meta`, hidden but in context |
| `user` with `isCompactSummary` | `compaction`, sized from the `compact_boundary` before it |
| `assistant` records of one response | one `message` role `assistant` (`thinking` keeps its signature, `tool_use`→`toolCall`), after a `model_change` when the model changes |
| `queued_command` attachment | `message` role `user` — a prompt typed while the model was busy has no other copy |
| `custom-title`, `agent-name`, `ai-title`, `summary` | title slot with its source, and one `title_change` |
| subagent `<id>/subagents/**/agent-<x>.jsonl` | Pi subagent file beside the session, titled by its `meta.json` description |

Hook output, context attachments, file-history snapshots, permission and mode switches, and cost state are Claude Code runtime bookkeeping and are left out.

Claude Code writes each content block of a response as its own record and starts tools before the response ends, so blocks arrive interleaved with tool results and injected prompts. The import merges a response into one assistant message, puts its tool results directly after it, and holds injected and queued prompts until the results are in, which is the order a provider accepts on resume. Entries chain in file order; only a turn that starts from an earlier point — a rewind, or a prompt typed while a tool ran — opens a Pi branch there, and Pi's active path matches the one Claude Code continued on.

Each import gets a fresh, deterministic session id, so it never collides with its source and re-importing lands on the same file. A `foreign_session_import` record names the source session, the same record OMP's own `--from-claude` writes.

### Duplicates

A Claude Code session can already be in Pi under another id: imported before, imported by `omp --from-claude`, or migrated through DSH. Two signals survive those trips:

- a `foreign_session_import` record naming the Claude Code session marks it **already imported**;
- otherwise the human prompts are fingerprinted — whitespace-normalized, at least 16 characters, command envelopes excluded — and a Pi session holding **every** fingerprinted prompt marks it a duplicate. Holding some marks it `≈` with the share, and the decision stays yours.

`convert-all claude` and `convert claude` skip duplicates unless `--include-duplicates` is passed; `list claude` shows each verdict. A copy that rewrote the prompts themselves is not detected — the browser's project checkboxes are the control for that.

## Fidelity

Every converted entry carries a `dsh` field holding the source `seq`, event type, surface intent, and the source payload **minus exactly the fields the Pi entry already materializes** — message content, tool-result metadata, compaction summary, title. Nothing is stored twice, and converting back rebuilds the original rows instead of an approximation.

`dshpi verify` proves this against your real sessions:

- **DSH → Pi → DSH** must reproduce the header and every event row byte for byte.
- **Pi → DSH → Pi** must reproduce the header, the title, and every transcript block. Entry counts legitimately grow here, because DSH replay needs the turn and step scaffolding Pi has no entries for; that scaffolding is dropped again on the way back.
- **Claude Code → Pi**, being one-way, must carry every prompt, reply, reasoning block, tool call, tool result, injected context message, and compaction of the source and of each subagent. Each kind is compared in its own order, since a response's tool results legitimately move up next to it.

```
$ dshpi verify dsh
553 checked · 553 identical · 0 changed · 0 unreadable

$ dshpi verify claude
68 checked · 68 identical · 0 changed · 0 unreadable
```

The 68 Claude Code sessions measured here import to 567 Pi files (subagents included) in about 4 s, and every file loads in OMP's own session manager with each tool call answered on the active path.

### Known limits

- Pi files are uncompressed, so a migrated corpus is roughly 4–5× the size of its zstd originals.
- Conversion does not copy attachments. DSH keeps binary attachments in `~/.dsh/attachments`; references survive in the log, the blobs are not moved.
- DSH `assistant/attempt` streaming telemetry has no Pi counterpart and rides along as an `ai.deepseek.dsh.assistant/attempt` record, invisible in the Pi transcript.
- Sub-sessions are independent sessions on every side; each converts on its own and the parent link is carried through the header.
- Claude Code keeps transcripts for `cleanupPeriodDays` (30 by default). A session migrated before its transcript was cleaned up has nothing left to compare against; only its project checkbox can keep it out.
- Claude Code's file-history checkpoints, the snapshots behind its rewind of file edits, do not import; the conversation does.
- Pi entry ids are minted deterministically from the source session and sequence, so re-running a conversion is byte-identical — but a native Pi session converted to DSH and back gets new ids for its mapped messages.

## Layout

```
src/paths.rs          session roots and the cwd-bucket encodings
src/dsh.rs            DSH discovery, zstd JSONL decode/encode
src/pi.rs             Pi discovery, title slot, JSONL decode/encode
src/claude.rs         Claude Code discovery and JSONL decoding
src/convert.rs        both DSH ⇄ Pi directions
src/claude_import.rs  Claude Code → Pi import
src/dedup.rs          duplicate detection for Claude Code imports
src/transcript.rs     readable projection shared by the preview and Markdown export
src/tui.rs            Ratatui browser
src/main.rs           CLI
```

```sh
cargo test   # format, mapping, and round-trip unit tests
```
