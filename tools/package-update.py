#!/usr/bin/env python3
"""Build an updater archive from a target's executable; no runtime mutation."""
import argparse
import hashlib
import json
import pathlib
import subprocess
import tarfile

parser = argparse.ArgumentParser()
parser.add_argument('--binary', type=pathlib.Path, required=True)
parser.add_argument('--target', required=True)
parser.add_argument('--version', required=True)
parser.add_argument('--output', type=pathlib.Path, required=True)
args = parser.parse_args()
info = json.loads(subprocess.check_output([str(args.binary.resolve()), '--update-info'], text=True))
assert info['version'] == args.version.removeprefix('v'), 'executable version differs from tag'
assert info['target'] == args.target and info['updater_protocol'] == 1, 'executable target/protocol mismatch'
assert args.target in ('x86_64-unknown-linux-gnu', 'aarch64-unknown-linux-gnu')
args.output.mkdir(parents=True, exist_ok=True)
name = f'rustpost-update-{args.target}.tar.gz'
with tarfile.open(args.output / name, 'w:gz') as archive:
    entry = archive.gettarinfo(str(args.binary), arcname='rustpost-cli')
    entry.uid = entry.gid = 0
    entry.uname = entry.gname = ''
    entry.mode = 0o755
    entry.mtime = 0
    with args.binary.open('rb') as binary:
        archive.addfile(entry, binary)
metadata = dict(info, filename=name, size=(args.output/name).stat().st_size,
                sha256=hashlib.sha256((args.output/name).read_bytes()).hexdigest(),
                executable_size=args.binary.stat().st_size,
                executable_sha256=hashlib.sha256(args.binary.read_bytes()).hexdigest())
(args.output / f'build-info-{args.target}.json').write_text(json.dumps(metadata, sort_keys=True))
