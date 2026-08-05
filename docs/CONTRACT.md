# CLI contract

What a program driving `ig` can rely on. Everything here is versioned with the
crate: additions are minor, removals and changed meanings are major.

Run `ig --dump-schema` for the machine-readable form of the command tree. It is
generated from the parser itself, so it cannot drift from what `ig` accepts.
Diffing it between releases is the cheapest way to catch a breaking change.

## Streams

- **stdout carries results only.** Values (`ig id`, `ig peer token`) and listings
  (`ig status`, `ig peer ls`, `ig port ls`).
- **stderr carries everything else.** Status summaries, warnings, errors, and
  the daemon's logs.
- A command whose result is an *action* rather than a value writes **nothing** to
  stdout in text mode. `ig port expose 8080 >/dev/null` produces no output on
  stdout to discard.
- `ig <cmd> 2>/dev/null` never removes a result; `ig <cmd> >/dev/null` never
  removes an error.
- Colour is emitted only when stderr is a terminal. Piping the daemon's logs to
  a file yields plain text.
- A reader that closes stdout early (`ig completion bash | head -1`) ends the
  process on `SIGPIPE`, as any other Unix tool would: exit `141`, nothing on
  stderr. It is not an application error, and does not use the table below.

## Exit codes

| Code | Meaning | Typical cause |
|------|---------|---------------|
| 0 | Success | Includes a successful `--dry-run` |
| 1 | Internal error | A bug, or an unclassified failure |
| 2 | Invalid arguments or request | Bad flag, malformed peer key, invalid route table |
| 3 | Not found | Peer unreachable, unknown ticket |
| 4 | Denied | Refused on authorization grounds |
| 5 | Conflict | Peer already added, two services claiming one port |
| 7 | Daemon unavailable | No daemon listening on `--socket` |

Codes 0-9 keep these meanings. Anything specific to a future subcommand will be
allocated at 10 or above.

The daemon classifies each failure once and the client maps it; callers never
need to match on the message text. Messages are for humans and may be reworded
in a patch release.

## Output formats

`--format text` (default) is for a person. `--format json` is the parsing
contract. `IG_FORMAT=json` sets it for a whole session.

| Command | `--format json` on stdout |
|---------|---------------------------|
| `id` | `{"id": "<64 hex>"}` |
| `peer token --label L` | `{"token": "<hex>"}` |
| `status` | `{"id", "peers", "exposed", "grants", "bindings"}` |
| `peer ls` | `{"peers": [...]}` |
| `port ls` | `{"exposed": [...], "grants": [...]}` |
| any action | `{"ok": true, "dry_run": <bool>, "detail": "<summary>"}` |
| any failure | `{"ok": false, "error": {"kind": "...", "message": "..."}}` |

Object shapes:

```jsonc
// peers[]
{"key": "<64 hex>", "label": "vm" | null, "online": true, "they_expose": [3001]}

// exposed[]  -- backend is a human summary, not a parsing target;
//               use the exit code and `ok` for control flow
{"port": 5432, "backend": "tcp db.internal:5432"}

// grants[]   -- one row per (port, grantee)
{"port": 5432, "to": "<64 hex>"}

// bindings[] -- `local` differs from `port` when remapped by `port bind`
{"port": 5432, "local": 5433, "peer": "<64 hex>"}
```

`error.kind` is one of `invalid`, `not_found`, `conflict`, `denied`,
`unavailable`, `internal`, and maps onto the exit code table above.

## Non-interactive execution

No command prompts or reads stdin, in any mode. Everything an operation needs is
expressible as a flag, so a single call always completes it. `--no-input` is
accepted so a script can assert this, and is reserved against any future prompt.

`--dry-run` is available on every mutating command: `peer add`, `peer rm`,
`peer pin`, `port expose`, `port unexpose`, `port bind`. It validates
everything the real call would -- unparseable keys, invalid route tables,
missing grantees -- reports what would change, and changes nothing. A dry run
that would have succeeded exits 0 and sets `"dry_run": true` in JSON output.

Mutating commands are idempotent where the operation allows: re-exposing a port
with a new backend replaces it rather than failing, and `port bind` to the same
local port is a no-op.

## Configuration precedence

CLI flag > environment variable > built-in default.

| Variable | Equivalent flag | Default |
|----------|-----------------|---------|
| `IG_SOCKET` | `--socket` | `/tmp/ig.sock` |
| `IG_FORMAT` | `--format` | `text` |
| `IG_QUIET` | `--quiet` | off |
| `RUST_LOG` | | `ig=info` (`ig=warn` under `--quiet`) |

The daemon's secret key lives at `$XDG_STATE_HOME/ig/key`, falling back to
`~/.local/state/ig/key`. Override with `--key`. Pins are stored beside it as
`<key>.peers.json`.

## Secrets

Enrollment tokens are the only secret this CLI handles. Pass them by file:

```sh
ig peer token --label vm > /run/token        # minted on the operator
ig daemon -a <ticket> --enroll-file /run/token
```

`--enroll <TOKEN>` still works but puts the token in argv, where it is visible
in the process table and lands in shell history. Prefer `--enroll-file`.

Peer keys are public and safe on the command line.

## Stability

- Flags, subcommand names, exit codes, and the JSON shapes above are the
  contract.
- `detail` strings, `backend` strings, and error messages are human-facing and
  may change in any release. Do not parse them.
- Deprecations keep working for one minor version with a warning on stderr, then
  are removed in the next major.
