#!/usr/bin/env python3
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys
import tarfile
import urllib.request

ROOT = Path(__file__).resolve().parent
ROM_SHA = 'b5c7090cc22efce51c1323cef31ca96fcd673a5b1d1b1017e219d393f2659913'
RELEASE_SHA = '0eecb2a34a5586c0e59110f77b9343b7b336e82fdb0e1a30e1dc1bab8a547e35'
RELEASE = 'https://github.com/espressif/qemu/releases/download/esp-develop-9.2.2-20260417/qemu-xtensa-softmmu-esp_develop_9.2.2_20260417-x86_64-linux-gnu.tar.xz'


def run(args, cwd=ROOT):
    subprocess.run([str(a) for a in args], cwd=cwd, check=True)


def sha(path):
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def download(url, destination, expected):
    if not destination.exists() or sha(destination) != expected:
        temporary = destination.with_suffix('.download')
        with urllib.request.urlopen(url, timeout=90) as response, temporary.open('wb') as stream:
            shutil.copyfileobj(response, stream)
        if sha(temporary) != expected:
            temporary.unlink()
            raise RuntimeError('Download checksum mismatch: ' + url)
        temporary.replace(destination)
    return destination


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--archive', type=Path)
    parser.add_argument('--qemu-tree', type=Path)
    parser.add_argument('--qemu-build', type=Path)
    parser.add_argument('--rom', type=Path)
    parser.add_argument('--jobs', type=int, default=min(os.cpu_count() or 1, 6))
    args = parser.parse_args()
    if sys.platform != 'linux' or platform.machine() != 'x86_64':
        parser.error('This integrated build currently supports Linux x86-64')
    cache = ROOT / '.build'
    cache.mkdir(exist_ok=True)
    manifest = json.loads((ROOT / 'qemu/source-manifest.json').read_text())
    if args.qemu_tree:
        source = args.qemu_tree.resolve()
    else:
        archive = args.archive or download(
            'https://github.com/espressif/qemu/archive/refs/tags/' + manifest['tag'] + '.tar.gz',
            cache / 'qemu-source.tar.gz', manifest['source_archive_sha256'])
        source = cache / 'qemu' / ('qemu-' + manifest['tag'])
        if not source.exists():
            run([sys.executable, ROOT / 'qemu/prepare.py', archive.resolve(), cache / 'qemu'])
    for item in manifest['source_files']:
        if sha(source / item['file']) != item['sha256']:
            raise RuntimeError('QEMU source changed: ' + item['file'] + '; prepare a fresh .build/qemu tree')
    run(['cargo', 'build', '--release', '--locked', '--workspace'])
    library = ROOT / 'target/release/liblilka_emulator.a'
    build = args.qemu_build.resolve() if args.qemu_build else source / 'build'
    if not (build / 'build.ninja').exists():
        build.mkdir(exist_ok=True)
        run([source / 'configure', '--target-list=xtensa-softmmu', '--enable-gcrypt',
             '--disable-docs', '--disable-gtk', '--disable-sdl', '--disable-vnc',
             '--disable-slirp', '--disable-tools', '--disable-user', '--disable-werror'], build)
    meson = build / 'pyvenv/bin/meson'
    run([meson, 'configure', build, '-Doptimization=3', '-Db_lto=true',
         '-Dx86_version=1', '-Dlilka_frontend=' + str(library)])
    run(['ninja', '-C', build, '-j', args.jobs, 'qemu-system-xtensa'])
    destination = ROOT / 'dist/lilka-emulator'
    destination.mkdir(parents=True, exist_ok=True)
    executable = destination / '.lilka-emulator.new'
    shutil.copy2(build / 'qemu-system-xtensa', executable)
    run(['strip', '--strip-debug', executable])
    executable.replace(destination / 'lilka-emulator')
    bridge = destination / '.liblilka_qemu_board_bridge.so.new'
    shutil.copy2(ROOT / 'target/release/liblilka_qemu_board_bridge.so', bridge)
    bridge.replace(destination / 'liblilka_qemu_board_bridge.so')
    (destination / 'rom').mkdir(exist_ok=True)
    rom = destination / 'rom/esp32s3_rev0_rom.bin'
    if args.rom:
        shutil.copy2(args.rom, rom)
    elif not rom.exists():
        release = download(RELEASE, cache / 'espressif-qemu.tar.xz', RELEASE_SHA)
        with tarfile.open(release) as archive:
            member = next(m for m in archive.getmembers() if m.name.endswith('/esp32s3_rev0_rom.bin'))
            with archive.extractfile(member) as stream, rom.open('wb') as output:
                shutil.copyfileobj(stream, output)
    if sha(rom) != ROM_SHA:
        raise RuntimeError('Unexpected ESP32-S3 ROM checksum')
    shutil.copytree(ROOT / 'LICENSES', destination / 'LICENSES', dirs_exist_ok=True)
    shutil.copy2(ROOT / 'packaging/fonts.conf', destination)
    shutil.copy2(ROOT / 'LICENSE', destination)
    shutil.copy2(ROOT / 'README.md', destination)
    metadata = json.loads(subprocess.check_output(['cargo', 'metadata', '--locked', '--format-version=1', '--filter-platform=x86_64-unknown-linux-gnu'], cwd=ROOT))
    with (destination / 'LICENSES/Rust-dependencies.txt').open('w') as output:
        for package in sorted(metadata['packages'], key=lambda p: (p['name'], p['version'])):
            output.write('\n' + package['name'] + ' ' + package['version'] + '\n' + str(package['license']) + '\n')
            directory = Path(package['manifest_path']).parent
            for license in sorted(directory.iterdir()):
                if license.is_file() and license.name.upper().startswith(('LICENSE', 'COPYING', 'NOTICE')):
                    output.write('\n' + license.name + '\n' + license.read_text(errors='replace') + '\n')
    print('Built:', destination / 'lilka-emulator')


if __name__ == '__main__':
    main()
