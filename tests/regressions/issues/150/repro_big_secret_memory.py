import os, sys, time, signal, subprocess, tempfile, json, select
# One 32 KB symbol-heavy secret value; watch the server's memory until it answers initialize.
n = int(sys.argv[1]) if len(sys.argv) > 1 else 32000
d = tempfile.mkdtemp()
env = os.path.join(d, 'B.env')
open(env, 'w', encoding='utf-8').write("BIG='" + ('"&<>/é\\' * 4600)[:n] + "END7319'\n")
p = subprocess.Popen(['target/release/browser-control-mcp', '--headless', '--secrets', env],
                     stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
p.stdin.write((json.dumps({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
    "protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "repro", "version": "1"}}}) + "\n").encode())
p.stdin.flush()
t0 = time.time(); peak = 0; verdict = None
LIMIT_KB = 3 * 1024 * 1024
while True:
    try:
        rss = int([l for l in open('/proc/%d/status' % p.pid) if l.startswith('VmRSS')][0].split()[1])
    except Exception:
        rss = 0
    peak = max(peak, rss)
    if p.poll() is not None:
        verdict = 'EXITED code %s stderr=%r' % (p.returncode, p.stderr.read()[:300]); break
    r, _, _ = select.select([p.stdout], [], [], 0.2)
    if r:
        verdict = 'ANSWERED initialize'; break
    if rss > LIMIT_KB:
        verdict = 'RSS_OVER_3GB'; break
    if time.time() - t0 > 90:
        verdict = 'NO_ANSWER_90S'; break
el = time.time() - t0
try:
    os.killpg(p.pid, signal.SIGKILL)
except Exception:
    pass
print('value_len', n, 'verdict', verdict, 'after_s', round(el, 1), 'peak_rss_MB', peak // 1024)
sys.exit(1 if verdict in ('RSS_OVER_3GB', 'NO_ANSWER_90S') else 0)
