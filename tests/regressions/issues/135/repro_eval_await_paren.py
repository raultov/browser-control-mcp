import sys,os; sys.path.insert(0,os.getcwd()+'/tests/compare'); from mcp_client import Server
s=Server(['target/release/browser-control-mcp','--headless']+sys.argv[1:],timeout=90)
bad=0
try:
  s.call('navigate',{'url':'about:blank'})
  ctl=s.call('evaluate_js',{'expression':"const r=await Promise.resolve('a'); r"}); print('control const r=await ...:',ctl.is_error,ctl.short(120))
  for e in ["await (await Promise.resolve('a'))", "(await Promise.resolve('abc')).length", "await (1+2)"]:
    r=s.call('evaluate_js',{'expression':e}); print(repr(e),'isError=',r.is_error,r.short(150)); bad+=r.is_error
finally: s.close()
print('BROKEN_FORMS',bad)
sys.exit(1 if bad else 0)
