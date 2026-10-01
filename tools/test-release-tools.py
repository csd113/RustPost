#!/usr/bin/env python3
"""Disposable signing pipeline checks. Requires OpenSSL 3; never uses CI keys."""
import hashlib
import json
import pathlib
import subprocess
import sys
import tarfile
import tempfile
import unittest

ROOT = pathlib.Path(__file__).resolve().parent
TARGETS = ('x86_64-unknown-linux-gnu', 'aarch64-unknown-linux-gnu')

class ReleaseToolsTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.directory = pathlib.Path(self.temp.name)
        self.key = self.directory / 'ephemeral.pem'
        subprocess.run(['openssl', 'genpkey', '-algorithm', 'ED25519', '-out', str(self.key)], check=True, capture_output=True)
        for target, machine in zip(TARGETS, (62, 183)):
            binary = bytearray(128)
            binary[:7] = b'\x7fELF\x02\x01\x01'
            binary[16:18] = (2).to_bytes(2, 'little')
            binary[18:20] = machine.to_bytes(2, 'little')
            executable = self.directory / f'fixture-{target}'
            executable.write_bytes(binary)
            artifact = self.directory / f'rustpost-update-{target}.tar.gz'
            with tarfile.open(artifact, 'w:gz') as archive:
                archive.add(executable, arcname='rustpost-cli')
            metadata = dict(version='1.1.0', target=target, schema=5, minimum_schema=4, updater_protocol=1,
                            filename=artifact.name, size=artifact.stat().st_size,
                            sha256=hashlib.sha256(artifact.read_bytes()).hexdigest(), executable_size=len(binary),
                            executable_sha256=hashlib.sha256(binary).hexdigest())
            (self.directory / f'build-info-{target}.json').write_text(json.dumps(metadata))

    def tearDown(self):
        self.temp.cleanup()

    def sign(self, version='v1.1.0'):
        return subprocess.run([sys.executable, str(ROOT / 'sign-updates.py'), '--directory', str(self.directory),
                               '--key', str(self.key), '--release-id', '42', '--version', version], capture_output=True)

    def test_valid_packages_generate_verified_raw_signatures(self):
        result = self.sign()
        self.assertEqual(result.returncode, 0, result.stderr.decode())
        for target in TARGETS:
            manifest = self.directory / f'rustpost-update-{target}.json'
            content = json.loads(manifest.read_bytes())
            self.assertEqual(content['release_id'], 42)
            self.assertEqual(content['version'], '1.1.0')
            self.assertEqual(manifest.with_suffix('.json.sig').stat().st_size, 64)

    def test_checksum_mismatch_refuses_signing(self):
        artifact = self.directory / f'rustpost-update-{TARGETS[0]}.tar.gz'
        artifact.write_bytes(artifact.read_bytes() + b'tamper')
        self.assertNotEqual(self.sign().returncode, 0)

    def test_incomplete_targets_and_version_mismatch_refuse_publication(self):
        self.assertNotEqual(self.sign('v1.2.0').returncode, 0)
        (self.directory / f'build-info-{TARGETS[1]}.json').unlink()
        self.assertNotEqual(self.sign().returncode, 0)

    def test_bad_schema_or_prerelease_is_rejected(self):
        self.assertNotEqual(self.sign('v1.1.0-rc.1').returncode, 0)
        path = self.directory / f'build-info-{TARGETS[0]}.json'
        metadata = json.loads(path.read_text())
        metadata['minimum_schema'] = 6
        path.write_text(json.dumps(metadata))
        self.assertNotEqual(self.sign().returncode, 0)

if __name__ == '__main__':
    unittest.main()
