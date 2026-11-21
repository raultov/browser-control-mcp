import os, statistics, sys, tempfile
sys.path.insert(0, os.path.join(os.getcwd(), 'tests', 'compare'))
from mcp_client import Server

# 100 secrets, each a 4-13 character window of a 10-character period, so every
# position of the reply starts 10 overlapping secrets.
P = 'Qx7Lm9Pz2R'
vals = [(P * 3)[o:o + L] for o in range(10) for L in range(4, 14)]
env = os.path.join(tempfile.mkdtemp(), 's.env')
with open(env, 'w') as f:
    f.write(''.join("S%03d='%s'\n" % (i, v) for i, v in enumerate(vals)))
escape = sys.argv[1] if len(sys.argv) > 1 else '%25252525252541'
# A string value of under 100 000 characters (the reply's text stays under it too).
expr = "'%s' + '%s'.repeat(9990)" % (escape, P)

def median_ms(flags):
    s = Server(['target/release/browser-control-mcp', '--headless'] + flags, timeout=300)
    s.call('evaluate_js', {'expression': expr}, timeout=300)
    t, last = [], None
    for _ in range(3):
        last = s.call('evaluate_js', {'expression': expr}, timeout=300)
        t.append(last.ms)
    s.close()
    return statistics.median(t), last

plain, r0 = median_ms([])
masked, r1 = median_ms(['--secrets', env])
added = masked - plain
print('reply chars without --secrets', len(r0.text), 'error', r1.is_error, 'masked', '<secret:' in r1.text)
print('median ms without', plain, 'with', masked, 'added', added)
if added >= 3000:
    print('BUG: masking a reply of under 100 000 characters added %d ms (README: under 3 seconds)' % added)
    sys.exit(1)
print('ok')
