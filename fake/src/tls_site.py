# A tiny https and wss server for the end-to-end tests: `tls_site.py CERT KEY LOG`.
# Prints the port it listens on, then serves until it is killed. Every request path is appended to
# LOG, one per line, so that a test can tell what reached it.
import base64
import hashlib
import http.server
import socketserver
import ssl
import sys

cert, key, log = sys.argv[1:4]

PAGE = b"""<title>secure</title><script>
function show() { document.title = 'secure fetch=' + (window.fetched || '-') + ' ws=' + (window.wsmsg || '-'); }
fetch('/ping').then(r => r.text()).then(t => { window.fetched = t; show(); }).catch(e => { window.fetched = 'failed'; show(); });
const ws = new WebSocket('wss://' + location.host + '/ws');
ws.onmessage = e => { window.wsmsg = e.data; show(); };
ws.onerror = () => { window.wsmsg = 'failed'; show(); };
</script>"""


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def do_GET(self):
        with open(log, "a") as f:
            f.write(self.path + "\n")
        if self.path == "/ws" and self.headers.get("Upgrade", "").lower() == "websocket":
            accept = base64.b64encode(
                hashlib.sha1(
                    (self.headers["Sec-WebSocket-Key"] + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()
                ).digest()
            ).decode()
            self.send_response(101)
            self.send_header("Upgrade", "websocket")
            self.send_header("Connection", "Upgrade")
            self.send_header("Sec-WebSocket-Accept", accept)
            self.end_headers()
            message = b"hello"
            self.wfile.write(bytes([0x81, len(message)]) + message)
            self.wfile.flush()
            self.close_connection = True
            try:
                self.connection.recv(1024)
            except OSError:
                pass
            return
        body = PAGE if self.path == "/" else b"pong"
        self.send_response(200)
        self.send_header("Content-Type", "text/html; charset=utf-8" if self.path == "/" else "text/plain")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


class Server(socketserver.ThreadingMixIn, http.server.HTTPServer):
    daemon_threads = True


context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
context.load_cert_chain(cert, key)
server = Server(("127.0.0.1", 0), Handler)
server.socket = context.wrap_socket(server.socket, server_side=True)
print(server.server_address[1], flush=True)
server.serve_forever()
