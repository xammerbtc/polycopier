"""Read-only storage diagnostics; output field names/counts, never values."""
import ast
import pathlib
import re
import sqlite3

source = '/var/lib/docker/volumes/arbigab_arbigab-data/_data/gabagool.db'
c = sqlite3.connect(f'file:{source}?mode=ro', uri=True, timeout=3)
c.execute('pragma query_only=ON')
print('Setting columns:', [r[1] for r in c.execute('pragma table_info(Setting)')])
for table in ('Bot','BotNode'):
    rows = c.execute(f'select privateKey,funderAddress,dryRun from {table}').fetchall()
    print(table, 'rows:', len(rows), 'key lengths:', sorted({len(str(r[0] or "")) for r in rows}), 'address lengths:', sorted({len(str(r[1] or "")) for r in rows}), 'valid key format count:',sum(bool(re.fullmatch(r'(0x)?[0-9a-fA-F]{64}',str(r[0] or ''))) for r in rows))
c.close()
p = pathlib.Path('/opt/arbigab/operator_control/gabagool_manual_live/gabagool_manual_live.py')
tree = ast.parse(p.read_text())
for node in ast.walk(tree):
    if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
        literals = [n.value for n in ast.walk(node) if isinstance(n, ast.Constant) and isinstance(n.value,str)]
        if any(any(key in v for key in ('privateKey','funderAddress','BotNode','Setting')) for v in literals):
            print('credential-related function:',node.name,'line:',node.lineno)
