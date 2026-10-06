# dsh-pi-migration-tool

Converts coding-agent sessions between **DeepSeek Harness** (DSH) and **Oh My Pi** (OMP / pi), in both directions, with a Ratatui browser for picking what to move.

```
dshpi                     # browse and convert interactively
dshpi convert-all dsh     # migrate every DSH session into ~/.omp/agent/sessions
dshpi verify dsh          # prove the conversion loses nothing
```

## Why it exists

The two harnesses persist sessions differently and neither reads the other:

| | DSH | Pi |
|---|---|---|
| Location | `~/.dsh/sessions/<bucket>/<id>/session.v<N>.jsonl.zstd` | `~/.omp/agent/sessions/<bucket>/<ts>_<id>.jsonl` |
| Framing | zstd-compressed JSONL, one file per released generation | plain JSONL, 256-byte fixed-width title slot first |
| Model | flat event log (`seq`, `time`, `type`, `data`) | append-only entry tree (`id`, `parentId`) |
| Project bucket | `--Users-me-Projects-x--` | `-Projects-x` under home, `--opt-work-x--` elsewhere |

## Install

Needs a Rust toolchain. Builds into the shared cache at `~/.rust/target` (see `.cargo/config.toml`).

```sh
cargo build --release
install -m755 ~/.rust/target/release/dshpi ~/.local/bin/dshpi
```

Session roots are discovered from `~/.dsh` and `~/.omp`, overridable with `DSH_HOME`, `OMP_HOME`, `DSH_SESSIONS_DIR`, or `OMP_SESSIONS_DIR`.

## Browser

`dshpi` with no subcommand opens the browser.

| Key | Action |
|---|---|
| `tab`, `1`, `2` | switch between the DSH and Pi session lists |
| `j` `k` `↑` `↓`, `g` `G`, `PgUp` `PgDn` | move |
| `space` | toggle selection |
| `a` / `A` | select everything shown / clear the selection |
| `/` | filter by id, title, or cwd |
| `enter` | preview the transcript (`j`/`k` scroll, `space` pages, `esc` closes) |
| `x` | convert the selection, or the row under the cursor |
| `m` | write `./exports/<id>.md` |
| `d` | toggle dry-run |
| `f` | allow overwriting an existing destination |
| `r` | rescan both roots |
| `?` / `q` | keys / quit |

The detail pane shows where a session would land and warns when that file already exists.

## Commands

```sh
dshpi list [dsh|pi] [--filter TEXT]

dshpi convert <id-or-path> [--to dsh|pi] [--out DIR] [--dry-run] [--force]
dshpi convert-all <dsh|pi> [--filter TEXT] [--out DIR] [--dry-run] [--force]

dshpi verify [dsh|pi] [--filter TEXT] [--all]
dshpi export <id-or-path> [--format md|text] [-o FILE]
```

`<id-or-path>` accepts a session-id prefix or a path to a session file; the direction is inferred from the source. Without `--out`, conversions write into the other harness's real session root, so the result is immediately listed and resumable there.

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

## Fidelity

Every converted entry carries a `dsh` field holding the source `seq`, event type, surface intent, and the source payload **minus exactly the fields the Pi entry already materializes** — message content, tool-result metadata, compaction summary, title. Nothing is stored twice, and converting back rebuilds the original rows instead of an approximation.

`dshpi verify` proves this against your real sessions:

- **DSH → Pi → DSH** must reproduce the header and every event row byte for byte.
- **Pi → DSH → Pi** must reproduce the header, the title, and every transcript block. Entry counts legitimately grow here, because DSH replay needs the turn and step scaffolding Pi has no entries for; that scaffolding is dropped again on the way back.

Measured on 551 DSH sessions and 4 Pi sessions: all identical, ~12 s.

```
$ dshpi verify dsh
551 checked · 551 identical · 0 changed · 0 unreadable
```

### Known limits

- Pi files are uncompressed, so a migrated corpus is roughly 4–5× the size of its zstd originals.
- Conversion does not copy attachments. DSH keeps binary attachments in `~/.dsh/attachments`; references survive in the log, the blobs are not moved.
- DSH `assistant/attempt` streaming telemetry has no Pi counterpart and rides along as an `ai.deepseek.dsh.assistant/attempt` record, invisible in the Pi transcript.
- Pi entry ids are minted deterministically from the source session and sequence, so re-running a conversion is byte-identical — but a native Pi session converted to DSH and back gets new ids for its mapped messages.
- Sub-sessions are independent sessions on both sides; each converts on its own and the parent link is carried through the header.

## Layout

```
src/paths.rs       session roots and the cwd-bucket encodings
src/dsh.rs         DSH discovery, zstd JSONL decode/encode
src/pi.rs          Pi discovery, title slot, JSONL decode/encode
src/convert.rs     both conversion directions
src/transcript.rs  readable projection shared by the preview and Markdown export
src/tui.rs         Ratatui browser
src/main.rs        CLI
```

```sh
cargo test   # format, mapping, and round-trip unit tests
```
