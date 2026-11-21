import os, sys, tempfile
sys.path.insert(0, os.path.join(os.getcwd(), 'tests', 'compare'))
from mcp_client import Server
from siteserver import Site

env = os.path.join(tempfile.mkdtemp(), 'esc.env')
open(env, 'w').write("CUTP='Lng7Prefix&Tail9xqLongEnd'\nCUTQ='Jsn7Prefix\"Tail8yz'\n")
site = Site()
s = Server(['target/release/browser-control-mcp', '--headless', '--secrets', env], timeout=120)
s.call('navigate', {'url': site.url('/basic.html')})
s.call('evaluate_js', {'expression': "document.body.innerHTML=''; const d=document.createElement('div'); d.id='m'; "
                       "d.setAttribute('data-k','Lng7Prefix&Tail9xqLongEnd'); "
                       "d.setAttribute('data-c', JSON.stringify({k:'Jsn7Prefix\"Tail8yz'})); document.body.append(d); 1"})
cases = [
    ('A', {'query': 'id="m" data-k="', 'before': 0, 'after': 13}, ['Lng7Prefix']),
    ('B', {'query': '" data-c="', 'before': 16, 'after': 0}, ['Tail9xq', 'LongEnd']),
    ('C', {'query': 'data-c="{&quot;k&quot;:&quot;', 'before': 0, 'after': 11}, ['Jsn7Prefix']),
]
leaks = 0
for name, args, parts in cases:
    text = s.call('inspect_dom', args).text
    shown = [p for p in parts if p in text]
    print(name, 'context:', [line for line in text.splitlines() if line and not line.startswith(('Found', '[...]'))])
    if shown:
        leaks += 1
        print('LEAK in %s: %s' % (name, shown))
control = s.call('inspect_dom', {'query': 'id="m" data-k="', 'before': 0, 'after': 40}).text
print('control (whole value masked):', '<secret:CUTP>' in control)
s.close()
sys.exit(1 if leaks else 0)
