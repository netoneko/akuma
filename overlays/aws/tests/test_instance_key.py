"""Exercise the actual bootstrap script with an injectable IMDS endpoint."""
import http.server
import os
from pathlib import Path
import subprocess
import tempfile
import threading
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / 'rootfs/sbin/instance-key'
KEY = 'ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGFiY2RlZmdoaWprbG1ub3BxcnN0dXZ3eHl6MDEyMzQ1 test'


class Metadata(http.server.BaseHTTPRequestHandler):
    key = KEY
    authenticated = 0

    def log_message(self, *args):
        pass

    def do_PUT(self):
        self.send_response(200)
        self.end_headers()
        self.wfile.write(b'test-token')

    def do_GET(self):
        if self.path != '/latest/meta-data/public-keys/0/openssh-key' or self.headers.get('X-aws-ec2-metadata-token') != 'test-token':
            self.send_error(403)
            return
        type(self).authenticated += 1
        self.send_response(200)
        self.end_headers()
        self.wfile.write(type(self).key.encode())


class BootstrapTest(unittest.TestCase):
    def setUp(self):
        Metadata.key = KEY
        Metadata.authenticated = 0
        self.server = http.server.HTTPServer(('127.0.0.1', 0), Metadata)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.temp = tempfile.TemporaryDirectory()
        self.keys = Path(self.temp.name) / 'authorized_keys'

    def tearDown(self):
        self.server.shutdown()
        self.thread.join()
        self.server.server_close()
        self.temp.cleanup()

    def run_bootstrap(self):
        env = dict(os.environ, METADATA_URL=f'http://127.0.0.1:{self.server.server_port}',
                   AUTHORIZED_KEYS=str(self.keys), INSTANCE_KEY_TRIES='1')
        return subprocess.run(['/bin/sh', str(SCRIPT)], env=env, capture_output=True, text=True)

    def test_imdsv2_installs_key_idempotently(self):
        self.assertEqual(self.run_bootstrap().returncode, 0)
        self.assertEqual(self.keys.read_text(), KEY + '\n')
        self.assertEqual(self.keys.stat().st_mode & 0o777, 0o600)
        self.assertEqual(self.run_bootstrap().returncode, 0)
        self.assertEqual(self.keys.read_text(), KEY + '\n')
        self.assertEqual(Metadata.authenticated, 2)

    def test_invalid_key_preserves_existing_file(self):
        self.keys.write_text('existing\n')
        Metadata.key = 'ssh-rsa AAAA wrong'
        self.assertNotEqual(self.run_bootstrap().returncode, 0)
        self.assertEqual(self.keys.read_text(), 'existing\n')

    def test_existing_partial_blob_is_not_a_duplicate(self):
        self.keys.write_text('comment ' + KEY.split()[1] + 'suffix\n')
        self.assertEqual(self.run_bootstrap().returncode, 0)
        self.assertTrue(self.keys.read_text().endswith(KEY + '\n'))


if __name__ == '__main__':
    unittest.main()
