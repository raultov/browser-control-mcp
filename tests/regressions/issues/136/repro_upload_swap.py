import sys,os,threading,tempfile,shutil; sys.path.insert(0,os.getcwd()+'/tests/compare'); from mcp_client import Server
from http.server import ThreadingHTTPServer as TS, BaseHTTPRequestHandler as BH
class H(BH):
  def log_message(s,*a): pass
  def do_GET(s):
    b=b'<title>u</title><input type=file id=f>'; s.send_response(200); s.send_header('Content-Type','text/html'); s.send_header('Content-Length',str(len(b))); s.end_headers(); s.wfile.write(b)
srv=TS(('127.0.0.1',0),H); threading.Thread(target=srv.serve_forever,daemon=True).start()
B=tempfile.mkdtemp(prefix='qa-upload-',dir=os.path.expanduser('~')); R=B+'/root'
os.makedirs(R); os.makedirs(B+'/outside')
open(R+'/ok.txt','w').write('INSIDE-OK-0000\n'); open(B+'/outside/secret.txt','w').write('OUTSD-MARK5521\n')
st=os.stat(R+'/ok.txt'); os.utime(B+'/outside/secret.txt',ns=(st.st_atime_ns,st.st_mtime_ns))
os.symlink(R+'/ok.txt',R+'/link.txt')
s=Server(['target/release/browser-control-mcp','--headless','--tool-groups','files','--upload-root',R]+sys.argv[1:],timeout=90)
out=[]
try:
  s.call('navigate',{'url':f'http://127.0.0.1:{srv.server_address[1]}/'})
  r=s.call('upload_file',{'selector':'#f','paths':[B+'/outside/secret.txt']}); print('direct outside refused:',r.is_error,r.short())
  r=s.call('upload_file',{'selector':'#f','paths':[R+'/link.txt']}); print('link inside accepted:',not r.is_error,r.short())
  os.remove(R+'/link.txt'); os.symlink(B+'/outside/secret.txt', R+'/link.txt')
  r=s.call('evaluate_js',{'expression':"await document.getElementById('f').files[0].text()"}); print('page read:',r.short()); out.append(r.text)
finally:
  s.close(); shutil.rmtree(B,ignore_errors=True)
print('LEAK' if any('MARK5521' in t for t in out) else 'NO_LEAK')
