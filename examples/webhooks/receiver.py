#!/usr/bin/env python3
"""A webhook receiver for a nuthatch nest, in the standard library and nothing else.

Run it, point a `[[webhooks]]` or `[[alerts]]` sink at it, and it prints what arrives
and whether the signature checks out:

    python3 receiver.py --secret hunter2
    python3 receiver.py --secret hunter2 --port 9000

The whole point of this file is the verification below. Everything else is a print
statement. If you take one thing from it, take `verify()`.
"""

import argparse
import hashlib
import hmac
import json
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer

SECRET = None
STRICT = True


def verify(secret: str, body: bytes, header: str | None) -> bool:
    """Is this POST really from the nest that holds `secret`?

    nuthatch signs the **exact bytes it sends**, not a re-serialisation of them, and
    sends the result as `X-Nuthatch-Signature: sha256=<hex>`. So verify against the raw
    body you received. If you parse the JSON first and re-encode it to check the
    signature, you will get a different byte string and the check will fail for reasons
    that look like a bug in nuthatch and are not.

    It is plain HMAC-SHA256 (RFC 2104), so any language's standard library agrees with
    it. `compare_digest` rather than `==` keeps the comparison constant-time.
    """
    if header is None:
        return False
    scheme, _, sent = header.partition("=")
    if scheme != "sha256" or not sent:
        return False
    expected = hmac.new(secret.encode(), body, hashlib.sha256).hexdigest()
    return hmac.compare_digest(expected, sent)


class Handler(BaseHTTPRequestHandler):
    def do_POST(self) -> None:
        body = self.rfile.read(int(self.headers.get("Content-Length", 0)))
        sig = self.headers.get("X-Nuthatch-Signature")

        if SECRET:
            ok = verify(SECRET, body, sig)
            print(f"signature: {'OK' if ok else 'BAD'}  ({sig})")
            if not ok and STRICT:
                # Reject loudly. A non-2xx leaves the delivery in nuthatch's outbox and
                # it will retry, which is what you want: an unverified payload should
                # not be silently accepted *or* silently dropped.
                self.send_response(401)
                self.end_headers()
                return
        elif sig:
            print(f"signature present but no --secret given, not verifying: {sig}")

        try:
            payload = json.loads(body)
            print(json.dumps(payload, indent=2)[:2000])
        except json.JSONDecodeError:
            print(f"non-JSON body ({len(body)} bytes): {body[:200]!r}")

        # 2xx is the ack. nuthatch removes the entry from its outbox on any 2xx and
        # retries on anything else, so only return this once you have durably handled
        # the payload - at-least-once delivery means you may see it twice.
        self.send_response(200)
        self.end_headers()
        self.wfile.write(b"ok")

    def log_message(self, *_args) -> None:
        pass  # the payload print above is the log


def main() -> int:
    global SECRET, STRICT
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--port", type=int, default=8099)
    ap.add_argument("--secret", help="the same string as `secret` in the webhook config")
    ap.add_argument(
        "--accept-unsigned",
        action="store_true",
        help="return 200 even when verification fails (for debugging only)",
    )
    args = ap.parse_args()
    SECRET, STRICT = args.secret, not args.accept_unsigned

    if not SECRET:
        print("no --secret: signatures will be printed but not checked\n", file=sys.stderr)
    print(f"listening on http://127.0.0.1:{args.port}/  (ctrl-c to stop)\n")
    try:
        HTTPServer(("127.0.0.1", args.port), Handler).serve_forever()
    except KeyboardInterrupt:
        return 0
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
