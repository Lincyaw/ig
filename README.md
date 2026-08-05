# iroh-gate

Reach a machine's internal network from outside it.

iroh-gate forwards specific ports between your machines over an encrypted,
peer-to-peer QUIC connection (built on [iroh](https://github.com/n0-computer/iroh)).
Neither machine needs an open inbound port, a public IP, or a relay you run --
iroh handles discovery, NAT traversal, and relay fallback.

What it is for: a laptop that can reach an internal network, and a machine
elsewhere that cannot. Declare what each exposed port is backed by -- a reverse
proxy onto internal sites, a forward to any `host:port` the laptop can reach, or
a Unix socket that exists only there -- and it becomes an ordinary local port on
the other machine. The connection is always made by the serving machine, so
names resolve with its resolver and the upstream sees its address.

Access is default deny and per peer. Each machine runs one long-lived daemon
with a stable identity (a keypair). You grant a specific port to a specific
peer's key; that peer, and no one else, can reach it. A machine you have not met
enrolls with a one-time token, so you can boot a fleet of untrusted workloads
that phone home and each get exactly the access you granted, with no manual key
exchange, and with siblings invisible to each other.

## Fork

iroh-gate is a fork of [cablehead/pai-sho](https://github.com/cablehead/pai-sho)
by Andy Gayton, MIT licensed. The peer mesh -- identity, directed grants, token
enrollment, auto-binding, reconnection -- is his work, and the ADRs under
`docs/adr/` are his. This fork adds what an exposed port is backed by
(reverse proxy / tcp / unix socket) and local port remapping, and is maintained
separately; it does not track upstream.

The two are still wire compatible: the peer-to-peer protocol is unchanged, and
everything added here is decided on one side of the link.

## Example

On my laptop the daemon is already running. Its ticket is stable, so I look it up
once, and I mint a one-time token for the VM I'm about to boot:

```sh
iroh-gate ticket
# 5hc4bjqfp6booceusm3jrfebbegyfi6aiqwbgx4xxqmpvg5usoyq
iroh-gate grant-token --label vm
# 7fd25613dd5e17cb...   (one-time, valid 5 minutes)
```

The VM runs an [http-nu](https://github.com/cablehead/http-nu) app on `:3001` and
[stellar](https://github.com/cablehead/stellar) on `:7331` for live CSS editing.
I start its daemon pointing home, exposing both ports to my laptop:

```sh
iroh-gate daemon -a 5hc4bjqfp6booceusm3jrfebbegyfi6aiqwbgx4xxqmpvg5usoyq \
    -e 3001,7331 --enroll 7fd25613dd5e17cb...
```

The VM enrolls under the label `vm`, and `localhost:3001` and `localhost:7331` on
my laptop reach it -- and only my laptop; anyone else who dials the VM is refused.
Close the laptop, reopen it, and the connection restores on its own, no new token
needed.

Spin up something new on the VM and expose it live:

```sh
http-nu :3002 -c '{|req| "hello from a new experiment"}'
iroh-gate expose 3002
```

It's immediately at `http://localhost:3002` in my browser. Done with it?
`iroh-gate unexpose 3002`.

## Install

Not published. Build it:

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
pin <key> --label <l>      Enroll a peer by key, no token (host-attested)
add-peer <ticket>          Connect to a peer
remove-peer <ticket>       Disconnect from a peer (and drop its pin)
expose <port> [--to <key>] Grant a local port to peers (default: all known)
              [--upstream <host:port> | --unix <path> | --routes <file>]
                           ... and declare what serves it (see Services)
unexpose <port> [--to <k>] Revoke grants for a port (or one peer's grant)
bind <port> --local <p>    Bind a peer's port to a different local port
list                       Show peers, grants, and bindings (JSON)
```

### Daemon Options

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

### Services

An exposed port forwards to `--host:<port>` on the serving machine. That is one
fixed address for every port, which is enough when the thing you want is
listening locally, and not enough when it lives behind the serving machine: an
internal site that resolves only there, a database on another internal host, a
daemon that only speaks over a Unix socket.

`expose` can say what serves the port instead:

```sh
iroh-gate expose 5432 --upstream db.internal:5432   # any host:port reachable here
iroh-gate expose 6000 --unix /var/run/docker.sock   # a local Unix socket
iroh-gate expose 8080 --routes internal-sites.toml  # a reverse proxy
iroh-gate expose 3002                               # no backend: --host:3002
```

Declared live, like every other grant -- no restart. The route table for
`--routes` is a list of routes, tried in order:

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

`host` and `prefix` select (both optional, first match wins), one of `upstream`
or `unix` is required, and `tls`, `sni`, `strip_prefix` and `host_header`
control what is sent upstream. `host_header` defaults to the upstream hostname,
which is what vhosted sites need -- the client's Host header names the tunnel,
not the site.

To declare services at startup instead, `--service` takes a file of the same
declarations:

```toml
[[service]]
kind = "http"
port = 8080
  [[service.route]]
  prefix = "/gitlab"
  upstream = "gitlab.internal:80"
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

In every kind the connection is made by the serving machine: names resolve with
its resolver, and the upstream sees its address rather than the peer's.

Access is unchanged. A service port is granted like any other, so grants,
enrollment, auto-binding and reconnection all apply -- a peer with no grant for
the port gets nothing. A port whose last grantee is revoked retires its service
with it, however the revoke was spelled.

Note that ports declared in a `--service` file are granted to the `-a` peers at
startup, the same as `-e` ports. The runtime `expose` requires the grant to be
named; the startup file treats the `-a` list as the grant.
On the peer's side it is an ordinary local port: `http://localhost:8080/gitlab/`
for the site, `psql -h 127.0.0.1 -p 5432` for the database.

`list` reports what each port is served by:

```
15500 -> tcp localhost:9201
16000 -> unix /var/run/docker.sock
18080 -> http (3 routes)
19000 -> default 127.0.0.1:19000
```

Peers are distinguished in HTTP logs by a synthetic address derived from their
key, in the 100.64.0.0/10 range. It is stable per peer and cannot collide with a
real internal network.

### Choosing the local port

A peer picks the port numbers it announces, and one of them may already be taken
on your machine. `bind` moves it:

```sh
iroh-gate bind 5432 --local 5433    # the peer's 5432 arrives on localhost:5433
iroh-gate bind 5432 --local 0       # or let the OS pick; `list` reports which
iroh-gate bind 5432 --clear         # back to binding the announced port
```

It takes effect immediately -- the existing listener is torn down and rebuilt,
with no need to wait for the peer to re-announce. `--bind 5432:5433` does the
same at startup, and `list` shows both numbers:

```json
"bindings": [{"port": 5432, "local": 5433, "peer": "..."}]
```


## How it works

**Identity.** Each daemon has a stable ticket -- an iroh endpoint ID backed by a
keypair persisted at `--key`. Because it is stable, a launcher can bake one operator
ticket into every workload it boots.

**Grants.** Access is default deny. A port is exposed by a grant -- `(port) -> peer
key` -- and served only to the peers named in one. iroh gives the connecting peer's
key cryptographically, so a grant names a proven identity, not a shareable address:
you cannot hand out reach by leaking a string
([ADR 0001](docs/adr/0001-directed-grants.md)).

**Enrollment.** An incoming connection from an unknown key is refused unless it
presents a one-time token minted by `grant-token`. A valid claim pins the peer's key
under the token's label and is then spent; pins persist across restarts, so a reboot
does not orphan enrolled workloads ([ADR 0002](docs/adr/0002-token-enrollment.md)).

**Forwarding.** Each peer is announced only the ports granted to it. When a peer
grants you port 3001, a local TCP listener binds `127.0.0.1:3001` on your side, and
traffic runs over the encrypted QUIC connection. It works both ways -- something
running locally on `:4001` becomes reachable on the peer with `iroh-gate expose 4001`.

**Reconnection.** If the connection drops, both sides reconnect with exponential
backoff. Existing bindings stay in place and resume when the link comes back.

## See also

[ngrok](https://ngrok.com) and [Cloudflare Tunnel](https://developers.cloudflare.com/cloudflare-one/connections/connect-networks/)
are great when you need a public URL anyone can reach. iroh-gate is for connecting your
own machines, or sharing a ticket with a friend so they can see something you're
working on.

[SSH tunnels](https://www.ssh.com/academy/ssh/tunneling) need inbound access on at
least one side. iroh-gate works when neither machine has open inbound ports.

[WireGuard](https://www.wireguard.com/), [Tailscale](https://tailscale.com), and
[NetBird](https://netbird.io/) are mesh VPNs that give every machine an IP on a
virtual network. iroh-gate is narrower: you expose specific ports, not your whole
machine, which makes it easier to reason about exactly what is reachable.

[dumbpipe](https://github.com/n0-computer/dumbpipe) is the direct inspiration.
