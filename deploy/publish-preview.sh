#!/bin/bash
set -euo pipefail
cd /opt/polycopier
test "$(cat build.exit)" = 0
test "$(cat build.phase)" = complete
g() { git -c safe.directory=/opt/polycopier "$@"; }
# Windows checkout line endings must not turn untouched files into unrelated diffs.
python3 - <<'PY'
import pathlib,subprocess
names=subprocess.check_output(['git','-c','safe.directory=/opt/polycopier','ls-files','--cached','--others','--exclude-standard','-z']).split(b'\0')
for name in names:
    if not name: continue
    p=pathlib.Path(name.decode())
    if p.exists() and (p.name == '.gitignore' or p.suffix in ('.rs','.sh','.service','.toml','.tsx','.ts','.md','.css','.example','.py','.lock','.svg')):
        raw=p.read_bytes()
        if b'\r\n' in raw: p.write_bytes(raw.replace(b'\r\n',b'\n'))
PY
g diff --check > diff-check.log
g switch codex/vps-preview 2>/dev/null || g switch -c codex/vps-preview
g config user.name 'Codex'
g config user.email 'codex@users.noreply.github.com'
g add src tests Cargo.toml Cargo.lock deploy web/src .gitignore .env.example README.md docs/VPS-PREVIEW.md
python3 deploy/check-staged-secrets.py
g diff --cached --check
g commit -m 'feat: deploy authenticated dry-run copier with RTDS and persistent state'
if ! g remote get-url fork >/dev/null 2>&1; then g remote add fork https://github.com/xammerbtc/polycopier.git; fi
g config credential.https://github.com.helper '!gh auth git-credential'
g push -u fork codex/vps-preview
