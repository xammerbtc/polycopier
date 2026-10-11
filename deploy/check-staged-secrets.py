"""Check staged files against actual local secret values without printing them."""
import pathlib
import subprocess

root = pathlib.Path('/opt/polycopier')
values = []
for name in ('.env','.env.credentials'):
    p = root / name
    if not p.exists(): continue
    for line in p.read_text().splitlines():
        if '=' not in line: continue
        key, value = line.split('=',1)
        if key in ('PRIVATE_KEY','DASHBOARD_TOKEN') and len(value) >= 16:
            values.append(value.encode())
            if value.startswith('0x'): values.append(value[2:].encode())
files = subprocess.check_output(['git','-c','safe.directory=/opt/polycopier','diff','--cached','--name-only','-z'],cwd=root).split(b'\0')
for file in files:
    if not file: continue
    p = root / file.decode()
    if p.exists() and any(value in p.read_bytes() for value in values):
        raise SystemExit('Secret check failed: staged file contains a protected secret value')
print('PASS: staged changes contain none of the deployed secret values.')
