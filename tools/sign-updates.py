#!/usr/bin/env python3
"""Validate both Linux packages and sign exact manifest bytes using a CI key."""
import argparse
import hashlib
import io
import json
import pathlib
import re
import subprocess
import tarfile

parser = argparse.ArgumentParser()
parser.add_argument('--directory', type=pathlib.Path, required=True)
parser.add_argument('--key', type=pathlib.Path, required=True)
parser.add_argument('--release-id', type=int, required=True)
parser.add_argument('--version', required=True)
args = parser.parse_args()
version = args.version.removeprefix('v')
assert re.fullmatch(r'(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)', version), 'stable release required'
assert args.release_id > 0
public_key = args.directory / 'release-public-key.pem'
subprocess.run(['openssl', 'pkey', '-in', str(args.key), '-pubout', '-out', str(public_key)], check=True)
for target in ('x86_64-unknown-linux-gnu', 'aarch64-unknown-linux-gnu'):
    metadata = json.loads((args.directory / f'build-info-{target}.json').read_text())
    assert metadata['version'] == version and metadata['target'] == target
    assert metadata['updater_protocol'] == 1
    assert isinstance(metadata['schema'], int) and isinstance(metadata['minimum_schema'], int)
    assert 0 < metadata['minimum_schema'] <= metadata['schema']
    assert 0 < metadata['size'] <= 256 * 1024 * 1024
    assert 0 < metadata['executable_size'] <= 256 * 1024 * 1024
    artifact = args.directory / f'rustpost-update-{target}.tar.gz'
    content = artifact.read_bytes()
    assert metadata['filename'] == artifact.name and metadata['size'] == len(content)
    assert metadata['sha256'] == hashlib.sha256(content).hexdigest()
    with tarfile.open(fileobj=io.BytesIO(content), mode='r:gz') as archive:
        entries = archive.getmembers()
        assert len(entries) == 1 and entries[0].name == 'rustpost-cli' and entries[0].isfile()
        executable = archive.extractfile(entries[0]).read()
    assert metadata['executable_size'] == len(executable)
    assert metadata['executable_sha256'] == hashlib.sha256(executable).hexdigest()
    machine = 62 if target.startswith('x86_64') else 183
    assert executable[:7] == b'\x7fELF\x02\x01\x01' and int.from_bytes(executable[18:20], 'little') == machine
    manifest = {key: metadata[key] for key in ('version', 'target', 'filename', 'size', 'sha256', 'executable_size', 'executable_sha256', 'schema', 'minimum_schema')}
    manifest.update(format=1, release_id=args.release_id, minimum_updater='1.0.0')
    path = args.directory / f'rustpost-update-{target}.json'
    path.write_bytes(json.dumps(manifest, sort_keys=True, separators=(',', ':')).encode())
    signature = path.with_suffix('.json.sig')
    subprocess.run(['openssl', 'pkeyutl', '-sign', '-rawin', '-inkey', str(args.key), '-in', str(path), '-out', str(signature)], check=True)
    assert signature.stat().st_size == 64, 'signing key must be Ed25519'
    subprocess.run(['openssl', 'pkeyutl', '-verify', '-rawin', '-pubin', '-inkey', str(public_key), '-in', str(path), '-sigfile', str(signature)], check=True)
