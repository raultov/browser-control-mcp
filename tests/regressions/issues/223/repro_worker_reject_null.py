import os, sys, tempfile, threading, time
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
sys.path.insert(0, 'tests/compare')
from mcp_client import Server

NESTED = len(sys.argv) < 2 or sys.argv[1] != 'flat'
d = tempfile.mkdtemp()
files = {
    'index.html': "<!doctype html><script>new Worker('/outer.js')</script>",
    'outer.js': ("new Worker('/inner.js');\n" if NESTED else '')
                + "setTimeout(() => Promise.reject(null), 300);\n"
                + "setTimeout(() => Promise.reject('other-rej'), 400);\n",
    'inner.js': "// an idle worker\n",
}
for name, content in files.items():
    with open(os.path.join(d, name), 'w') as f:
        f.write(content)


class H(SimpleHTTPRequestHandler):
    def __init__(self, *a, **k):
        super().__init__(*a, directory=d, **k)

    def log_message(self, *a):
        pass


srv = ThreadingHTTPServer(('127.0.0.1', 0), H)
threading.Thread(target=srv.serve_forever, daemon=True).start()
port = srv.server_address[1]
s = Server(['target/release/browser-control-mcp', '--headless'])
s.call('navigate', {'url': f'http://127.0.0.1:{port}/index.html'})
time.sleep(2)
logs = s.call('get_console_logs', {}).text
s.close()
srv.shutdown()
print(logs)
if 'other-rej' not in logs:
    print('the worker did not run')
    sys.exit(2)
if 'Uncaught (in promise) null' not in logs:
    print('MISSING: Uncaught (in promise) null')
    sys.exit(1)
sys.exit(0)
