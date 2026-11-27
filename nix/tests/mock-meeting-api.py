"""Mock meeting-api for the videocall-cli device enrollment NixOS test.

Speaks only what `videocall-cli auth` calls, with the shapes the CLI's own
tests mock (videocall-cli/src/auth/tests.rs, videocall-meeting-types::device):

  GET    /version              Date header only (the CLI reads the server clock)
  GET    /cli/authorize        the browser hop; records the query
  POST   /api/v1/devices       code exchange, checked like the real server
  GET    /api/v1/devices/me    device JWT -> 200 while live, 401 once revoked
  DELETE /api/v1/devices/me    device JWT -> 204, device revoked

Test controls (the human and the test script, not the CLI), under /_mock/:

  POST /_mock/approve               the human clicks Approve: returns {"code"}
  POST /_mock/reject-next-exchange  next POST /api/v1/devices answers invalid_grant
  POST /_mock/revoke-all            the human revokes every device in the browser
  GET  /_mock/state                 what the mock saw

Usage: mock-meeting-api.py PORT   (listens on 127.0.0.1; its origin is the
audience of every token). Needs python3 and the `cryptography` package.
"""

import base64
import hashlib
import json
import secrets
import sys
import time
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlsplit

from cryptography.exceptions import InvalidSignature
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PublicKey

PORT = int(sys.argv[1])
ORIGIN = f"http://127.0.0.1:{PORT}"
REMOTE_REDIRECT_URI = "urn:ietf:wg:oauth:2.0:oob"
CLI_ISSUER = "videocall-cli"
LEEWAY = 60

LOCK = __import__("threading").Lock()
STATE = {
    "authorizations": [],  # recorded GET /cli/authorize queries
    "codes": {},  # code -> authorization (single use)
    "devices": {},  # device_id -> {name, public_key, revoked}
    "deletes": 0,  # accepted DELETE /me
    "exchanges": 0,  # POST /api/v1/devices that succeeded
    "reject_next_exchange": False,
}


def b64url_decode(text):
    return base64.urlsafe_b64decode(text + "=" * (-len(text) % 4))


def b64url(raw):
    return base64.urlsafe_b64encode(raw).rstrip(b"=").decode()


def fingerprint(raw_key):
    """First 8 bytes of SHA-256(raw key), upper-case hex in four groups."""
    hexed = hashlib.sha256(raw_key).hexdigest()[:16].upper()
    return "-".join(hexed[i : i + 4] for i in range(0, 16, 4))


def verify_jwt(token, public_key_b64):
    """Check an EdDSA JWT's signature, issuer, audience and times.

    Returns (header, claims) or None.
    """
    try:
        head_b64, claims_b64, sig_b64 = token.split(".")
        header = json.loads(b64url_decode(head_b64))
        claims = json.loads(b64url_decode(claims_b64))
        key = Ed25519PublicKey.from_public_bytes(b64url_decode(public_key_b64))
        key.verify(b64url_decode(sig_b64), f"{head_b64}.{claims_b64}".encode())
    except (ValueError, KeyError, InvalidSignature):
        return None
    now = time.time()
    if header.get("alg") != "EdDSA":
        return None
    if claims.get("iss") != CLI_ISSUER or claims.get("aud") != ORIGIN:
        return None
    if not (claims["iat"] - LEEWAY <= now <= claims["exp"] + LEEWAY):
        return None
    return header, claims


class Handler(BaseHTTPRequestHandler):
    # BaseHTTPRequestHandler stamps a Date header on every response, like the
    # real server; the CLI uses it to detect a skewed clock.
    server_version = "mock-meeting-api"

    def log_message(self, fmt, *args):
        print(f"{self.command} {self.path} -> {fmt % args}", flush=True)

    def reply(self, status, body=None, content_type="application/json"):
        data = b"" if body is None else (body if isinstance(body, bytes) else json.dumps(body).encode())
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(data)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(data)

    def device_error(self, status, error):
        self.reply(status, {"error": error, "error_description": f"mock: {error}"})

    def body_json(self):
        length = int(self.headers.get("Content-Length") or 0)
        return json.loads(self.rfile.read(length) or b"{}")

    def calling_device(self):
        """The live device behind the bearer JWT, or None (-> 401)."""
        auth = self.headers.get("Authorization", "")
        if not auth.startswith("Bearer "):
            return None
        token = auth[len("Bearer ") :]
        try:
            kid = json.loads(b64url_decode(token.split(".")[0])).get("kid")
        except (ValueError, IndexError):
            return None
        device = STATE["devices"].get(kid)
        if device is None or device["revoked"]:
            return None
        checked = verify_jwt(token, device["public_key"])
        if checked is None or checked[1].get("sub") != kid:
            return None
        return kid, device

    def do_GET(self):
        url = urlsplit(self.path)
        with LOCK:
            if url.path == "/version":
                return self.reply(200, {"service": "mock-meeting-api"})
            if url.path == "/_mock/state":
                return self.reply(200, STATE | {"codes": len(STATE["codes"])})
            if url.path == "/cli/authorize":
                query = {k: v[0] for k, v in parse_qs(url.query).items()}
                valid = (
                    query.get("response_type") == "code"
                    and query.get("client_id") == "videocall-cli"
                    and query.get("code_challenge_method") == "S256"
                    and query.get("redirect_uri") == REMOTE_REDIRECT_URI
                    and len(query.get("public_key", "")) == 43
                    and query.get("code_challenge")
                    and query.get("device_name")
                )
                if not valid:
                    return self.reply(400, {"error": "invalid_request"})
                STATE["authorizations"].append(query)
                return self.reply(200, b"consent page", "text/plain")
            if url.path == "/api/v1/devices/me":
                found = self.calling_device()
                if found is None:
                    return self.reply(401, {})
                kid, device = found
                now = int(time.time())
                return self.reply(200, {
                    "device_id": kid, "name": device["name"],
                    "fingerprint": device["fingerprint"],
                    "user_id": "alice@example.test", "user_name": "Alice",
                    "created_at": now, "last_used_at": now,
                    "idle_expires_at": now + 90 * 86400, "abs_expires_at": now + 365 * 86400,
                })
        self.reply(404, {})

    def do_DELETE(self):
        with LOCK:
            if urlsplit(self.path).path == "/api/v1/devices/me":
                found = self.calling_device()
                if found is None:
                    return self.reply(401, {})
                found[1]["revoked"] = True
                STATE["deletes"] += 1
                return self.reply(204)
        self.reply(404, {})

    def do_POST(self):
        path = urlsplit(self.path).path
        with LOCK:
            if path == "/_mock/approve":
                if not STATE["authorizations"]:
                    return self.reply(409, {"error": "nothing to approve"})
                code = secrets.token_urlsafe(32)
                STATE["codes"][code] = STATE["authorizations"][-1]
                return self.reply(200, {"code": code})
            if path == "/_mock/reject-next-exchange":
                STATE["reject_next_exchange"] = True
                return self.reply(200, {})
            if path == "/_mock/revoke-all":
                for device in STATE["devices"].values():
                    device["revoked"] = True
                return self.reply(200, {})
            if path == "/api/v1/devices":
                return self.exchange()
        self.reply(404, {})

    def exchange(self):
        """POST /api/v1/devices: the code is burned by any attempt."""
        try:
            body = self.body_json()
            code, verifier = body["code"], body["code_verifier"]
            redirect_uri, registration = body["redirect_uri"], body["registration"]
        except (ValueError, KeyError):
            return self.device_error(400, "invalid_request")
        authorization = STATE["codes"].pop(code, None)
        if STATE["reject_next_exchange"]:
            STATE["reject_next_exchange"] = False
            return self.device_error(400, "invalid_grant")
        if authorization is None or redirect_uri != authorization["redirect_uri"]:
            return self.device_error(400, "invalid_grant")
        challenge = b64url(hashlib.sha256(verifier.encode()).digest())
        if not secrets.compare_digest(challenge, authorization["code_challenge"]):
            return self.device_error(400, "invalid_grant")
        # The registration JWS must be signed by the key the human approved.
        checked = verify_jwt(registration, authorization["public_key"])
        if checked is None:
            return self.device_error(400, "invalid_grant")
        claims = checked[1]
        if claims.get("public_key") != authorization["public_key"] or claims.get("name") != authorization["device_name"]:
            return self.device_error(400, "invalid_grant")
        device_id = str(uuid.uuid4())
        fp = fingerprint(b64url_decode(authorization["public_key"]))
        STATE["devices"][device_id] = {
            "name": claims["name"], "public_key": authorization["public_key"],
            "fingerprint": fp, "revoked": False,
        }
        STATE["exchanges"] += 1
        now = int(time.time())
        self.reply(201, {
            "device_id": device_id, "fingerprint": fp, "name": claims["name"],
            "user_id": "alice@example.test", "user_name": "Alice",
            "idle_expires_at": now + 90 * 86400, "abs_expires_at": now + 365 * 86400,
        })


if __name__ == "__main__":
    ThreadingHTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
