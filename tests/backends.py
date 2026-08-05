#!/usr/bin/env python3
"""Stand-ins for internal resources, over TCP or a Unix socket.

  backends.py http  <name> tcp  <port>
  backends.py http  <name> unix <path>
  backends.py echo  <name> tcp  <port>
  backends.py echo  <name> unix <path>
"""
import json
import os
import socket
import socketserver
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


def make_handler(name):
    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def do_GET(self):
            body = json.dumps(
                {
                    "site": name,
                    "path": self.path,
                    "host_header": self.headers.get("Host"),
                }
            ).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, *a):
            pass

    return Handler


class UnixHTTPServer(ThreadingHTTPServer):
    address_family = socket.AF_UNIX

    def server_bind(self):
        socketserver.TCPServer.server_bind(self)
        self.server_name = "localhost"
        self.server_port = 0


def echo_loop(conn, name):
    with conn:
        data = conn.recv(4096)
        if data:
            conn.sendall(b"%s says: %s" % (name.encode(), data))


def serve_echo(name, family, address):
    srv = socket.socket(family, socket.SOCK_STREAM)
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(address)
    srv.listen(16)
    while True:
        conn, _ = srv.accept()
        threading.Thread(target=echo_loop, args=(conn, name), daemon=True).start()


if __name__ == "__main__":
    proto, name, transport, where = sys.argv[1:5]
    if transport == "unix":
        try:
            os.unlink(where)
        except FileNotFoundError:
            pass

    if proto == "http":
        if transport == "unix":
            UnixHTTPServer(where, make_handler(name)).serve_forever()
        else:
            ThreadingHTTPServer(("127.0.0.1", int(where)), make_handler(name)).serve_forever()
    else:
        if transport == "unix":
            serve_echo(name, socket.AF_UNIX, where)
        else:
            serve_echo(name, socket.AF_INET, ("127.0.0.1", int(where)))
