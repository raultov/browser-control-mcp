import sys, os, re, time, threading, functools, tempfile
from http.server import ThreadingHTTPServer, SimpleHTTPRequestHandler
sys.path.insert(0, 'tests/compare')
from mcp_client import Server

d = tempfile.mkdtemp()
open(os.path.join(d, 'top.html'), 'w').write(
    "<!doctype html><html><body><p>focus host</p>"
    "<iframe id=\"deny\" width=\"420\" height=\"140\" allow=\"focus-without-user-activation 'none'\"></iframe>"
    "<iframe id=\"plain\" width=\"420\" height=\"140\"></iframe>"
    "<iframe id=\"same\" width=\"420\" height=\"140\" allow=\"focus-without-user-activation 'none'\"></iframe>"
    "<script>const o='http://localhost:'+location.port; deny.src=o+'/inner.html?n=deny'; "
    "plain.src=o+'/inner.html?n=plain'; same.src='/inner.html?n=same';</script></body></html>")
open(os.path.join(d, 'inner.html'), 'w').write(
    "<!doctype html><html><body><input id=\"q\"><input id=\"b\" onfocus=\"this.blur()\">"
    "<span id=\"out\"></span> <span id=\"md\"></span><script>const n=new URLSearchParams(location.search).get('n'); "
    "q.setAttribute('aria-label','field '+n); b.setAttribute('aria-label','blurry '+n); "
    "b.oninput=()=>out.textContent='binput-'+n+':'+b.value; b.onmousedown=()=>md.textContent='md-'+n;</script></body></html>")


class H(SimpleHTTPRequestHandler):
    def log_message(self, *a):
        pass


httpd = ThreadingHTTPServer(('127.0.0.1', 0), functools.partial(H, directory=d))
threading.Thread(target=httpd.serve_forever, daemon=True).start()
port = httpd.server_address[1]
s = Server(['target/release/browser-control-mcp', '--headless'], timeout=90)
s.call('navigate', {'url': f'http://127.0.0.1:{port}/top.html'})
time.sleep(2)
S = s.call('snapshot', {})
ref = dict(re.findall(r'textbox "blurry (deny|plain|same)".*?\[ref=(\w+)\]', S.text))
print('refs', ref)
blamed = 0
for n in ['plain', 'deny', 'same']:
    for tool, args in [('fill_input', {'selector': ref[n], 'text': 'nope'}),
                       ('press_key', {'selector': ref[n], 'key': 'x'})]:
        r = s.call(tool, args)
        print(n, tool, 'is_error=%s' % r.is_error, r.short(300))
        if n != 'plain' and 'Permissions-Policy' in r.text:
            blamed += 1
print('find binput-plain:', s.call('find_in_snapshot', {'text': 'binput-plain'}).short(300))
s.close()
print('POLICY_BLAMED:', blamed)
sys.exit(1 if blamed else 0)
