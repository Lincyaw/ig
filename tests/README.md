# Tests

`cargo test` covers the units. `e2e.sh` covers the thing the units cannot: two
daemons, a real iroh connection, and every backend kind end to end.

```sh
cargo build            # e2e.sh runs target/debug/iroh-gate
tests/e2e.sh
```

It stands up six local backends (http over tcp and over a unix socket, line
echo over both, plus one squatting a port to force a remap), starts two daemons,
has one authorize and grant to the other, and then checks routing, prefix
stripping, Host rewriting, keep-alive, raw tcp and unix forwarding, the local
port remap, and that every kind goes dark when its grant is revoked.

Not in CI: it needs outbound network for iroh's discovery and relay, and the
first dial after a daemon starts retries for up to a minute while its pkarr
record propagates. Run it by hand.

Both daemons run on one host, so this exercises the code paths, not the network
isolation -- the property that the serving machine makes the connection follows
from where `lookup_host` and `connect` run, which is visible in `service.rs`.
