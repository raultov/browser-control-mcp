import sys,os,threading,time; sys.path.insert(0,os.getcwd()+'/tests/compare'); from mcp_client import Server
from http.server import ThreadingHTTPServer as TS, BaseHTTPRequestHandler as BH
PAGE=b'<title>r</title><script src=/app.js></script>'
JS=b"window.done=fetch('/api',{method:'POST',body:'x'}).then(r=>r.text());"
class H(BH):
  def log_message(s,*a): pass
  def send(s,b,ct):
    s.send_response(200); s.send_header('Content-Type',ct); s.send_header('Content-Length',str(len(b))); s.end_headers(); s.wfile.write(b)
  def do_GET(s): s.send(JS,'application/javascript') if s.path.startswith('/app.js') else s.send(PAGE,'text/html')
  def do_POST(s): s.rfile.read(int(s.headers.get('Content-Length') or 0)); s.send(b'{"ok":1}','application/json')
srv=TS(('127.0.0.1',0),H); threading.Thread(target=srv.serve_forever,daemon=True).start()
u=f'http://127.0.0.1:{srv.server_address[1]}/'
missed=0
for run in range(5):
  s=Server(['target/release/browser-control-mcp','--headless']+sys.argv[1:],timeout=90)
  try:
    s.call('navigate',{'url':u})
    s.call('evaluate_js',{'expression':'await window.done'}); time.sleep(1)
    t=s.call('get_network_logs',{}).text
  finally: s.close()
  ok=('POST '+u+'api') in t and ('GET '+u+' ') in t
  missed+=0 if ok else 1
  print('run',run,'captured' if ok else 'MISSING page requests:', '' if ok else t.strip().replace('\n',' | ')[:300])
print('MISSED_RUNS',missed,'of 5')
sys.exit(1 if missed else 0)
