#!/usr/bin/env python3
"""Build the browser (WebAssembly) package of the Lilka emulator into dist/web.

Needs Emscripten (emcc on PATH, or EMSDK set), Rust with the
wasm32-unknown-emscripten target and rust-src, Python 3.10+, Ninja and patch.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))
import build as native  # noqa: E402  (shared download/checksum helpers)

CACHE = ROOT / '.build/web'
SYSROOT = CACHE / 'sysroot'
DEPENDENCIES = [
    ('zlib', 'https://zlib.net/fossils/zlib-1.3.1.tar.gz',
     '9a93b2b7dfdac77ceba5a558a580e74667dd6fede4585b91eefb60f03b72df23'),
    ('libffi', 'https://github.com/libffi/libffi/releases/download/v3.4.7/libffi-3.4.7.tar.gz',
     '138607dee268bdecf374adf9144c00e839e38541f75f24a1fcf18b78fda48b2d'),
    ('pixman', 'https://cairographics.org/releases/pixman-0.44.2.tar.gz',
     '6349061ce1a338ab6952b92194d1b0377472244208d47ff25bef86fc71973466'),
    ('glib', 'https://download.gnome.org/sources/glib/2.84/glib-2.84.0.tar.xz',
     'f8823600cb85425e2815cfad82ea20fdaa538482ab74e7293d58b3f64a5aff6a'),
    ('libgpg-error', 'https://gnupg.org/ftp/gcrypt/libgpg-error/libgpg-error-1.55.tar.bz2',
     '95b178148863f07d45df0cea67e880a79b9ef71f5d230baddc0071128516ef78'),
    ('libgcrypt', 'https://gnupg.org/ftp/gcrypt/libgcrypt/libgcrypt-1.11.1.tar.bz2',
     '24e91c9123a46c54e8371f3a3a2502f1198f2893fbfbf59af95bc1c21499b00e'),
]
# Every object shares one threaded wasm memory, so all of them need atomics.
CFLAGS = ['-O3', '-pthread']
LDFLAGS = ['-pthread', '-sWASM_BIGINT', '-L' + str(SYSROOT / 'lib')]


def run(args, cwd=ROOT, env=None):
    print('+', ' '.join(str(a) for a in args), flush=True)
    subprocess.run([str(a) for a in args], cwd=cwd, check=True, env=env)


def emscripten_env():
    env = dict(os.environ)
    if not shutil.which('emcc', path=env.get('PATH')):
        emsdk = Path(env.get('EMSDK') or ROOT / '.build/emsdk')
        tools = emsdk / 'upstream/emscripten'
        if not (tools / 'emcc').exists():
            sys.exit('Emscripten not found: put emcc on PATH or set EMSDK')
        env['EMSDK'] = str(emsdk)
        env['PATH'] = os.pathsep.join([str(tools), str(emsdk), env['PATH']])
        node = sorted((emsdk / 'node').glob('*/bin'))
        if node:
            env['PATH'] = str(node[-1]) + os.pathsep + env['PATH']
    # Emscripten's Python tools need 3.10+, which the system python3 may not be.
    env.setdefault('EMSDK_PYTHON', sys.executable)
    env['PKG_CONFIG_PATH'] = env['EM_PKG_CONFIG_PATH'] = str(SYSROOT / 'lib/pkgconfig')
    env['PKG_CONFIG_LIBDIR'] = env['PKG_CONFIG_PATH']
    env['CFLAGS'] = env['CXXFLAGS'] = ' '.join(CFLAGS + ['-I' + str(SYSROOT / 'include')])
    env['LDFLAGS'] = ' '.join(LDFLAGS)
    return env


def meson():
    venv = CACHE / 'venv'
    if not (venv / 'bin/meson').exists():
        run([sys.executable, '-m', 'venv', venv])
        run([venv / 'bin/pip', 'install', '--quiet', 'meson==1.5.0', 'tomli'])
    return venv / 'bin/meson'


def cross_file(extra_c=()):
    quoted = lambda items: ', '.join(repr(i) for i in items)
    c_args = CFLAGS + ['-I' + str(SYSROOT / 'include')] + list(extra_c)
    path = CACHE / ('cross-' + hashlib.sha256(repr(c_args).encode()).hexdigest()[:8] + '.meson')
    path.write_text(f"""[host_machine]
system = 'emscripten'
cpu_family = 'wasm32'
cpu = 'wasm32'
endian = 'little'

[binaries]
c = 'emcc'
cpp = 'em++'
ar = 'emar'
ranlib = 'emranlib'
pkg-config = ['pkg-config', '--static']

[built-in options]
c_args = [{quoted(c_args)}]
cpp_args = [{quoted(c_args)}]
c_link_args = [{quoted(LDFLAGS)}]
cpp_link_args = [{quoted(LDFLAGS)}]
""")
    return path


def unpack(name, url, expected):
    (CACHE / 'dl').mkdir(parents=True, exist_ok=True)
    archive = native.download(url, CACHE / 'dl' / url.rsplit('/', 1)[1], expected)
    source = CACHE / 'src' / archive.name.split('.tar')[0]
    if not source.exists():
        source.parent.mkdir(parents=True, exist_ok=True)
        temporary = source.with_name(source.name + '.partial')
        shutil.rmtree(temporary, ignore_errors=True)
        with tarfile.open(archive) as stream:
            stream.extractall(temporary, filter='data')
        (inner,) = temporary.iterdir()
        inner.replace(source)
        temporary.rmdir()
    return source


def build_dependencies(env, jobs):
    SYSROOT.mkdir(parents=True, exist_ok=True)
    stamps = SYSROOT / 'stamps'
    stamps.mkdir(exist_ok=True)
    for name, url, expected in DEPENDENCIES:
        stamp = stamps / (name + '-' + expected[:12])
        if stamp.exists():
            continue
        source = unpack(name, url, expected)
        configure = ['emconfigure', './configure', '--prefix=' + str(SYSROOT)]
        # Autoconf generator tools run on the build machine: use the host compiler.
        autoconf = configure + ['CC_FOR_BUILD=' + (shutil.which('cc') or 'cc')]
        if name == 'zlib':
            run(configure + ['--static'], source, env)
            # On macOS zlib's configure picks Apple libtool, which archives nothing useful.
            run(['emmake', 'make', '-j', jobs, 'install', 'AR=emar', 'ARFLAGS=rcs',
                 'RANLIB=emranlib'], source, env)
        elif name == 'libffi':
            run(autoconf + ['--host=wasm32-unknown-linux', '--enable-static', '--disable-shared',
                             '--disable-dependency-tracking', '--disable-builddir',
                             '--disable-multi-os-directory', '--disable-raw-api',
                             '--disable-docs'], source, env)
            run(['emmake', 'make', '-j', jobs, 'install', 'SUBDIRS=include'], source, env)
        elif name == 'pixman':
            build = source / '_build'
            shutil.rmtree(build, ignore_errors=True)
            run([meson(), 'setup', build, '--prefix=' + str(SYSROOT), '--cross-file', cross_file(),
                 '--default-library=static', '--buildtype=release',
                 '-Dtests=disabled', '-Ddemos=disabled'], source, env)
            run([meson(), 'install', '-C', build], source, env)
        elif name == 'glib':
            stub = CACHE / 'resolv'
            stub.mkdir(exist_ok=True)
            # Emscripten has no resolver; GLib only links against res_query.
            (stub / 'res_query.c').write_text(
                '#include <netdb.h>\nint res_query(const char *n, int c, int t, unsigned char *d, int l)\n'
                '{ h_errno = HOST_NOT_FOUND; return -1; }\n')
            run(['emcc', *CFLAGS, '-c', stub / 'res_query.c', '-o', stub / 'res_query.o'], stub, env)
            run(['emar', 'rcs', SYSROOT / 'lib/libresolv.a', stub / 'res_query.o'], stub, env)
            # meson calls emcc 'emscripten', which skips the probe that makes gsize
            # match size_t (unsigned long); unsigned int then breaks every caller.
            script = source / 'meson.build'
            script.write_text(script.read_text().replace(
                "if cc.get_id() == 'gcc' or cc.get_id() == 'clang'\n  foreach type_name, size_compatibility",
                "if cc.get_id() in ['gcc', 'clang', 'emscripten']\n  foreach type_name, size_compatibility"))
            build = source / '_build'
            shutil.rmtree(build, ignore_errors=True)
            run([meson(), 'setup', build, '--prefix=' + str(SYSROOT),
                 '--cross-file', cross_file(['-Wno-incompatible-function-pointer-types']),
                 '--default-library=static', '--buildtype=release', '--force-fallback-for=pcre2',
                 '-Dselinux=disabled', '-Dxattr=false', '-Dlibmount=disabled', '-Dnls=disabled',
                 '-Dtests=false', '-Dglib_debug=disabled', '-Dglib_assert=false',
                 '-Dglib_checks=false', '-Dintrospection=disabled', '-Dsysprof=disabled'],
                source, env)
            # Emscripten declares these but does not provide them in the final link.
            config = build / 'config.h'
            config.write_text('\n'.join(
                line for line in config.read_text().splitlines()
                if line not in ('#define HAVE_POSIX_SPAWN 1', '#define HAVE_PTHREAD_GETNAME_NP 1')) + '\n')
            run([meson(), 'install', '-C', build], source, env)
        elif name == 'libgpg-error':
            run(autoconf + ['--host=wasm32-unknown-emscripten', '--enable-static', '--disable-shared',
                             '--disable-doc', '--disable-tests', '--disable-nls',
                             '--disable-languages', '--enable-install-gpg-error-config'], source, env)
            # The lock object sizes come from a target program; run it under Node.
            run(['emmake', 'make', '-C', 'src', 'gen-posix-lock-obj'], source, env)
            with (source / 'src/lock-obj-pub.native.h').open('w') as header:
                subprocess.run(['node', 'gen-posix-lock-obj'], cwd=source / 'src', env=env,
                               stdout=header, check=True)
            run(['emmake', 'make', '-j', jobs, 'install'], source, env)
        elif name == 'libgcrypt':
            # Its FIPS-only getrandom path is compiled whenever the header has
            # GRND_RANDOM; Emscripten declares getrandom but has only getentropy.
            entropy = source / 'random/rndgetentropy.c'
            entropy.write_text(entropy.read_text().replace(
                '#ifdef GRND_RANDOM', '#if defined(GRND_RANDOM) && !defined(__EMSCRIPTEN__)'))
            run(autoconf + ['--host=wasm32-unknown-emscripten', '--enable-static', '--disable-shared',
                             '--disable-doc', '--disable-asm', '--disable-jent-support',
                             '--with-libgpg-error-prefix=' + str(SYSROOT)], source, env)
            run(['emmake', 'make', '-j', jobs, '-C', 'compat', 'install'], source, env)
            run(['emmake', 'make', '-j', jobs, '-C', 'mpi', 'install'], source, env)
            run(['emmake', 'make', '-j', jobs, '-C', 'cipher', 'install'], source, env)
            run(['emmake', 'make', '-j', jobs, '-C', 'random', 'install'], source, env)
            run(['emmake', 'make', '-j', jobs, '-C', 'src', 'install'], source, env)
        stamp.touch()


def prepare_qemu(qemu_tree):
    """The native patched QEMU tree plus the browser patches of qemu/web-manifest.json."""
    manifest = json.loads((ROOT / 'qemu/source-manifest.json').read_text())
    web = json.loads((ROOT / 'qemu/web-manifest.json').read_text())
    if qemu_tree:
        source = qemu_tree.resolve()
    else:
        archive = native.download(
            'https://github.com/espressif/qemu/archive/refs/tags/' + manifest['tag'] + '.tar.gz',
            ROOT / '.build/qemu-source.tar.gz', manifest['source_archive_sha256'])
        key = hashlib.sha256((ROOT / 'qemu/source-manifest.json').read_bytes() +
                             (ROOT / 'qemu/web-manifest.json').read_bytes()).hexdigest()
        prepared = CACHE / ('qemu-' + key[:12])
        source = prepared / ('qemu-' + manifest['tag'])
        if not source.exists():
            shutil.rmtree(prepared, ignore_errors=True)
            run([sys.executable, ROOT / 'qemu/prepare.py', archive.resolve(), prepared])
            for patch in web['patches']:
                path = ROOT / 'qemu' / patch['file']
                if native.sha(path) != patch['sha256']:
                    raise RuntimeError('Patch hash mismatch: ' + patch['file'])
                run(['patch', '-p1', '--batch', '--forward', '-i', path], source)
    for item in web['source_files']:
        if native.sha(source / item['file']) != item['sha256']:
            raise RuntimeError('QEMU source changed: ' + item['file'] + '; prepare a fresh QEMU source tree')
    return source


def build_rust(env, jobs):
    """liblilka_emulator.a for Emscripten: the session code and the board bridge."""
    env = dict(env)
    # Linking into QEMU's shared (threaded) memory needs std rebuilt with atomics;
    # RUSTC_BOOTSTRAP enables -Zbuild-std on the pinned stable toolchain.
    env['RUSTC_BOOTSTRAP'] = '1'
    env['RUSTFLAGS'] = '-C target-feature=+atomics,+bulk-memory,+mutable-globals -C panic=abort'
    run(['cargo', 'build', '--release', '--locked', '--jobs', jobs, '-p', 'lilka-emulator',
         '--target', 'wasm32-unknown-emscripten', '--no-default-features', '--features', 'web',
         '-Zbuild-std=std,panic_abort'], ROOT, env)
    target = Path(json.loads(subprocess.check_output(
        ['cargo', 'metadata', '--no-deps', '--format-version=1'], cwd=ROOT))['target_directory'])
    return target / 'wasm32-unknown-emscripten/release/liblilka_emulator.a'


def build_qemu(source, library, env, jobs, build):
    configuration = ['emconfigure', source / 'configure', '--static',
                     '--target-list=xtensa-softmmu', '--without-default-features',
                     '--without-default-devices', '--with-devices-xtensa=lilka',
                     '--enable-gcrypt', '--enable-pixman', '--enable-tcg-interpreter',
                     '--disable-fdt', '--disable-docs', '--disable-tools', '--disable-user',
                     '--disable-werror', '--python=' + sys.executable,
                     # meson's find_library (gpg-error) does not ask pkg-config.
                     '--extra-cflags=-I' + str(SYSROOT / 'include'),
                     '--extra-ldflags=-L' + str(SYSROOT / 'lib')]
    # wasm SIMD128 and full LTO were measured: no faster boot, longer links.
    options = ['-Doptimization=3', '-Ddebug=false', '-Dlilka_frontend=' + str(library)]
    settings = [str(a) for a in configuration + options]
    stamp = build / 'lilka-build.json'
    if not (build / 'build.ninja').exists() or not stamp.exists() or json.loads(stamp.read_text()) != settings:
        build.mkdir(parents=True, exist_ok=True)
        run(configuration + options, build, env)
        stamp.write_text(json.dumps(settings))
    run(['ninja', '-C', build, '-j', jobs, 'qemu-system-xtensa.js'], build, env)
    return build


def rom(destination):
    """The ESP32-S3 mask ROM from Espressif's QEMU release, as the desktop build ships it."""
    destination.parent.mkdir(parents=True, exist_ok=True)
    if not destination.exists() or native.sha(destination) != native.ROM_SHA:
        release = native.download(native.RELEASE, ROOT / '.build/espressif-qemu.tar.xz', native.RELEASE_SHA)
        with tarfile.open(release) as archive:
            member = next(m for m in archive.getmembers() if m.name.endswith('/esp32s3_rev0_rom.bin'))
            with archive.extractfile(member) as stream, destination.open('wb') as output:
                shutil.copyfileobj(stream, output)
    if native.sha(destination) != native.ROM_SHA:
        raise RuntimeError('Unexpected ESP32-S3 ROM checksum')


def package(build):
    destination = ROOT / 'dist/web'
    staging = destination.with_name('.web.new')
    shutil.rmtree(staging, ignore_errors=True)
    shutil.copytree(ROOT / 'web/static', staging)
    for name in ('qemu-system-xtensa.js', 'qemu-system-xtensa.wasm'):
        shutil.copy2(build / name, staging / name)
    rom(staging / 'rom/esp32s3_rev0_rom.bin')
    shutil.copytree(ROOT / 'LICENSES', staging / 'LICENSES')
    shutil.copy2(ROOT / 'LICENSE', staging)
    shutil.copy2(ROOT / 'README.md', staging)
    # Node reads the ES module loader as one too (web/test/boot.mjs).
    (staging / 'package.json').write_text('{"private": true, "type": "module"}\n')
    shutil.rmtree(destination, ignore_errors=True)
    staging.replace(destination)
    return destination


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--jobs', type=int, default=min(os.cpu_count() or 1, 8))
    parser.add_argument('--qemu-tree', type=Path, help='Already prepared QEMU source tree')
    parser.add_argument('--deps-only', action='store_true')
    args = parser.parse_args()
    if sys.version_info < (3, 10):
        parser.error('Emscripten needs Python 3.10+; run this script with it')
    env = emscripten_env()
    CACHE.mkdir(parents=True, exist_ok=True)
    build_dependencies(env, args.jobs)
    if args.deps_only:
        return
    source = prepare_qemu(args.qemu_tree)
    library = build_rust(env, args.jobs)
    build = build_qemu(source, library, env, args.jobs, source / 'build')
    print('Built:', package(build))


if __name__ == '__main__':
    main()
