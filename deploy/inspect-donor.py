"""Report credential field names only. Never output configuration values."""
import json
import pathlib
import sqlite3

base = pathlib.Path('/var/lib/docker/volumes/arbigab_arbigab-data/_data')
p = base / 'configs/69faaa2d-3462-45e0-8f2f-daedf1493456.json'
obj = json.loads(p.read_text())
print('config field names:', sorted(obj.keys()))
db = sqlite3.connect(f'file:{base / "gabagool.db"}?mode=ro', uri=True, timeout=2)
db.execute('pragma query_only=ON')
tables = [r[0] for r in db.execute("select name from sqlite_master where type='table'")]
print('database table names:', tables)
for table in tables:
    if any(w in table.lower() for w in ('account', 'wallet', 'bot', 'config', 'user', 'credential')):
        cols = db.execute('pragma table_info("' + table.replace('"', '""') + '")').fetchall()
        print('column names:', table, [c[1] for c in cols])
db.close()
