#!/usr/bin/env python3
"""Serve the browser build (dist/web) locally with the headers its threads need.

SharedArrayBuffer requires cross-origin isolation, so every response carries
COOP/COEP. Open http://localhost:8080 after `python3 web/build.py`.
"""
import argparse
import functools
import http.server
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


class Handler(http.server.SimpleHTTPRequestHandler):
    extensions_map = {**http.server.SimpleHTTPRequestHandler.extensions_map,
                      '.js': 'text/javascript', '.wasm': 'application/wasm', '.svg': 'image/svg+xml'}

    def end_headers(self):
        self.send_header('Cross-Origin-Opener-Policy', 'same-origin')
        self.send_header('Cross-Origin-Embedder-Policy', 'require-corp')
        self.send_header('Cache-Control', 'no-cache')
        super().end_headers()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--port', type=int, default=8080)
    parser.add_argument('--bind', default='127.0.0.1')
    parser.add_argument('--directory', type=Path, default=ROOT / 'dist/web')
    args = parser.parse_args()
    if not (args.directory / 'qemu-system-xtensa.wasm').exists():
        parser.error(f'{args.directory} has no browser build; run python3 web/build.py first')
    handler = functools.partial(Handler, directory=str(args.directory))
    with http.server.ThreadingHTTPServer((args.bind, args.port), handler) as server:
        print(f'Serving {args.directory} at http://{"localhost" if args.bind == "127.0.0.1" else args.bind}:{args.port}')
        server.serve_forever()


if __name__ == '__main__':
    main()
