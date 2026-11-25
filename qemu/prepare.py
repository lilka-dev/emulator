"""Prepare pinned QEMU sources in a NEW directory; never edit an existing tree."""
import argparse,hashlib,json,subprocess,tarfile
from pathlib import Path
p=argparse.ArgumentParser();p.add_argument('archive',type=Path);p.add_argument('destination',type=Path);a=p.parse_args()
bundle=Path(__file__).resolve().parent;manifest=json.loads((bundle/'source-manifest.json').read_text())
def sha(path):
 with path.open('rb') as f:return hashlib.file_digest(f,'sha256').hexdigest()
if sha(a.archive)!=manifest['source_archive_sha256']:p.error('Archive does not match the pinned source SHA-256')
for patch in manifest['patches']:
 if sha(bundle/patch['file'])!=patch['sha256']:p.error('Patch hash mismatch: '+patch['file'])
a.destination.mkdir(parents=True,exist_ok=False)
with tarfile.open(a.archive) as archive:archive.extractall(a.destination,filter='data')
source=a.destination/('qemu-'+manifest['tag'])
for patch in manifest['patches']:
 subprocess.run(['patch','-p1','--batch','--forward','-i',str(bundle/patch['file'])],cwd=source,check=True)
for item in manifest['source_files']:
 if sha(source/item['file'])!=item['sha256']:raise RuntimeError('Source mismatch: '+item['file'])
print('Verified sources:',source.resolve())
