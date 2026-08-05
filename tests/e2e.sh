#!/usr/bin/env bash
# End-to-end: two pai-sho daemons.
#
#   A = the "jump host". Declares services. Reaches the internal resources.
#   B = the "outside machine". Only ever talks to A over iroh.
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
WORK="$HERE/e2e-run"
BIN="$(cd "$HERE/.." && pwd)/target/debug/ig"
rm -rf "$WORK"; mkdir -p "$WORK"
cd "$WORK"

# AF_UNIX paths cap at ~108 bytes, well under the work dir's length, so the
# sockets live in a short directory of their own.
SOCKDIR=$(mktemp -d /tmp/pais.XXXX)
HTTP_PORT=18080     # http service, declared in the startup file
TCP_PORT=15500      # tcp service, declared live -- and remapped on B
UNIX_PORT=16000     # unix service, declared live
PLAIN_PORT=19000    # no service: the original --host:<port> behaviour
REMAP_TO=15501      # where B moves TCP_PORT after finding it taken
PLAIN_REMAP=19001   # A and B share this machine, so B cannot bind PLAIN_PORT
                    # either -- A's own backend is sitting on it

# Daemons are launched directly rather than through the a()/b() wrappers, and
# their PIDs recorded: a wrapper runs in a subshell, so `jobs -p` would hold the
# subshell rather than the binary it exec'd, and every aborted run would leave a
# daemon squatting the test ports.
DAEMON_PIDS=()
cleanup() {
  for p in "${DAEMON_PIDS[@]:-}"; do [[ -n $p ]] && kill "$p" 2>/dev/null; done
  for p in $(jobs -p); do kill "$p" 2>/dev/null; done
  wait 2>/dev/null
  rm -rf "$SOCKDIR"
}
trap cleanup EXIT

pass=0; fail=0
check() { # check <name> <expected-substring> <actual>
  if [[ "$3" == *"$2"* ]]; then echo "  PASS  $1"; pass=$((pass+1));
  else echo "  FAIL  $1"; echo "        want substring: $2"; echo "        got: $3"; fail=$((fail+1)); fi
}
talk() { python3 -c '
import socket,sys
s=socket.create_connection(("127.0.0.1",int(sys.argv[1])),timeout=8)
s.sendall(sys.argv[2].encode()); s.settimeout(8)
sys.stdout.write(s.recv(4096).decode(errors="replace"))' "$1" "$2" 2>&1; }
a() { $BIN --socket "$WORK/a.sock" "$@"; }
b() { $BIN --socket "$WORK/b.sock" "$@"; }

echo "== internal resources (only A is supposed to reach these) =="
python3 "$HERE/backends.py" http alpha  tcp  9101 &
python3 "$HERE/backends.py" http beta   tcp  9102 &
python3 "$HERE/backends.py" http onsock unix "$SOCKDIR/http.sock" &
python3 "$HERE/backends.py" echo db     tcp  9201 &
python3 "$HERE/backends.py" echo docker unix "$SOCKDIR/docker.sock" &
python3 "$HERE/backends.py" http plain  tcp  $PLAIN_PORT &
# Something already holding TCP_PORT on B's side, which is the whole reason
# `bind` exists.
python3 "$HERE/backends.py" echo squatter tcp $TCP_PORT &
sleep 1.5
for f in "$SOCKDIR"/http.sock "$SOCKDIR"/docker.sock; do [[ -S $f ]] || { echo "  $f missing"; exit 1; }; done
echo "  ready, and $TCP_PORT is deliberately occupied"

cat > services.toml <<EOF
[[service]]
kind = "http"
port = $HTTP_PORT

  [[service.route]]
  prefix = "/alpha"
  upstream = "127.0.0.1:9101"
  strip_prefix = true
  host_header = "alpha.internal"

  [[service.route]]
  prefix = "/beta"
  upstream = "localhost:9102"
  strip_prefix = true

  [[service.route]]
  prefix = "/uds"
  unix = "$SOCKDIR/http.sock"
  strip_prefix = true
EOF

echo
echo "== daemon A (startup file declares only the http service) =="
$BIN --socket "$WORK/a.sock" daemon --key "$WORK/a.key" --service "$WORK/services.toml" > a.log 2>&1 &
DAEMON_PIDS+=($!)
for _ in $(seq 40); do [[ -S a.sock ]] && break; sleep 0.25; done
TICKET_A=$(a id); echo "  ticket: $TICKET_A"

echo "== daemon B =="
$BIN --socket "$WORK/b.sock" daemon --key "$WORK/b.key" > b.log 2>&1 &
DAEMON_PIDS+=($!)
for _ in $(seq 40); do [[ -S b.sock ]] && break; sleep 0.25; done
TICKET_B=$(b id); echo "  ticket: $TICKET_B"

echo
echo "== A authorizes B, then declares the rest live =="
a peer pin "$TICKET_B" --label outside >/dev/null
a port expose $HTTP_PORT  >/dev/null                                   # already has a backend
a port expose $TCP_PORT   --upstream localhost:9201 >/dev/null         # new tcp service
a port expose $UNIX_PORT  --unix "$SOCKDIR/docker.sock" >/dev/null     # new unix service
a port expose $PLAIN_PORT >/dev/null                                   # no backend: the default
echo "  declared without restarting the daemon"

echo
echo "== list reports what serves each port =="
L=$(a port ls --format json)
echo "$L" | python3 -c 'import json,sys; [print("   ", e["port"], "->", e["backend"]) for e in json.load(sys.stdin)["exposed"]]'
check "list names the http backend"    "\"backend\": \"http (3 routes)\""               "$L"
check "list names the tcp backend"     "\"backend\": \"tcp localhost:9201\""            "$L"
check "list names the unix backend"    "\"backend\": \"unix $SOCKDIR/docker.sock\""     "$L"
check "list marks an undeclared port"  "\"backend\": \"default 127.0.0.1:$PLAIN_PORT\"" "$L"

echo
echo "== B dials A =="
# Resolving an endpoint id goes through n0's pkarr/DNS, and A's record is not
# published the instant it binds. Retry until it lands.
dialed=no
for i in $(seq 20); do
  if b peer add "$TICKET_A" >/dev/null 2>&1; then echo "  connected on attempt $i"; dialed=yes; break; fi
  sleep 3
done
[[ $dialed == yes ]] || { echo "  could not dial A"; cat a.log b.log; exit 1; }

echo -n "  waiting for the two bindable ports "
for _ in $(seq 60); do
  n=$(b status --format json 2>/dev/null | grep -Ec "\"port\": ($HTTP_PORT|$UNIX_PORT)")
  if [[ "$n" == 2 ]]; then echo "ok"; break; fi
  echo -n "."; sleep 1
done
[[ "${n:-0}" == 2 ]] || { echo " TIMEOUT"; cat a.log b.log; exit 1; }

echo
echo "== the port that was already taken on B =="
check "B could not bind the squatted port"  "failed to bind 127.0.0.1:$TCP_PORT"  "$(cat b.log)"
b port bind $TCP_PORT   --local $REMAP_TO   >/dev/null
b port bind $PLAIN_PORT --local $PLAIN_REMAP >/dev/null
sleep 1
BL=$(b status --format json)
check "list shows the remapped local port"  "\"local\": $REMAP_TO"                "$BL"
R=$(talk $REMAP_TO "select 1")
check "the service works on the new port"   "db says: select 1"                   "$R"
R=$(talk $TCP_PORT "select 1")
check "the squatter still owns the old one" "squatter says: select 1"             "$R"

echo
echo "== http service =="
R=$(curl -s --max-time 10 "http://127.0.0.1:$HTTP_PORT/alpha/")
check "routes /alpha, strips prefix"        '"path": "/"'                         "$R"
check "rewrites Host to host_header"        '"host_header": "alpha.internal"'     "$R"
R=$(curl -s --max-time 10 "http://127.0.0.1:$HTTP_PORT/beta/some/page")
check "routes /beta, keeps sub-path"        '"path": "/some/page"'                "$R"
R=$(curl -s --max-time 10 "http://127.0.0.1:$HTTP_PORT/uds/hello")
check "routes to HTTP on a unix socket"     '"site": "onsock"'                    "$R"
R=$(curl -s --max-time 10 -o /dev/null -w '%{http_code}' "http://127.0.0.1:$HTTP_PORT/nope")
check "404s an unrouted path"               "404"                                 "$R"
R=$(curl -s --max-time 10 "http://127.0.0.1:$HTTP_PORT/alpha/1" "http://127.0.0.1:$HTTP_PORT/uds/2")
check "keep-alive across two requests"      '"path": "/2"'                        "$R"

echo
echo "== unix service, and the unchanged default =="
R=$(talk $UNIX_PORT "GET /containers")
check "forwards raw bytes to the socket"    "docker says: GET /containers"        "$R"
R=$(curl -s --max-time 10 "http://127.0.0.1:$PLAIN_REMAP/still/works")
check "undeclared port still forwards"      '"site": "plain"'                     "$R"
check "and it kept the original path"       '"path": "/still/works"'              "$R"

echo
echo "== the contract: dry-run, exit codes, stream hygiene =="
BEFORE=$(a port ls --format json)
a port expose 17999 --upstream localhost:9201 --dry-run >/dev/null 2>&1
AFTER=$(a port ls --format json)
if [[ "$BEFORE" == "$AFTER" ]]; then
  echo "  PASS  --dry-run changed nothing"; pass=$((pass+1))
else
  echo "  FAIL  --dry-run changed nothing"; fail=$((fail+1))
fi
R=$(a port expose 17999 --upstream localhost:9201 --dry-run --format json 2>/dev/null)
check "--dry-run reports dry_run: true"     '"dry_run": true'                     "$R"
a port expose 17998 --to deadbeef >/dev/null 2>&1
check "bad peer key exits 2 (invalid)"      "2"                                   "$?"
a port unexpose 17997 --dry-run >/dev/null 2>&1
check "dry-run on an unknown port exits 0"  "0"                                   "$?"
OUT=$(a port ls 2>/dev/null)
ERR=$(a port ls 2>&1 >/dev/null)
check "port ls puts data on stdout"         "$TCP_PORT"                           "$OUT"
if [[ -z "$ERR" ]]; then
  echo "  PASS  port ls keeps stderr empty"; pass=$((pass+1))
else
  echo "  FAIL  port ls keeps stderr empty"; echo "        got: $ERR"; fail=$((fail+1))
fi
OUT=$(a port expose $PLAIN_PORT 2>/dev/null)
if [[ -z "$OUT" ]]; then
  echo "  PASS  an action writes nothing to stdout"; pass=$((pass+1))
else
  echo "  FAIL  an action writes nothing to stdout"; echo "        got: $OUT"; fail=$((fail+1))
fi

echo
echo "== unexpose retires the service, however it is spelled =="
# Revoking the one remaining grantee by name reaches the same state as revoking
# them all, so it has to land the same way.
a port unexpose $UNIX_PORT --to "$TICKET_B" >/dev/null
sleep 2
if a port ls --format json | grep -q "unix $SOCKDIR/docker.sock"; then
  echo "  FAIL  --to <last grantee> retires the service"; fail=$((fail+1))
else
  echo "  PASS  --to <last grantee> retires the service"; pass=$((pass+1))
fi
R=$(talk $UNIX_PORT "ping")
check "and stops serving"                   "refused"                             "$R"

a port unexpose $TCP_PORT >/dev/null
sleep 2
if a port ls --format json | grep -q "tcp localhost:9201"; then
  echo "  FAIL  bare unexpose retires the service"; fail=$((fail+1))
else
  echo "  PASS  bare unexpose retires the service"; pass=$((pass+1))
fi
R=$(talk $REMAP_TO "select 1")
check "revoked port stops serving"          "refused"                             "$R"

echo
echo "passed: $pass   failed: $fail"
[[ $fail -eq 0 ]]
