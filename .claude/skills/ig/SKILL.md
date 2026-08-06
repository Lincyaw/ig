---
name: ig
description: "Operate ig, a peer-to-peer tunnel that makes one machine's internal network reachable from another machine -- internal-only websites, databases on private hosts, unix sockets, ports behind NAT. Use this skill whenever the user wants to reach something only one specific machine can see (a corporate intranet site, a database behind a bastion, a docker socket on a laptop), whenever they mention ig / iroh-gate / a jump host / tunnelling or tunneling into an internal network, and whenever an ig command, a services.toml, or a route table needs writing or debugging. Use it even when they describe the problem without naming the tool: 'I need a VPN into the office', 'expose this port to my other box', 'my server can't resolve that hostname but my laptop can'. Do NOT use it for ordinary local port forwarding that plain ssh -L already solves between two reachable hosts, or for public-internet ingress tools like ngrok or Cloudflare Tunnel."
---

# ig

`ig` moves reach, not packets you have to think about. A machine that can see
something makes it appear as a port on another machine's `localhost`. Neither
side needs an open inbound port or a public IP.

Two daemons, one per machine. Keep the roles straight, because getting them
backwards is the most common mistake:

- **inside** -- the machine that *can already reach* the thing. It declares what
  is worth reaching and grants it. Every upstream connection is made from here,
  so names resolve with this machine's resolver and the upstream sees this
  machine's address.
- **outside** -- the machine that *wants* it. Granted ports simply appear on its
  `127.0.0.1`. It runs no configuration of its own.

If the user says "I want to reach X from Y", then X's machine is inside and Y is
outside. Say which is which back to them before writing commands -- a
misread here produces a setup that looks right and connects to nothing.

## Choosing the backend

This is the decision that matters, and `--help` will not make it for you. On the
inside machine, an exposed port needs to know what serves it:

| The thing you want | Flag | Why this one |
|---|---|---|
| One `host:port` only that machine can reach | `--upstream db.internal:5432` | Raw bytes, any protocol. The name resolves on the inside machine. |
| Several internal websites behind one port, routed by path or Host | `--routes sites.toml` | A real reverse proxy: path routing, prefix stripping, Host rewriting, TLS termination. |
| A unix socket that exists only on that filesystem | `--unix /var/run/docker.sock` | Raw bytes to the socket. |
| Something on the inside machine's own localhost | *(no flag)* | Falls back to `--host:<port>`, default `127.0.0.1`. |

Two traps worth naming up front:

- **Reach for `--upstream` before `--routes`.** A single internal site does not
  need a route table; `--upstream wiki.internal:80` is enough and has fewer
  moving parts. Use `--routes` only when one port must serve more than one
  upstream, or when you need Host rewriting or TLS termination.
- **A unix socket forwarded out arrives as a TCP port.** `--unix` is about where
  the *inside* end lives. The outside machine always gets `127.0.0.1:<port>`, so
  tools that insist on a socket path need their TCP equivalent
  (`DOCKER_HOST=tcp://127.0.0.1:6000`).

## Setting it up

Two commands per side, in this order. `ig id` prints a machine's ticket, which
is its public key -- safe to paste anywhere.

```sh
# --- inside ---
ig daemon &
ig id                                              # -> TICKET_INSIDE

# --- outside ---
ig daemon &
ig id                                              # -> TICKET_OUTSIDE

# --- inside: authorize, then declare ---
ig peer pin <TICKET_OUTSIDE> --label laptop
ig port expose 5432 --upstream db.internal:5432
ig port expose 8080 --routes sites.toml

# --- outside: dial in ---
ig peer add <TICKET_INSIDE>
```

Both halves are required and they are not symmetric: the inside machine
authorizes the outside key, and the outside machine dials the inside ticket.
Authorizing without dialing leaves nothing connected; dialing without being
authorized is refused.

Granted ports then bind themselves on the outside machine. Nothing to import.

### The alternative: startup flags

Everything above also works as daemon arguments, which is what you want when the
machine boots unattended or you are writing a systemd unit:

```sh
ig daemon --service services.toml -a <TICKET_OUTSIDE> -e 5432,8080
```

`--service` takes the same declarations as `expose`, in a file (see the
reference below). Note the asymmetry: ports declared in a `--service` file are
granted to the `-a` peers automatically, whereas a runtime `ig port expose`
grants to every currently known peer unless `--to <key>` names one.

### The alternative: enrollment tokens

When you cannot get the outside machine's key in advance -- a VM you are about
to boot, a container in CI -- mint a one-time token instead:

```sh
ig peer token --label ci > /run/token          # inside
ig daemon -a <TICKET_INSIDE> --enroll-file /run/token   # outside
```

Three things about tokens that will bite otherwise: they live in memory, so
restarting the inside daemon voids every outstanding one; they expire after five
minutes; and they are only claimable at `ig daemon` startup, not by
`ig peer add`. Prefer `--enroll-file` over `--enroll <TOKEN>` -- an argv value is
visible in the process table and lands in shell history.

## Checking that it worked

`ig status` on either side. Read it from the inside to confirm the grant landed,
from the outside to confirm the port bound:

```sh
ig status                       # for a person
ig status --format json         # for a script
```

The exposed table names the backend, which is the fastest way to catch a port
declared with the wrong one. Then just use the port: `curl`, `psql`, whatever
the protocol actually is. A tunnel that resolves but returns nothing usually
means the backend is wrong, not that the tunnel is broken.

## When it does not work

Work down this list. Most of it is not guessable from the error message.

**`peer add` fails right after the other daemon started.** Expected, not a
misconfiguration. Endpoint discovery goes through n0's pkarr/DNS and the record
is not published the instant the daemon binds. `peer add` does not retry on its
own, so retry it yourself every few seconds; it typically lands within 15
seconds. Do not start rewriting the config over this.

**A granted port never appears on the outside machine.** The grant names a
specific key. Run `ig port ls` on the inside and check the `to` column actually
holds the outside machine's key -- a grant to the wrong key, or to nobody, looks
identical to a working one until you look.

**The daemon log says `failed to bind 127.0.0.1:<port>`.** Something local
already owns that number. The peer chose it, so move it on your side:

```sh
ig port bind 5432 --local 5433     # the peer's 5432 arrives on localhost:5433
ig port bind 5432 --local 0        # or let the OS pick; status reports which
ig port bind 5432 --clear          # back to the announced number
```

It takes effect immediately, with no need to wait for a re-announce.

**An HTTP upstream serves the wrong site, or a default landing page.** The Host
header. The client dialed `127.0.0.1:8080` through a tunnel, so its Host names
the tunnel rather than the site, and an upstream that vhosts on Host has no way
to know better. Set `host_header` on the route.

**A port stopped working after a revoke.** Revoking the *last* grant for a port
retires its backend, whichever way the revoke was spelled. Re-`expose` it with
the backend again, not just the grant.

**Exit code 7.** No daemon is listening on that control socket. Check `--socket`
or `IG_SOCKET`; the default is `/tmp/ig.sock`.

**Two daemons on one machine collide.** They need distinct `--socket` *and*
distinct `--key`, or the second silently shares the first's identity.

**HTTP/2 or a WebSocket through the http backend.** Downstream is HTTP/1.1 only,
by design -- a QUIC stream cannot un-read bytes, so the h2c preface sniff is
skipped. WebSocket upgrades are untested. Browsers through the tunnel are
unaffected. If the user needs h2c end to end, `--upstream` instead of `--routes`
carries bytes without interpreting them.

## Driving it from a script or an agent

`ig` is built to be called without a human present, so use that rather than
parsing prose:

- `--format json` on any command; `--dry-run` on every mutating one.
- Exit codes carry the reason: `2` invalid, `3` not found, `4` denied, `5`
  conflict, `7` no daemon. Branch on these, never on the message text.
- `detail`, `backend`, and error message strings are human-facing and may be
  reworded in any release. Do not build logic on them.
- Nothing prompts or reads stdin, ever, so one call always completes an
  operation.

```sh
ig status --format json | jq -r '.exposed[].port'
ig port expose 8080 --routes sites.toml --dry-run --format json
```

`ig --dump-schema` emits the whole command tree as JSON, generated from the
parser itself. Read it when constructing an invocation you are unsure of --
it cannot drift from what `ig` actually accepts. `docs/CONTRACT.md` in the repo
is the prose version.

## Reference

### Route tables (`--routes`)

A list tried in order, first match wins. A route needs `upstream` or `unix`, and
nothing else is required.

```toml
[[route]]
prefix = "/gitlab"
upstream = "gitlab.internal:80"     # resolved by the inside machine
strip_prefix = true
host_header = "gitlab.internal"     # set this whenever the upstream vhosts

[[route]]
host = "wiki.local"                 # matched against the request's Host header
upstream = "wiki.internal:443"
tls = true                          # terminate TLS here, so the outside need not

[[route]]
prefix = "/app"
unix = "/run/app.sock"              # an HTTP server on a unix socket
```

| Key | Meaning |
|---|---|
| `host` | Match the request's Host header, hostname only. Optional. |
| `prefix` | Match a path prefix. Optional. |
| `upstream` | Where to send it, `host:port`. This or `unix` is required. |
| `unix` | Send it to an HTTP server on a unix socket instead. |
| `tls` | Speak TLS to the upstream. |
| `sni` | SNI to present. Defaults to the upstream hostname. |
| `strip_prefix` | Remove `prefix` from the path before forwarding. |
| `host_header` | Host header to send. Defaults to the upstream hostname. |

A route with neither `host` nor `prefix` matches everything, so put it last. An
unmatched request gets a 404.

### Startup declarations (`--service`)

The same three backends, in one file. `kind` picks which:

```toml
[[service]]
kind = "http"
port = 8080

  [[service.route]]                 # the route table, inline
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

### Command tree

```
ig daemon [options]              Start the daemon
ig id                            This daemon's endpoint id (its ticket)
ig status                        Peers, exposed ports, local bindings

ig peer add <ticket>             Connect to a peer
ig peer rm <ticket>              Disconnect, drop its pin
ig peer ls                       Known peers, and what they expose to us
ig peer pin <key> --label <l>    Authorize by key, no token
ig peer token --label <l>        Mint a one-time token (5 min, in memory)

ig port expose <port> [--to <key>] [--upstream H:P | --unix P | --routes F]
ig port unexpose <port> [--to <key>]
ig port ls                       Exposed ports and what serves each
ig port bind <port> --local <p>  Land a peer's port elsewhere

ig completion <shell>
```

Global on every command: `--socket` (`IG_SOCKET`), `--format` (`IG_FORMAT`),
`--quiet` (`IG_QUIET`), `--no-input`, and `--dry-run` on mutating ones.

State lives at `$XDG_STATE_HOME/ig/key` (default `~/.local/state/ig/key`), with
pins beside it as `<key>.peers.json`. The key is the machine's identity: keep it
and the ticket stays stable across restarts, which is what lets you bake one
ticket into a workload you boot repeatedly.
