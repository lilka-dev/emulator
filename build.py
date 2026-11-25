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
        digest = hashlib.sha256()
        for chunk in iter(lambda: stream.read(1024 * 1024), b''):
            digest.update(chunk)
        return digest.hexdigest()


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
    parser.add_argument('--no-lto', action='store_true', help='Faster linking for local development')
    parser.add_argument('--portable', action='store_true', help='Bundle shared libraries (requires patchelf)')
    args = parser.parse_args()
    if sys.platform != 'linux' or platform.machine() != 'x86_64':
        parser.error('This integrated build currently supports Linux x86-64')
    if args.jobs < 1:
        parser.error('--jobs must be positive')
    if args.portable and not shutil.which('patchelf'):
        parser.error('--portable requires patchelf')
    cache = ROOT / '.build'
    cache.mkdir(exist_ok=True)
    manifest = json.loads((ROOT / 'qemu/source-manifest.json').read_text())
    if args.qemu_tree:
        source = args.qemu_tree.resolve()
    else:
        archive = args.archive or download(
            'https://github.com/espressif/qemu/archive/refs/tags/' + manifest['tag'] + '.tar.gz',
            cache / 'qemu-source.tar.gz', manifest['source_archive_sha256'])
        prepared = cache / ('qemu-' + sha(ROOT / 'qemu/source-manifest.json')[:12])
        source = prepared / ('qemu-' + manifest['tag'])
        if not source.exists():
            run([sys.executable, ROOT / 'qemu/prepare.py', archive.resolve(), prepared])
    for item in manifest['source_files']:
        if sha(source / item['file']) != item['sha256']:
            raise RuntimeError('QEMU source changed: ' + item['file'] + '; prepare a fresh QEMU source tree')
    run(['cargo', 'build', '--release', '--locked', '--workspace', '--jobs', args.jobs])
    target = Path(json.loads(subprocess.check_output(
        ['cargo', 'metadata', '--no-deps', '--format-version=1'], cwd=ROOT))['target_directory'])
    library = target / 'release/liblilka_emulator.a'
    build = args.qemu_build.resolve() if args.qemu_build else source / 'build'
    configuration = [str(source / 'configure'), '--target-list=xtensa-softmmu',
                     '--without-default-features', '--without-default-devices',
                     '--with-devices-xtensa=lilka', '--enable-gcrypt', '--enable-pixman',
                     '--disable-fdt', '--disable-docs', '--disable-tools',
                     '--disable-user', '--disable-werror']
    options = ['-Doptimization=3', '-Ddebug=false',
               '-Db_lto=' + str(not args.no_lto).lower(),
               '-Db_lto_threads=' + str(min(args.jobs, 2)),
               '-Dx86_version=1', '-Dlilka_frontend=' + str(library)]
    stamp = build / 'lilka-build.json'
    settings = configuration + options
    if not (build / 'build.ninja').exists() or not stamp.exists() or json.loads(stamp.read_text()) != settings:
        build.mkdir(parents=True, exist_ok=True)
        run(configuration + options, build)
        stamp.write_text(json.dumps(settings))
    run(['ninja', '-C', build, '-j', args.jobs, 'qemu-system-xtensa'])
    destination = ROOT / 'dist/lilka-emulator'
    destination.mkdir(parents=True, exist_ok=True)
    executable = destination / '.lilka-emulator.new'
    shutil.copy2(build / 'qemu-system-xtensa', executable)
    run(['strip', '--strip-debug', executable])
    executable.replace(destination / 'lilka-emulator')
    bridge = destination / '.liblilka_qemu_board_bridge.so.new'
    shutil.copy2(target / 'release/liblilka_qemu_board_bridge.so', bridge)
    bridge.replace(destination / 'liblilka_qemu_board_bridge.so')
    (destination / 'rom').mkdir(exist_ok=True)
    rom = destination / 'rom/esp32s3_rev0_rom.bin'
    if args.rom:
        if args.rom.resolve() != rom.resolve():
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
    if args.portable:
        run([sys.executable, ROOT / 'packaging/bundle.py', destination])
    else:
        shutil.rmtree(destination / 'lib', ignore_errors=True)
        (destination / 'runtime.json').unlink(missing_ok=True)
    print('Built:', destination / 'lilka-emulator')


if __name__ == '__main__':
    main()
