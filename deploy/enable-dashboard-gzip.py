"""Enable gzip only on the loopback dashboard proxy, with validation and rollback."""

import os
import pathlib
import shutil
import subprocess
import tempfile

CONFIG = pathlib.Path('/etc/nginx/sites-enabled/arbigab-dashboard-8443')
BACKUP = pathlib.Path('/etc/nginx/sites-available/arbigab-dashboard-8443.pre-gzip')
ANCHOR = '    server_name localhost;\n'
SETTINGS = (
    '    gzip on;\n'
    '    gzip_vary on;\n'
    '    gzip_min_length 1024;\n'
    '    gzip_comp_level 5;\n'
    '    gzip_types application/json text/css application/javascript;\n'
)


def main() -> None:
    original = CONFIG.read_text()
    first_server, separator, remainder = original.partition('\n}\n')
    if not separator or original.count(ANCHOR) != 1:
        raise RuntimeError('Unexpected nginx server layout; inspect manually')
    if SETTINGS in first_server and '        proxy_buffering on;\n' in first_server:
        print('Loopback dashboard gzip and buffering already configured')
        return
    if first_server.count('        proxy_buffering off;\n') != 1:
        raise RuntimeError('Unexpected loopback proxy buffering; inspect manually')
    if BACKUP.exists() and SETTINGS not in first_server:
        raise RuntimeError('Existing backup does not match current migration state')
    if not BACKUP.exists():
        shutil.copy2(CONFIG, BACKUP)
    if SETTINGS not in first_server:
        first_server = first_server.replace(ANCHOR, ANCHOR + SETTINGS, 1)
    first_server = first_server.replace('        proxy_buffering off;\n',
                                        '        proxy_buffering on;\n', 1)
    updated = first_server + separator + remainder
    with tempfile.NamedTemporaryFile(mode='w', dir=CONFIG.parent,
                                     prefix='.polycopier-gzip-', delete=False) as stream:
        stream.write(updated)
        temporary = pathlib.Path(stream.name)
    os.chmod(temporary, CONFIG.stat().st_mode & 0o777)
    os.replace(temporary, CONFIG)
    try:
        subprocess.run(['nginx', '-t'], check=True, capture_output=True, text=True)
        subprocess.run(['systemctl', 'reload', 'nginx'], check=True)
    except BaseException:
        shutil.copy2(BACKUP, CONFIG)
        subprocess.run(['systemctl', 'reload', 'nginx'], check=False)
        raise
    print('Loopback dashboard gzip enabled; backup saved outside sites-enabled')


if __name__ == '__main__':
    main()
