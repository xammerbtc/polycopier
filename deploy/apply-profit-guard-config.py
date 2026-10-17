"""Add visible profit-guard defaults to the existing paper TOML without resetting it."""
from pathlib import Path

path = Path('/opt/polycopier/config.toml')
lines = path.read_text().splitlines()
start = lines.index('[risk]') + 1
end = next((i for i in range(start, len(lines)) if lines[i].startswith('[')), len(lines))
block = '\n'.join(lines[start:end])
additions = []
if 'profit_guard_trigger_pct' not in block:
    additions.append('profit_guard_trigger_pct = 0.07')
if 'profit_guard_loss_pct' not in block:
    additions.append('profit_guard_loss_pct = 0.10')
if additions:
    lines[end:end] = additions + ['']
    temporary = path.with_suffix('.toml.tmp')
    temporary.write_text('\n'.join(lines) + '\n')
    temporary.replace(path)
print('Paper profit guard config present')
