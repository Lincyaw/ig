# ig

Reach a machine's internal network from outside it.

You have a machine that can see things you cannot -- an internal site that
resolves only in its DNS, a database on another internal host, a socket that
exists only on its filesystem. ig makes those reachable from elsewhere,
as ordinary ports on `localhost`.

Neither machine needs an open inbound port, a public IP, or a relay you run.
Both sides dial out; [iroh](https://github.com/n0-computer/iroh) handles
discovery, NAT traversal, and relay fallback. That is the part that makes this
work when the machine you care about is a laptop behind NAT.

Every connection to an internal resource is made by the machine that can reach
it, so names resolve with its resolver and the upstream sees its address rather
than yours.

## Example

The laptop is inside the network. It runs the daemon, and declares three things
worth reaching:

```toml
# services.toml
[[service]]
kind = "http"
port = 8080

  [[service.route]]
  prefix = "/gitlab"
  upstream = "gitlab.internal:80"
  strip_prefix = true

  [[service.route]]
  prefix = "/wiki"
  upstream = "wiki.internal:80"
  strip_prefix = true

[[service]]
kind = "tcp"
port = 5432
upstream = "db.internal:5432"

[[service]]
kind = "unix"
port = 6000
path = "/var/run/docker.sock"
```

```sh
ig daemon --service services.toml
# Ticket: e51a1db43748a73b44c12233239cbb36a763953c7901be97443cf075c02abce2
```

The machine outside dials in with a one-time token:

```sh
ig peer token --label desktop > /run/token   # on the laptop
ig daemon -a e51a1db4... --enroll-file /run/token
```

The three ports show up on its `localhost`:

```sh
curl http://localhost:8080/gitlab/    # the internal site
psql -h 127.0.0.1 -p 5432             # the internal database
curl http://localhost:6000/version    # the laptop's docker socket
```

Nobody else can reach any of it. Close the laptop, open it again, and the
connection restores on its own.

## Install

Tagging `v*` builds Linux x86_64 and macOS (both architectures) and attaches
them to a GitHub release, with a `.sha256` beside each. The repository is
private, so downloading one needs an authenticated `gh`:

```sh
gh release download --repo Lincyaw/ig --pattern '*aarch64-apple-darwin*'
```

There is no Windows build: the control socket is AF_UNIX, unix sockets are a
backend kind, and SIGPIPE is handled through libc.

Otherwise build it:

```sh
cargo build --release    # target/release/ig
```

## Usage

```
ig [OPTIONS] <COMMAND>
```

### Commands

Noun first, verb second, so the tree is walkable: `ig peer --help` lists what
you can do to a peer, `ig port --help` what you can do to a port.

```
ig daemon [options]              Start the daemon
ig id                            Print this daemon's endpoint id
ig status                        Peers, exposed ports, and local bindings

ig peer add <ticket>             Connect to a peer
ig peer rm <ticket>              Disconnect, and drop its pin
ig peer ls                       Known peers and what they expose to us
ig peer pin <key> --label <l>    Authorize by key, no token
ig peer token --label <l>        Mint a one-time enrollment token (5 min)

ig port expose <port> [--to <key>]           Grant a port
       [--upstream <host:port> | --unix <path> | --routes <file>]
                                             ... and declare what serves it
ig port unexpose <port> [--to <key>]         Revoke grants
ig port ls                                   Exposed ports and what serves each
ig port bind <port> --local <p>              Land a peer's port elsewhere

ig completion <shell>            Print a shell completion script
```

Global flags, valid on every command:

| Flag | Env | Meaning |
|------|-----|---------|
| `--socket <path>` | `IG_SOCKET` | The daemon's control socket |
| `--format text\|json` | `IG_FORMAT` | Result format on stdout |
| `--quiet` | `IG_QUIET` | Drop status chatter from stderr |
| `--no-input` | | Assert that nothing will prompt (already true) |
| `--dry-run` | | On every mutating command: report, change nothing |

`--dump-schema` prints the whole command tree as JSON, generated from the
parser, for anything that needs to construct invocations without guessing.

### Daemon options

| Option | Default | Description |
|--------|---------|-------------|
| `--host` | `127.0.0.1` | Where ports with no declared service forward to |
| `-a, --add` | | Add peer on startup (repeatable) |
| `-e, --expose` | | Expose port to the `-a` peers (repeat or comma-separate) |
| `--enroll-file` | | Read the one-time token from a file (preferred) |
| `--enroll` | | The token inline; leaks through argv, prefer `--enroll-file` |
| `--service` | | Declare services on startup, from a TOML file (repeatable) |
| `--bind` | | Remap a peer's port as `REMOTE:LOCAL` (repeatable) |
| `--key` | `~/.local/state/ig/key` | Secret key path (created if missing) |

## Services

An exposed port forwards to `--host:<port>` on the serving machine. That is one
fixed address for every port, which is enough when the thing you want is
listening locally, and not enough when it lives behind that machine.

`expose` can say what serves the port instead:

```sh
ig port expose 5432 --upstream db.internal:5432   # any host:port reachable there
ig port expose 6000 --unix /var/run/docker.sock   # a socket that exists only there
ig port expose 8080 --routes internal-sites.toml  # a reverse proxy
ig port expose 3002                               # no backend: --host:3002
```

Declared live, like every other grant -- no restart. `--service <file>` makes the
same declarations at startup, in the `[[service]]` form shown in the example
above.

### Route tables

The `--routes` file is a list of routes, tried in order, first match wins:

```toml
[[route]]
prefix = "/gitlab"
upstream = "gitlab.internal:80"     # resolved by the serving machine
strip_prefix = true

[[route]]
host = "wiki.local"                 # matched against the request's Host header
upstream = "wiki.internal:443"
tls = true                          # terminate TLS here, so the peer need not

[[route]]
prefix = "/app"
unix = "/run/app.sock"              # an HTTP server on a Unix socket
```

| Key | Meaning |
|-----|---------|
| `host` | Match the request's Host header, hostname only. Optional. |
| `prefix` | Match a path prefix. Optional. |
| `upstream` | Where to send it, `host:port`. One of this or `unix` is required. |
| `unix` | Send it to an HTTP server on a Unix socket instead. |
| `tls` | Speak TLS to the upstream. |
| `sni` | SNI to present. Defaults to the upstream hostname. |
| `strip_prefix` | Remove `prefix` from the path before forwarding. |
| `host_header` | Host header to send. Defaults to the upstream hostname. |

`host_header` defaults the way it does because the client dialed
`127.0.0.1:8080` through a tunnel, so its own Host header names the tunnel, not
the site. Upstreams that vhost on Host need the real name.

## Access

Default deny, per peer. Each daemon has a stable identity -- an iroh endpoint id
backed by a keypair persisted at `--key`. Because it is stable, you can bake one
ticket into every workload you boot.

A port is exposed by a grant, `(port) -> peer key`, and served only to the peers
named in one. iroh gives the connecting peer's key cryptographically, so a grant
names a proven identity, not a shareable address: you cannot hand out reach by
leaking a string ([ADR 0001](docs/adr/0001-directed-grants.md)).

An incoming connection from an unknown key is refused unless it presents a
one-time token from `ig peer token`. A valid claim pins the peer's key under the
token's label and is then spent; pins persist across restarts, so a reboot does
not orphan enrolled workloads ([ADR 0002](docs/adr/0002-token-enrollment.md)).
`pin` authorizes a key directly when nothing secret should travel into the
workload ([ADR 0003](docs/adr/0003-host-attested-enrollment.md)).

This applies to every kind of port equally. A service port is granted like any
other, and the grant check runs ahead of the dispatch, so it is the single gate.
A port whose last grantee is revoked retires its service with it, however the
revoke was spelled.

Note that ports declared in a `--service` file are granted to the `-a` peers at
startup, the same as `-e` ports. The runtime `expose` requires the grant to be
named; the startup file treats the `-a` list as the grant.

## Choosing the local port

A peer picks the numbers it announces, and one may already be taken on your
machine:

```sh
ig port bind 5432 --local 5433    # the peer's 5432 arrives on localhost:5433
ig port bind 5432 --local 0       # or let the OS pick; status reports which
ig port bind 5432 --clear         # back to binding the announced port
```

It takes effect immediately -- the existing listener is torn down and rebuilt,
with no need to wait for the peer to re-announce. `--bind 5432:5433` does the
same at startup.

## Inspecting

`ig status` shows the whole picture; `ig peer ls` and `ig port ls` show one
part each.

```
$ ig status
id  e51a1db43748a73b44c12233239cbb36a763953c7901be97443cf075c02abce2

peers
  2ecee2029149  desktop     online   exposes -

exposed
  3002    default 127.0.0.1:3002          to 2ecee2029149
  5432    tcp db.internal:5432            to 2ecee2029149
  6000    unix /var/run/docker.sock       to 2ecee2029149
  8080    http (2 routes)                 to 2ecee2029149

bindings
  (none)
```

Add `--format json` to any of them for the parsing contract:

```json
{
  "exposed": [{"port": 5432, "backend": "tcp db.internal:5432"}],
  "grants":  [{"port": 5432, "to": "2ecee2029149..."}]
}
```

## How it works

**Forwarding.** Each peer is announced only the ports granted to it. When a peer
grants you a port, a local TCP listener binds it on `127.0.0.1` and traffic runs
over the encrypted QUIC connection. It works both ways.

**Reconnection.** If the connection drops, both sides reconnect with exponential
backoff. Bindings stay in place and resume when the link comes back.

**The http kind** uses [pingora](https://github.com/cloudflare/pingora) as a
library. Its transport abstraction is `Box<dyn IO>`, blanket-implemented for
anything meeting its supertraits, so an iroh QUIC stream only has to implement
those to be served as if it were a socket. No pingora type appears outside
`src/service.rs`.

**Peers are distinguishable in http logs.** A tunnelled stream has no address,
and giving every peer `127.0.0.1` would merge them into one client, so each
peer's key hashes to a stable synthetic address in `100.64.0.0/10`.

**Upstreams resolve per request, cached for 5 seconds.** Not to save the lookup:
pingora keys its upstream connection pool on the resolved address, so
re-resolving a name whose records rotate would hand it a different key each time
and defeat keep-alive.

## Driving it from a program

stdout carries results, stderr carries everything else, and the exit code says
what happened -- `2` invalid, `3` not found, `5` conflict, `7` no daemon, and so
on. Nothing needs to grep an error message to decide whether to retry.

```sh
ig status --format json | jq -r '.exposed[].port'
ig port expose 8080 --routes sites.toml --dry-run --format json
```

[docs/CONTRACT.md](docs/CONTRACT.md) is the full reference: streams, exit codes,
every JSON shape, environment variables, and what is stable versus what is
human-facing prose.

## Limitations

- Downstream is HTTP/1.1 only. A QUIC stream cannot un-read bytes, so the h2c
  preface sniff is skipped. Browsers through the tunnel are unaffected.
- WebSocket upgrades through the http kind are untested.
- The receiving side always lands on a local TCP port. A Unix socket forwarded
  from the other machine arrives as `127.0.0.1:<port>`, so tools that insist on
  a socket path need an equivalent (`DOCKER_HOST=tcp://...`).
- `bind` remaps are global across peers, not per peer.

## Testing

`cargo test` covers the units. `tests/e2e.sh` stands up two daemons and every
backend kind end to end; see [tests/README.md](tests/README.md).

## Credit

Forked from [cablehead/pai-sho](https://github.com/cablehead/pai-sho) by Andy
Gayton, MIT licensed. The peer mesh -- stable identity, directed grants, token
enrollment, auto-binding, reconnection -- and the ADRs under `docs/adr/` are his
work. Maintained separately here; it does not track upstream.
