# iroh-gate

Reach a machine's internal network from outside it.

You have a machine that can see things you cannot -- an internal site that
resolves only in its DNS, a database on another internal host, a socket that
exists only on its filesystem. iroh-gate makes those reachable from elsewhere,
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
iroh-gate daemon --service services.toml
# Ticket: e51a1db43748a73b44c12233239cbb36a763953c7901be97443cf075c02abce2
iroh-gate grant-token --label desktop
# 7fd25613dd5e17cb...   (one-time, valid 5 minutes)
```

The machine outside dials in with that token:

```sh
iroh-gate daemon -a e51a1db4... --enroll 7fd25613dd5e17cb...
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

Not published anywhere. Build it:

```sh
cargo build --release    # target/release/iroh-gate
```

## Usage

```
iroh-gate [--socket <path>] <command>
```

### Commands

```
daemon [options]           Start the daemon
ticket                     Print the daemon's ticket
grant-token --label <l>    Mint a one-time enrollment token (valid 5 min)
pin <key> --label <l>      Authorize a peer by key, no token (host-attested)
add-peer <ticket>          Connect to a peer
remove-peer <ticket>       Disconnect from a peer (and drop its pin)
expose <port> [--to <key>] Grant a port to peers (default: all known)
              [--upstream <host:port> | --unix <path> | --routes <file>]
                           ... and declare what serves it
unexpose <port> [--to <k>] Revoke grants for a port (or one peer's grant)
bind <port> --local <p>    Bind a peer's port to a different local port
list                       Show peers, grants, and bindings (JSON)
```

### Daemon options

| Option | Default | Description |
|--------|---------|-------------|
| `--host` | `127.0.0.1` | Where ports with no declared service forward to |
| `-a, --add` | | Add peer on startup (repeatable) |
| `-e, --expose` | | Expose port to the `-a` peers (repeat or comma-separate) |
| `--enroll` | | One-time token to present to the `-a` peers |
| `--service` | | Declare services on startup, from a TOML file (repeatable) |
| `--bind` | | Remap a peer's port as `REMOTE:LOCAL` (repeatable) |
| `--key` | `~/.local/state/iroh-gate/key` | Secret key path (created if missing) |
| `--socket` | `/tmp/iroh-gate.sock` | Unix socket path |

## Services

An exposed port forwards to `--host:<port>` on the serving machine. That is one
fixed address for every port, which is enough when the thing you want is
listening locally, and not enough when it lives behind that machine.

`expose` can say what serves the port instead:

```sh
iroh-gate expose 5432 --upstream db.internal:5432   # any host:port reachable there
iroh-gate expose 6000 --unix /var/run/docker.sock   # a socket that exists only there
iroh-gate expose 8080 --routes internal-sites.toml  # a reverse proxy
iroh-gate expose 3002                               # no backend: --host:3002
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
one-time token from `grant-token`. A valid claim pins the peer's key under the
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
iroh-gate bind 5432 --local 5433    # the peer's 5432 arrives on localhost:5433
iroh-gate bind 5432 --local 0       # or let the OS pick; list reports which
iroh-gate bind 5432 --clear         # back to binding the announced port
```

It takes effect immediately -- the existing listener is torn down and rebuilt,
with no need to wait for the peer to re-announce. `--bind 5432:5433` does the
same at startup.

## Inspecting

`list` reports what serves each port and where each binding actually landed:

```json
{
  "i_expose": [
    {"port": 3002, "backend": "default 127.0.0.1:3002"},
    {"port": 5432, "backend": "tcp db.internal:5432"},
    {"port": 6000, "backend": "unix /var/run/docker.sock"},
    {"port": 8080, "backend": "http (2 routes)"}
  ],
  "bindings": [
    {"port": 5432, "local": 5433, "peer": "e51a1db4..."}
  ]
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
