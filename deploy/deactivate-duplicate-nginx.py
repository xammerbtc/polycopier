"""Deactivate a stale enabled nginx backup without deleting its target file."""

import pathlib
import subprocess


ENABLED = pathlib.Path('/etc/nginx/sites-enabled')
DUPLICATE = ENABLED / 'arbigab-dashboard-8443.bak-20261012'
EXPECTED_TARGET = pathlib.Path('/etc/nginx/sites-available/arbigab-dashboard-8443')
ACTIVE = ENABLED / 'arbigab-dashboard-8443'


def main() -> None:
    if not DUPLICATE.is_symlink():
        print('Duplicate nginx backup already inactive')
        return
    if DUPLICATE.resolve() != EXPECTED_TARGET or not ACTIVE.is_file():
        raise RuntimeError('Unexpected nginx layout; refusing to change it')
    raw_target = DUPLICATE.readlink()
    DUPLICATE.unlink()
    try:
        subprocess.run(['nginx', '-t'], check=True, capture_output=True, text=True)
        subprocess.run(['systemctl', 'reload', 'nginx'], check=True)
    except BaseException:
        DUPLICATE.symlink_to(raw_target)
        subprocess.run(['systemctl', 'reload', 'nginx'], check=False)
        raise
    print('Duplicate nginx backup deactivated; original file retained in sites-available')


if __name__ == '__main__':
    main()
