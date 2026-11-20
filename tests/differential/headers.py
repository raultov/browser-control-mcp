"""Header redaction in get_network_logs, end to end on one browser: a page
sends every credential header a page may set and some standard ones, and
the server answers with credential and standard response headers. By
default every credential value is hidden and every standard one shown, in
the details of every request and of one (`request_id`); with
`include_sensitive_headers` every value is shown.

Usage (from the repository root):
    python3 -I tests/differential/headers.py [--browser chrome|firefox]
        [--bin target/debug/browser-control-mcp]
Exits 1 on a value shown that should be hidden, or the other way round.
"""

import argparse
import base64
import json
import os
import re
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(HERE))
sys.path.insert(0, os.path.join(ROOT, "tests", "compare"))

from mcp_client import Server  # noqa: E402
from siteserver import Site  # noqa: E402

# Credential request headers a page may set (not Cookie, Proxy-* or Sec-*,
# which fetch refuses; the cookie is set by document.cookie instead).
REQUEST_HIDDEN = [
    "Authorization", "X-Api-Key", "X-API_KEY", "x-goog-api-key", "X-CSRF-Token", "X-XSRF-TOKEN",
    "X-Session-Id", "X-Auth-Key", "Ocp-Apim-Subscription-Key", "X-Amz-Security-Token", "X-Authorization",
    "X-Forwarded-Authorization", "Authentication", "DPoP", "X-Appwrite-JWT", "X-Firebase-AppCheck",
    "X-GitHub-OTP", "X-Vercel-Protection-Bypass", "X-Honeycomb-Team", "X-WP-Nonce", "X-JFrog-Art-Api",
    "CB-ACCESS-SIGN", "X-BAPI-SIGN", "X-Ovh-Application", "X-WSSE", "X-SID", "X-Bearer", "PRIVATE-TOKEN",
    "Access-Control-Token", "X-RateLimit-Token", "X-Build", "Idempotency-Key", "From", "Baggage",
]
REQUEST_SHOWN = ["Accept", "Content-Language", "X-Request-Id", "traceparent", "X-Requested-With"]
RESPONSE_HIDDEN = ["Set-Cookie", "X-Auth-Token", "X-Session", "X-Shopify-Access-Token", "Access-Control-Secret"]
RESPONSE_SHOWN = ["ETag", "Access-Control-Allow-Origin", "X-Request-Id", "Server-Timing", "Cross-Origin-Opener-Policy",
                  "X-RateLimit-Remaining", "Content-Security-Policy", "Referrer-Policy"]


def value(name):
    """A value no other header has, to find it in the reply."""
    return "v%s-%d" % (re.sub(r"[^a-z0-9]", "", name.lower())[:12], abs(hash(name)) % 9973)


def check(text, hidden, shown, show_all):
    """The headers whose value is wrong in `text`."""
    wrong = []
    for name in hidden:
        if (value(name) in text) != show_all:
            wrong.append("%s %s" % (name, "hidden" if show_all else "shown"))
    for name in shown:
        if value(name) not in text:
            wrong.append("%s hidden" % name)
    return wrong


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--browser", default="chrome", choices=["chrome", "firefox"])
    parser.add_argument("--bin", default=os.path.join(ROOT, "target", "debug", "browser-control-mcp"))
    parser.add_argument("--visible", action="store_true")
    args = parser.parse_args()

    command = [args.bin, "--browser", args.browser] + ([] if args.visible else ["--headless"])
    site = Site()
    server = Server(command, timeout=120)
    wrong = []
    try:
        server.call("navigate", {"url": site.url("/basic.html")})
        sent = {name: value(name) for name in REQUEST_HIDDEN + REQUEST_SHOWN}
        answered = "&".join("h=" + base64.urlsafe_b64encode(("%s: %s" % (n, value(n))).encode()).decode()
                            for n in RESPONSE_HIDDEN + RESPONSE_SHOWN)
        script = ("document.cookie = 'qa_cookie=%s; path=/'; await fetch('/headers?hdr=1&%s', {headers: %s}).then(r => r.status)"
                  % (value("Cookie"), answered, json.dumps(sent)))
        status = server.call("evaluate_js", {"expression": script}).text
        if "200" not in status:
            raise RuntimeError("the request failed: " + status.strip()[:200])
        time.sleep(1)
        hidden = REQUEST_HIDDEN + RESPONSE_HIDDEN + ["Cookie"]
        shown = REQUEST_SHOWN + RESPONSE_SHOWN
        details = server.call("get_network_logs", {"url_filter": "hdr=1", "include_details": True}).text
        wrong += ["details: " + w for w in check(details, hidden, shown, False)]
        request_id = re.search(r'"(\S+)":\s*\{[^{}]*"url":\s*"[^"]*hdr=1', details) or re.search(r"^(\S+) GET \S*hdr=1", server.call("get_network_logs", {"url_filter": "hdr=1"}).text, re.M)
        if request_id:
            one = server.call("get_network_logs", {"request_id": request_id.group(1)}).text
            wrong += ["request_id: " + w for w in check(one, hidden, shown, False)]
        else:
            wrong.append("request_id: the request is not listed")
        every = server.call("get_network_logs", {"url_filter": "hdr=1", "include_details": True,
                                                 "include_sensitive_headers": True}).text
        wrong += ["include_sensitive_headers: " + w for w in check(every, hidden, shown, True)]
    finally:
        server.close()
        site.stop()
    for line in wrong:
        print("  " + line)
    print("header redaction (%s): %d header(s) checked, %d wrong" % (args.browser, len(REQUEST_HIDDEN + REQUEST_SHOWN + RESPONSE_HIDDEN + RESPONSE_SHOWN) + 1, len(wrong)))
    return 1 if wrong else 0


if __name__ == "__main__":
    sys.exit(main())
