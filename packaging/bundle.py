#!/usr/bin/env python3
"""Bundle Linux libraries without replacing the host's glibc or graphics drivers."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess

HOST = re.compile(r"^(ld-linux|lib(c|m|pthread|rt|dl|resolv|util|nss_[^.]+)\.so\.)")
GUI = ["libX11.so.6", "libX11-xcb.so.1", "libXcursor.so.1", "libXi.so.6",
       "libXrandr.so.2", "libxcb.so.1", "libxcb-render.so.0", "libxcb-shape.so.0",
       "libxcb-xfixes.so.0", "libxcb-xkb.so.1", "libxkbcommon.so.0",
       "libxkbcommon-x11.so.0", "libwayland-client.so.0", "libwayland-cursor.so.0",
       "libwayland-egl.so.1", "libasound.so.2"]


def dependencies(path):
    output = subprocess.check_output(['ldd', str(path)], text=True)
    if 'not found' in output:
        raise RuntimeError(output)
    for name, location in re.findall(r'^\s*(\S+) => (/\S+)', output, re.M):
        if HOST.match(name):
            continue
        source = Path(location)
        if '/glibc-hwcaps/' in location:
            source = Path(location.split('/glibc-hwcaps/')[0]) / name
            if not source.exists():
                raise RuntimeError('No baseline CPU library for ' + name)
        yield name, source


def licenses(source, destination):
    if shutil.which('dpkg-query'):
        result = subprocess.run(['dpkg-query', '-S', str(source)], text=True, capture_output=True)
        if result.returncode:
            result = subprocess.run(['dpkg-query', '-S', '*/' + source.name], text=True, capture_output=True)
        for line in result.stdout.splitlines():
            package = line.split(': ', 1)[0].split(':')[0]
            copyright = Path('/usr/share/doc') / package / 'copyright'
            if copyright.is_file():
                shutil.copy2(copyright, destination / (package + '.txt'))
                return
    elif shutil.which('rpm'):
        package = subprocess.check_output(['rpm', '-qf', '--qf', '%{NAME}', str(source)], text=True)
        folder = Path('/usr/share/licenses') / package
        if folder.is_dir():
            shutil.copytree(folder, destination / package, dirs_exist_ok=True)
            return
    raise RuntimeError('Cannot locate redistribution license for ' + str(source))


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('directory', type=Path)
    args = parser.parse_args()
    root = args.directory.resolve()
    staging = root / '.lib.new'
    shutil.rmtree(staging, ignore_errors=True)
    staging.mkdir()
    notices = root / 'LICENSES/system-libraries'
    notices.mkdir(parents=True, exist_ok=True)
    common = Path('/usr/share/common-licenses')
    if common.is_dir():
        shutil.copytree(common, notices / 'common', dirs_exist_ok=True)
    cache = subprocess.check_output(['ldconfig', '-p'], text=True)
    pending = list(dependencies(root / 'lilka-emulator'))
    pending += list(dependencies(root / 'liblilka_qemu_board_bridge.so'))
    for name in GUI:
        candidates = re.findall(r'^\s*' + re.escape(name) + r' .* => (/\S+)', cache, re.M)
        candidates = [p for p in candidates if 'x86-64' in next(
            line for line in cache.splitlines() if line.strip().startswith(name + ' ') and p in line)]
        if not candidates:
            raise RuntimeError('Install runtime library before bundling: ' + name)
        pending.append((name, Path(candidates[0])))
    copied = {}
    while pending:
        name, source = pending.pop()
        if name in copied:
            continue
        target = staging / name
        shutil.copy2(source, target, follow_symlinks=True)
        licenses(source, notices)
        copied[name] = source.name
        pending.extend(dependencies(source))
        subprocess.run(['patchelf', '--force-rpath', '--set-rpath', '$ORIGIN', str(target)], check=True)
    old = root / 'lib'
    shutil.rmtree(old, ignore_errors=True)
    staging.rename(old)
    for name in ['lilka-emulator', 'liblilka_qemu_board_bridge.so']:
        subprocess.run(['patchelf', '--force-rpath', '--set-rpath', '$ORIGIN/lib', str(root / name)], check=True)
    versions = subprocess.check_output(['readelf', '--version-info', str(root / 'lilka-emulator'),
                                        str(root / 'liblilka_qemu_board_bridge.so'),
                                        *[str(p) for p in old.iterdir()]], text=True)
    required = sorted(set(re.findall(r'GLIBC_[0-9.]+', versions)), key=lambda v: tuple(map(int, v[6:].split('.'))))
    manifest = {'glibc_minimum': required[-1] if required else None,
                'libraries': {name: hashlib.sha256((old / name).read_bytes()).hexdigest() for name in sorted(copied)}}
    (root / 'runtime.json').write_text(json.dumps(manifest, indent=2) + '\n')
    print('Bundled', len(copied), 'libraries;', manifest['glibc_minimum'])


if __name__ == '__main__':
    main()
