#!/usr/bin/env python3
"""Sign pipette-mgmt requests as an account: RFC 9421 with Ed25519, the profile the
server verifies (docs/authentication.md section 2.4). Standard library only, Python 3.10+.

    from sign import signer_from_env, signed_request
    signer = signer_from_env()
    status, body = signed_request(signer, "https://collector.pipette.liquid.ai/api/v1/users/me")

Or print the headers for curl, one signature per request:

    curl -H @<(python3 tools/sign.py "$URL") "$URL"

PIPETTE_KEY picks the key:
    op://vault/item/private key   a 1Password item, read with the `op` CLI
    agent[:<match>]               an ssh-agent key at SSH_AUTH_SOCK (the 1Password agent
                                  too): the Ed25519 key whose SHA256 fingerprint or comment
                                  contains <match>, or the only Ed25519 key it holds
    <path>                        a private key file, PKCS#8 PEM or unencrypted OpenSSH
"""

import base64
import hashlib
import os
import secrets
import socket
import struct
import subprocess
import sys
import time
import urllib.error
import urllib.request
from dataclasses import dataclass
from typing import Callable
from urllib.parse import urlsplit

# Ed25519 signing, RFC 8032 section 5.1. Not constant-time: fine for a tool on your
# own machine, not for a service signing on behalf of others.
_P = 2**255 - 19
_Q = 2**252 + 27742317777372353535851937790883648493
_D = -121665 * pow(121666, _P - 2, _P) % _P
_GX = 15112221349535400772501151409588531511454012693041857206046113283949847762202
_GY = 46316835694926478169428394003475163141307993866256225615783033603165251855960
_G = (_GX, _GY, 1, _GX * _GY % _P)


def _add(a, b):
    x = (a[1] - a[0]) * (b[1] - b[0]) % _P
    y = (a[1] + a[0]) * (b[1] + b[0]) % _P
    c = 2 * a[3] * b[3] * _D % _P
    z = 2 * a[2] * b[2] % _P
    e, f, g, h = y - x, z - c, z + c, y + x
    return (e * f, g * h, f * g, e * h)


def _mul(s, point):
    out = (0, 1, 1, 0)
    while s:
        if s & 1:
            out = _add(out, point)
        point = _add(point, point)
        s >>= 1
    return out


def _compress(point):
    zinv = pow(point[2], _P - 2, _P)
    x, y = point[0] * zinv % _P, point[1] * zinv % _P
    return (y | (x & 1) << 255).to_bytes(32, "little")


def _expand(seed: bytes):
    h = hashlib.sha512(seed).digest()
    a = int.from_bytes(h[:32], "little") & ((1 << 254) - 8) | (1 << 254)
    return a, h[32:]


def _hash_q(data: bytes) -> int:
    return int.from_bytes(hashlib.sha512(data).digest(), "little") % _Q


def ed25519_public_key(seed: bytes) -> bytes:
    return _compress(_mul(_expand(seed)[0], _G))


def ed25519_sign(seed: bytes, message: bytes) -> bytes:
    a, prefix = _expand(seed)
    public = _compress(_mul(a, _G))
    r = _hash_q(prefix + message)
    big_r = _compress(_mul(r, _G))
    s = (r + _hash_q(big_r + public + message) * a) % _Q
    return big_r + s.to_bytes(32, "little")


@dataclass
class Signer:
    """Signs with one account key. `public_key` is the raw 32 bytes."""

    public_key: bytes
    sign: Callable[[bytes], bytes]


def key_id(signer: Signer) -> str:
    """`userkey_` + hex SHA-256 of the raw public key: the `keyid` the server reads."""
    return "userkey_" + hashlib.sha256(signer.public_key).hexdigest()


def seed_signer(seed: bytes) -> Signer:
    return Signer(ed25519_public_key(seed), lambda data: ed25519_sign(seed, data))


def text_signer(text: str) -> Signer:
    """A private key's text, PKCS#8 PEM (what `op read` gives) or unencrypted OpenSSH."""
    der = base64.b64decode(
        "".join(line for line in text.splitlines() if not line.startswith("-----"))
    )
    if "OPENSSH PRIVATE KEY" not in text:
        at = der.find(b"\x06\x03\x2b\x65\x70")  # the Ed25519 OID
        seed_at = der.find(b"\x04\x22\x04\x20", at)
        if at < 0 or seed_at < 0:
            raise ValueError("not an Ed25519 PKCS#8 key")
        return seed_signer(der[seed_at + 4 : seed_at + 36])
    r = _Reader(der)
    r.skip(len(b"openssh-key-v1\0"))
    if r.string() != b"none":
        raise ValueError('the key is passphrase-protected; make one with -N ""')
    r.string()  # kdfname
    r.string()  # kdfoptions
    r.skip(4)  # key count
    r.string()  # public key
    priv = _Reader(r.string())
    priv.skip(8)  # two check ints
    if priv.string() != b"ssh-ed25519":
        raise ValueError("not an Ed25519 key")
    priv.string()  # public key
    return seed_signer(priv.string()[:32])


def file_signer(path: str) -> Signer:
    with open(os.path.expanduser(path)) as f:
        return text_signer(f.read())


def one_password_signer(ref: str) -> Signer:
    """A 1Password SSH key item, e.g. `op://Employee/k1/private key`. Needs `op` signed in."""
    return text_signer(
        subprocess.run(
            ["op", "read", ref], check=True, capture_output=True, text=True
        ).stdout
    )


def agent_signer(match: str = "", sock: str | None = None) -> Signer:
    """An Ed25519 key held by the ssh-agent at `sock`; `match` picks by fingerprint or comment."""
    sock = sock or os.environ.get("SSH_AUTH_SOCK")
    if not sock:
        raise RuntimeError("SSH_AUTH_SOCK is not set")
    answer = _Reader(_agent_request(sock, b"\x0b"))  # REQUEST_IDENTITIES
    if answer.byte() != 12:
        raise RuntimeError("the agent refused to list its keys")
    keys = []
    for _ in range(answer.uint32()):
        blob, comment = answer.string(), answer.string().decode(errors="replace")
        fingerprint = "SHA256:" + base64.b64encode(
            hashlib.sha256(blob).digest()
        ).decode().rstrip("=")
        key = _Reader(blob)
        if key.string() == b"ssh-ed25519":
            keys.append((blob, comment, fingerprint, key.string()))
    picked = [k for k in keys if match in k[2] or match in k[1]]
    if len(picked) != 1:
        held = "; ".join(f"{k[2]} {k[1]}" for k in keys) or "none"
        raise RuntimeError(
            f'{len(picked)} Ed25519 agent keys match "{match}" (held: {held})'
        )
    blob, public_key = picked[0][0], picked[0][3]

    def sign(data: bytes) -> bytes:
        reply = _Reader(
            _agent_request(
                sock, b"\x0d" + _string(blob) + _string(data) + struct.pack(">I", 0)
            )
        )
        if reply.byte() != 14:
            raise RuntimeError("the agent refused to sign")
        sig = _Reader(reply.string())
        sig.string()  # "ssh-ed25519"
        return sig.string()

    return Signer(public_key, sign)


def signer_from_env(fallback: str | None = None) -> Signer:
    """The signer PIPETTE_KEY names (see the module docstring); `fallback` when it is unset."""
    source = os.environ.get("PIPETTE_KEY") or fallback
    if not source:
        raise RuntimeError(
            "set PIPETTE_KEY to an op:// reference, agent[:<match>], or a key file"
        )
    if source.startswith("op://"):
        return one_password_signer(source)
    if source == "agent" or source.startswith("agent:"):
        return agent_signer(source[len("agent:") :])
    return file_signer(source)


def authority(url: str) -> str:
    """The `Host` to send and sign: lowercased, a default port dropped as curl and fetch do."""
    parts = urlsplit(url)
    host = parts.netloc.rpartition("@")[2].lower()
    return host.removesuffix({"https": ":443", "http": ":80"}.get(parts.scheme, ""))


def sign_request(
    signer: Signer,
    method: str,
    url: str,
    body: bytes | None = None,
    created: int | None = None,
    nonce: str | None = None,
) -> dict[str, str]:
    """The `Signature-Input`, `Signature` and, with a body, `Content-Digest` headers."""
    parts = urlsplit(url)
    headers = {}
    components = [
        ("@method", method.upper()),
        ("@authority", authority(url)),
        ("@path", parts.path or "/"),
        ("@query", "?" + parts.query),
    ]
    if body:
        digest = base64.b64encode(hashlib.sha256(body).digest()).decode()
        headers["Content-Digest"] = f"sha-256=:{digest}:"
        components.append(("content-digest", headers["Content-Digest"]))
    created = int(time.time()) if created is None else created
    nonce = secrets.token_hex(12) if nonce is None else nonce
    names = " ".join(f'"{name}"' for name, _ in components)
    params = f'({names});created={created};nonce="{nonce}";keyid="{key_id(signer)}";alg="ed25519";tag="pipette"'
    base = (
        "".join(f'"{name}": {value}\n' for name, value in components)
        + f'"@signature-params": {params}'
    )
    headers["Signature-Input"] = f"sig={params}"
    headers["Signature"] = (
        f"sig=:{base64.b64encode(signer.sign(base.encode())).decode()}:"
    )
    return headers


def signed_request(
    signer: Signer,
    url: str,
    method: str | None = None,
    body: bytes | str | None = None,
    headers: dict[str, str] | None = None,
) -> tuple[int, bytes]:
    """One signed request: `(status, body)`, error statuses included."""
    data = body.encode() if isinstance(body, str) else body
    method = method or ("POST" if data else "GET")
    # urllib would send a default port in `Host`, unlike the signed authority.
    all_headers = {"Host": authority(url)}
    if data:
        all_headers["Content-Type"] = "application/json"
    all_headers |= (headers or {}) | sign_request(signer, method, url, data)
    request = urllib.request.Request(url, data=data, method=method, headers=all_headers)
    try:
        with urllib.request.urlopen(request) as res:
            return res.status, res.read()
    except urllib.error.HTTPError as e:
        return e.code, e.read()


class _Reader:
    def __init__(self, buf: bytes):
        self.buf, self.at = buf, 0

    def skip(self, n: int):
        self.at += n

    def byte(self) -> int:
        self.at += 1
        return self.buf[self.at - 1]

    def uint32(self) -> int:
        self.at += 4
        return struct.unpack(">I", self.buf[self.at - 4 : self.at])[0]

    def string(self) -> bytes:
        n = self.uint32()
        self.at += n
        return self.buf[self.at - n : self.at]


def _string(b: bytes) -> bytes:
    return struct.pack(">I", len(b)) + b


def _agent_request(sock: str, message: bytes) -> bytes:
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
        s.connect(sock)
        s.sendall(_string(message))
        got = b""
        while len(got) < 4 or len(got) < 4 + struct.unpack(">I", got[:4])[0]:
            chunk = s.recv(65536)
            if not chunk:
                raise RuntimeError("the agent closed the connection")
            got += chunk
        return got[4 : 4 + struct.unpack(">I", got[:4])[0]]


if __name__ == "__main__":
    try:
        if len(sys.argv) < 2:
            raise SystemExit("usage: sign.py <url> [method] [body-file]")
        url, method = sys.argv[1], sys.argv[2] if len(sys.argv) > 2 else "GET"
        body = open(sys.argv[3], "rb").read() if len(sys.argv) > 3 else None
        for name, value in sign_request(signer_from_env(), method, url, body).items():
            print(f"{name}: {value}")
    except (OSError, ValueError, RuntimeError, subprocess.CalledProcessError) as e:
        sys.exit(str(e))
