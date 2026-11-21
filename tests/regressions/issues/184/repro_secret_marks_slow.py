import os, sys, tempfile
sys.path.insert(0, os.path.join(os.getcwd(), 'tests', 'compare'))
from mcp_client import Server
from siteserver import Site

env = os.path.join(tempfile.mkdtemp(), 's100.env')
open(env, 'w').write('\n'.join("S%03d='Vq%03dKx7Lm9Pz2Rt5Wb8Nc4Hd6Jf3Gs1Ay0Ue'" % (i, i) for i in range(100)) + '\n')
site = Site()
# One escape decoded six times (%25... -> A), then 99 985 cut marks.
expr = "'%25252525252541' + '…'.repeat(99985)"
ms = {}
for tag, flags in (('without', []), ('with', ['--secrets', env])):
    s = Server(['target/release/browser-control-mcp', '--headless'] + flags, timeout=120)
    s.call('navigate', {'url': site.url('/basic.html')})
    s.call('evaluate_js', {'expression': '1'})
    r = s.call('evaluate_js', {'expression': expr}, timeout=100)
    ms[tag] = r.ms
    print('%s --secrets: is_error=%s, %d ms' % (tag, r.is_error, r.ms))
    s.close()
diff = ms['with'] - ms['without']
print('masking cost: %d ms' % diff)
if diff > 3000:
    print('SLOW: masking a 100 000-character reply took more than 3 s')
    sys.exit(1)
