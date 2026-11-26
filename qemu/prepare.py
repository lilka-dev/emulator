"""Prepare pinned QEMU sources in a NEW directory; never edit an existing tree."""
import argparse,copy,hashlib,json,subprocess,tarfile
from pathlib import Path
p=argparse.ArgumentParser();p.add_argument('archive',type=Path);p.add_argument('destination',type=Path);a=p.parse_args()
bundle=Path(__file__).resolve().parent;manifest=json.loads((bundle/'source-manifest.json').read_text())
def extract_filter(member,destination):
 try:return tarfile.data_filter(member,destination)
 except tarfile.LinkOutsideDestinationError:
  if not member.issym():raise
  root=Path(destination).resolve()
  target=(root/member.name).parent.joinpath(member.linkname).resolve()
  if target==root or not target.is_relative_to(root):raise
  safe_member=copy.copy(member);safe_member.linkname=member.name
  filtered=tarfile.data_filter(safe_member,destination)
  filtered.linkname=member.linkname
  return filtered
def sha(path):
 digest=hashlib.sha256()
 with path.open('rb') as f:
  for chunk in iter(lambda:f.read(1024*1024),b''):digest.update(chunk)
 return digest.hexdigest()
if sha(a.archive)!=manifest['source_archive_sha256']:p.error('Archive does not match the pinned source SHA-256')
for patch in manifest['patches']:
 if sha(bundle/patch['file'])!=patch['sha256']:p.error('Patch hash mismatch: '+patch['file'])
if not hasattr(tarfile,'data_filter'):p.error('Install Python security updates: tarfile.data_filter is required')
a.destination.mkdir(parents=True,exist_ok=False)
with tarfile.open(a.archive) as archive:archive.extractall(a.destination,filter=extract_filter)
source=a.destination/('qemu-'+manifest['tag'])
for patch in manifest['patches']:
 subprocess.run(['patch','-p1','--batch','--forward','-i',str(bundle/patch['file'])],cwd=source,check=True)
for item in manifest['source_files']:
 if sha(source/item['file'])!=item['sha256']:raise RuntimeError('Source mismatch: '+item['file'])
print('Verified sources:',source.resolve())
