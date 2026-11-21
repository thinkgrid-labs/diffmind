"""Speak diffmind's daemon handshake to check authentication end to end.

Usage: python3 -I daemon_probe.py <path to daemon.json>
Prints one line per check and exits 1 if any check fails. Never prints the token.
"""
import hashlib, hmac, json, os, socket, sys

DOMAIN = b"diffmind-daemon-server-proof-v1"
info = json.load(open(sys.argv[1], encoding="utf-8"))
port, token = info["port"], info["token"]
failed = []


def exchange(tok, request):
    s = socket.create_connection(("127.0.0.1", port), timeout=30)
    f = s.makefile("rwb")
    nonce = os.urandom(32)
    f.write(json.dumps({"hello": nonce.hex()}).encode() + b"\n"); f.flush()
    proof = json.loads(f.readline())["proof"]
    expected = hmac.new(bytes.fromhex(token), DOMAIN + nonce, hashlib.sha256).hexdigest()
    proof_ok = hmac.compare_digest(proof, expected)
    env = dict(request); env["token"] = tok
    f.write(json.dumps(env).encode() + b"\n"); f.flush()
    reply = f.readline().decode()
    s.close()
    return proof_ok, reply


def check(name, cond, detail=""):
    print(("PASS " if cond else "FAIL ") + name + (f"  [{detail}]" if detail else ""))
    if not cond:
        failed.append(name)


ok, reply = exchange(token, {"op": "ping"})
check("server proof verifies with the real token", ok)
check("ping with the real token is answered", "error" not in reply.lower(), reply.strip()[:120])

bad = "0" * 64 if token != "0" * 64 else "1" * 64
ok, reply = exchange(bad, {"op": "ping"})
check("ping with a wrong token is refused", "invalid token" in reply, reply.strip()[:120])

ok, reply = exchange(bad, {"op": "shutdown"})
check("shutdown with a wrong token is refused", "invalid token" in reply, reply.strip()[:120])
ok, reply = exchange(token, {"op": "ping"})
check("daemon still alive after the refused shutdown", "error" not in reply.lower())

sys.exit(1 if failed else 0)
