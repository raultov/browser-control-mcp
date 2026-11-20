import sys; sys.path.insert(0,'tests/compare')
from mcp_client import Server
CASES=[
 ('A', "try {\n  await Promise.reject(new Error('boom'))\n}\ncatch (e) {\n  window.qaCaught = e.message\n}", 'window.qaCaught', 'boom'),
 ('B', "try {\n  await Promise.resolve(1)\n}\nfinally {\n  window.qaFin = 'fin-ok'\n}", 'window.qaFin', 'fin-ok'),
 ('C', "if (await Promise.resolve(false)) {\n  window.qaBr = 'then'\n}\nelse {\n  window.qaBr = 'else-ok'\n}", 'window.qaBr', 'else-ok'),
 ('D-control', "if (await Promise.resolve(false)) {\n  window.qaBr2 = 'then'\n} else {\n  window.qaBr2 = 'else-ok2'\n}", 'window.qaBr2', 'else-ok2'),
]
bad=0
s=Server(['target/release/browser-control-mcp','--headless','--browser','chrome'],timeout=120)
try:
  for label,e,chk,cwant in CASES:
    r=s.call('evaluate_js',{'expression':e})
    ok=not r.is_error
    extra=''
    if ok:
      c=s.call('evaluate_js',{'expression':chk})
      ok=(not c.is_error) and cwant in c.text
      extra=' check-> '+c.short(120)
    bad+=not ok; print(label,'OK ' if ok else 'BAD',r.is_error,r.short(200),extra, flush=True)
finally: s.close()
print('BAD_COUNT',bad); sys.exit(1 if bad else 0)
