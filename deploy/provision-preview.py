"""Server-only provisioning. Credential values must never reach stdout."""
import os
import pathlib
import re
import secrets
import sqlite3
import subprocess

root = pathlib.Path('/opt/polycopier')
uid = int(subprocess.check_output(['id', '-u', 'polycopier']))
gid = int(subprocess.check_output(['id', '-g', 'polycopier']))

def private_file(path, text, owner=False):
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(fd, 'w') as f:
        f.write(text)
    if owner:
        os.chown(path, uid, gid)

env = root / '.env'
if not env.exists():
    private_file(env, 'DASHBOARD_TOKEN=' + secrets.token_urlsafe(32) + '\nDASHBOARD_BIND=127.0.0.1:3000\nPOLYCOPIER_MODE=simulation\n', True)

source = pathlib.Path('/var/lib/docker/volumes/arbigab_arbigab-data/_data/gabagool.db')
db = sqlite3.connect(f'file:{source}?mode=ro', uri=True, timeout=3)
db.execute('pragma query_only=ON')
rows = db.execute('select privateKey, funderAddress from Bot where dryRun = 0').fetchall()
settings = dict(db.execute('select key,value from Setting where key in (?,?)', ('privateKey','funderAddress')).fetchall())
if settings.get('privateKey') and settings.get('funderAddress'):
    rows.append((settings['privateKey'], settings['funderAddress']))
db.close()
pairs = {(str(k).strip(), str(a).strip()) for k, a in rows
         if re.fullmatch(r'(0x)?[0-9a-fA-F]{64}', str(k).strip())
         and re.fullmatch(r'0x[0-9a-fA-F]{40}', str(a).strip())}
destination = root / '.env.credentials'
if destination.exists():
    print('Credential staging file already exists; preserved.')
elif len(pairs) == 1:
    key, address = pairs.pop()
    private_file(destination, f'PRIVATE_KEY={key}\nFUNDER_ADDRESS={address}\n')
    print('Credential pair transferred locally on VPS; root-only staging file, not loaded by preview service.')
else:
    print(f'Credential transfer needs source selection: {len(pairs)} distinct valid live wallet pairs. No credentials copied.')

print('Dashboard token stored in /opt/polycopier/.env; no secret values printed.')
