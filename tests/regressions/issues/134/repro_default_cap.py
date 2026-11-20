import sys,os,threading; sys.path.insert(0,os.getcwd()+'/tests/compare'); from mcp_client import Server
from http.server import ThreadingHTTPServer as TS, BaseHTTPRequestHandler as B
class H(B):
  def log_message(s,*a): pass
  def do_GET(s):
    b=b'<title>ok</title>ok'; s.send_response(200); s.send_header('Content-Type','text/html'); s.send_header('Content-Length',str(len(b))); s.end_headers(); s.wfile.write(b)
srv=TS(('127.0.0.1',0),H); threading.Thread(target=srv.serve_forever,daemon=True).start()
url=f'http://127.0.0.1:{srv.server_address[1]}/'
BASE=['target/release/browser-control-mcp','--headless','--max-instances','2']+sys.argv[1:]
s=Server(BASE,timeout=120)
try:
  opens=[]
  for i in range(1,4):
    r=s.call('open_instance',{'label':f'i{i}','browser':'chrome'}); opens.append(r); print('open',i,r.is_error,r.short(200))
  d=s.call('navigate',{'url':url}); print('default',d.is_error,d.short(300))
finally: s.close()
refused=any(r.is_error and 'Instance limit reached' in r.text for r in opens)
print('RESULT','DEFAULT_OK' if (not d.is_error and d.text.startswith('Navigated to')) else 'DEFAULT_BLOCKED','REFUSED',refused)
