#!/usr/bin/env python3
"""Attach a locally built installer to a GitLab release so the in-app updater can find it.

  GITLAB_TOKEN=<personal access token with api scope> \\
    packaging/gitlab/upload-installer.py v0.2.0 target/macos/SeeMyMac-0.2.0-macOS.zip \\
    target/macos/SeeMyMac-0.2.0-macOS.dmg target/installers/SeeMyMac-0.2.0-setup.exe

Each file is uploaded to the project's generic package registry and linked from the release
(replacing a link of the same name). The release must already exist; the tag pipeline creates it
(as an upcoming release) and the manual "publish-release" job publishes it. Upload the Windows
installer and the macOS zip before publishing: src/update.rs picks them by file name:
  SeeMyMac-<version>-setup.exe  (Windows; one installer for ARM64 and x86 PCs)
  SeeMyMac-<version>-macOS.zip  (macOS, signed and notarized)
"""
import json
import os
import sys
from pathlib import Path
from urllib.parse import quote
from urllib.request import Request, urlopen

PROJECT = 'kestermckinney%2FSeeMyMac'
API = f'https://gitlab.com/api/v4/projects/{PROJECT}'


def call(path, method='GET', data=None, content_type='application/json'):
    req = Request(API + path, data=data, method=method,
                  headers={'PRIVATE-TOKEN': os.environ['GITLAB_TOKEN'], 'Content-Type': content_type})
    with urlopen(req, timeout=600) as response:
        body = response.read()
        return json.loads(body) if body else None


if len(sys.argv) < 3 or 'GITLAB_TOKEN' not in os.environ:
    raise SystemExit(__doc__)
tag, files = sys.argv[1], [Path(f) for f in sys.argv[2:]]
if not tag.startswith('v'):
    raise SystemExit('The tag looks like v0.2.0')
release_path = '/releases/' + quote(tag, safe='')
links = call(release_path)['assets']['links']  # fails if the release does not exist yet
for path in files:
    name = path.name
    endpoint = f'/packages/generic/seemymac/{quote(tag, safe="")}/{quote(name)}'
    call(endpoint, 'PUT', path.read_bytes(), 'application/octet-stream')
    link = {'name': name, 'url': API + endpoint, 'direct_asset_path': '/' + name, 'link_type': 'package'}
    match = next((l for l in links if l['name'] == name), None)
    if match:
        call(f'{release_path}/assets/links/{match["id"]}', 'PUT', json.dumps(link).encode())
    else:
        call(f'{release_path}/assets/links', 'POST', json.dumps(link).encode())
    print('attached', name)
