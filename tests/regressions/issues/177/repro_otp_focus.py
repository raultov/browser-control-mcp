import sys, os
sys.path.insert(0, os.path.abspath('tests/compare'))
from mcp_client import Server
from siteserver import Site

PAGE = os.path.join('tests', 'compare', 'site', 'repro_otp.html')
with open(PAGE, 'w') as f:
    f.write("<!doctype html><title>otp</title><label>d1 <input id=d1 maxlength=1></label><label>d2 <input id=d2 maxlength=1></label>"
            "<label>d3 <input id=d3 maxlength=1></label><label>d4 <input id=d4 maxlength=1></label>"
            "<script>const ds=[d1,d2,d3,d4];ds.forEach((d,i)=>d.addEventListener('input',()=>{if(d.value.length===1){if(i<3)ds[i+1].focus();else d.blur();}}));</script>")
site = Site()
s = Server(['target/release/browser-control-mcp', '--headless'], timeout=90)
try:
    s.call('navigate', {'url': site.url('/repro_otp.html')})
    fill = s.call('fill_input', {'selector': '#d1', 'text': '1234'})
    val = s.call('evaluate_js', {'expression': "[d1,d2,d3,d4].map(d=>d.value).join('')"})
    print('fill_input is_error=%s: %s' % (fill.is_error, fill.text.strip()))
    print('page holds:', val.text.strip().replace('\n', ' '))
    bug = fill.is_error and 'cannot take focus' in fill.text and '"1234"' in val.text
    print('BUG: fill_input says the field cannot take focus while the page holds 1234' if bug else 'OK')
finally:
    s.close()
    site.stop()
    os.remove(PAGE)
sys.exit(1 if bug else 0)
