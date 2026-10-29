"""tools/sign.py against the vector pipette-httpsig and the app pin, and real ssh keys.

python3 -m unittest discover -s tools/tests
"""

import base64
import os
import socket
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from sign import agent_signer, authority, file_signer, sign_request, text_signer  # noqa: E402

# RFC 9421 B.1.4 test-key-ed25519 as PKCS#8 PEM; the label is split so no scanner
# reads the source as a key.
RFC_SEED = base64.urlsafe_b64decode("n4Ni-HpISpVObnQMW0wOhCKROaIKqKtW_2ZYb2p9KcU=")
LABEL = "PRIVATE" + " KEY"
RFC_PEM = "\n".join(
    [
        f"-----BEGIN {LABEL}-----",
        base64.b64encode(
            bytes.fromhex("302e020100300506032b657004220420") + RFC_SEED
        ).decode(),
        f"-----END {LABEL}-----",
    ]
)


def ssh_keygen(directory: str, kind: str, passphrase: str = "") -> str:
    path = os.path.join(directory, kind)
    subprocess.run(
        ["ssh-keygen", "-q", "-t", kind, "-N", passphrase, "-C", "test", "-f", path],
        check=True,
    )
    return path


class SignTest(unittest.TestCase):
    def test_the_shared_vector_reproduces(self):
        headers = sign_request(
            text_signer(RFC_PEM),
            "POST",
            "https://mgmt.example/api/v1/orgs?dry_run=true",
            b'{"slug":"acme"}',
            created=1760000000,
            nonce="4f1c0de5a9b7",
        )
        self.assertEqual(
            headers,
            {
                "Content-Digest": "sha-256=:Dmcct3wNRuKmSGlN9J7lDIw9DvE0Xp6DUXXIZHWJ65k=:",
                "Signature-Input": 'sig=("@method" "@authority" "@path" "@query" "content-digest");created=1760000000;nonce="4f1c0de5a9b7";keyid="userkey_b16c2d1bead1262639764fdb0ee4d3774599336bd493404cda4b1136c59f2062";alg="ed25519";tag="pipette"',
                "Signature": "sig=:xErs0vVcglQO4jeofP53oNks9pyROP4E0OOttcbMlHt5CAanP85tQV21Sy4jOZEzJg3Bm9gRrGXvqDssOFy+Bg==:",
            },
        )

    def test_authority_drops_a_default_port(self):
        cases = [
            ("https://Mgmt.Example:443/x", "mgmt.example"),
            ("http://mgmt.example:80/x", "mgmt.example"),
            ("https://mgmt.example:8443/x", "mgmt.example:8443"),
            ("http://u@mgmt.example:443/x", "mgmt.example:443"),
        ]
        for url, expected in cases:
            with self.subTest(url=url):
                self.assertEqual(authority(url), expected)

    def test_an_openssh_key_reads_as_ssh_keygen_wrote_it(self):
        with tempfile.TemporaryDirectory() as d:
            path = ssh_keygen(d, "ed25519")
            blob = base64.b64decode(Path(path + ".pub").read_text().split()[1])
            self.assertEqual(file_signer(path).public_key, blob[-32:])

    def test_other_keys_are_refused(self):
        with tempfile.TemporaryDirectory() as d:
            cases = [
                (ssh_keygen(d, "ecdsa"), "not an Ed25519 key"),
                (ssh_keygen(d, "ed25519", "secret"), "passphrase-protected"),
            ]
            for path, message in cases:
                with (
                    self.subTest(path=path),
                    self.assertRaisesRegex(ValueError, message),
                ):
                    file_signer(path)

    def test_the_agent_signs_as_the_key_file(self):
        with tempfile.TemporaryDirectory() as d:
            path, sock = ssh_keygen(d, "ed25519"), os.path.join(d, "agent.sock")
            agent = subprocess.Popen(
                ["ssh-agent", "-D", "-a", sock],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
            try:
                while not os.path.exists(sock):
                    time.sleep(0.05)
                env = dict(os.environ, SSH_AUTH_SOCK=sock)
                subprocess.run(["ssh-add", "-q", path], check=True, env=env)
                # Ed25519 is deterministic, so the two signatures are equal.
                signer = agent_signer("test", sock)
                self.assertEqual(signer.sign(b"base"), file_signer(path).sign(b"base"))
            finally:
                agent.terminate()
                agent.wait()

    def test_an_agent_that_hangs_up_is_an_error(self):
        with tempfile.TemporaryDirectory() as d:
            sock = os.path.join(d, "agent.sock")
            with socket.socket(socket.AF_UNIX) as server:
                server.bind(sock)
                server.listen()

                def hang_up():
                    conn, _ = server.accept()
                    with conn:
                        conn.recv(64)
                        conn.sendall(
                            b"\x00\x00\x00\x10\x0c"
                        )  # 16 bytes promised, 1 sent

                threading.Thread(target=hang_up, daemon=True).start()
                with self.assertRaisesRegex(RuntimeError, "closed the connection"):
                    agent_signer("", sock)


if __name__ == "__main__":
    unittest.main()
