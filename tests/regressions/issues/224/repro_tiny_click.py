"""click_element on a visible, uncovered 1x1 px button: reported covered by body."""
import json, sys
sys.path.insert(0, "tests/compare")
from mcp_client import Server

PAGE = ("data:text/html,<body><button id=b style='width:1px;height:1px;padding:0;border:0' "
        "onclick='window.hit=1'></button>")
s = Server(["target/release/browser-control-mcp", "--headless"])
try:
    s.call("navigate", {"url": PAGE})
    rect = json.loads(s.call("evaluate_js", {"expression": "JSON.stringify(b.getBoundingClientRect())"}).text)["value"]
    top = json.loads(s.call("evaluate_js", {"expression":
        "const r=b.getBoundingClientRect(); document.elementFromPoint(r.x+r.width/2, r.y+r.height/2)===b"}).text)["value"]
    r = s.call("click_element", {"selector": "#b"})
finally:
    s.close()
print("rect:", rect, "button is topmost at its centre:", top)
print("click_element:", r.text.strip())
if top and r.is_error and "covered" in r.text:
    print("BUG: uncovered button reported covered")
    sys.exit(1)
print("OK")
