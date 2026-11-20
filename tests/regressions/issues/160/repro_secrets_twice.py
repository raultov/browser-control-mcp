import sys
sys.path.insert(0, 'tests/compare')
from mcp_client import Server
from siteserver import Site

browser = sys.argv[1] if len(sys.argv) > 1 else 'chrome'
site = Site()
s = Server(['target/release/browser-control-mcp', '--headless', '--secrets', 'repro_secrets_twice.env']
           + (['--browser', 'firefox'] if browser == 'firefox' else []), timeout=60)
leaks = 0
try:
    s.call('navigate', {'url': site.url('/basic.html')})
    s.call('evaluate_js', {'expression': "const v=['qaZ7\\\"Xk2W','<Vw9P>&mN4'].join('');const d=document.createElement('div');d.id='cfg';d.setAttribute('data-once',JSON.stringify({t:v}));d.setAttribute('data-twice',JSON.stringify(JSON.stringify({t:v})));document.body.append(d);'ok'"})
    for name, tool, args in (('outerHTML', 'evaluate_js', {'expression': "document.getElementById('cfg').outerHTML"}),
                             ('inspect_dom query', 'inspect_dom', {'query': 'data-twice'})):
        t = s.call(tool, args).text
        line = [l for l in t.splitlines() if 'data-twice' in l]
        leaked = any(w in t for w in ('Xk2W', 'Vw9P', 'mN4'))
        leaks += leaked
        print(name, 'LEAK' if leaked else 'masked', line[:1])
finally:
    s.close(); site.stop()
print('SECRET_LEAKED' if leaks else 'ALL_MASKED')
