"""Stage an isolated, stopped live service without submitting any orders."""

import os
import pathlib
import pwd
import secrets
import stat
import subprocess
import tomllib

ROOT = pathlib.Path('/opt/polycopier')
LIVE = pathlib.Path('/opt/polycopier-live')
UNIT = pathlib.Path('/etc/systemd/system/polycopier.service')
SOURCE_CREDENTIALS = ROOT / '.env.credentials'
SOURCE_CONFIG = ROOT / 'deploy/config.live-1000.toml'
SOURCE_UNIT = ROOT / 'deploy/polycopier.service'
SOURCE_BINARY = ROOT / 'target/release/polycopier'


def write_new(path: pathlib.Path, content: bytes, mode: int, uid: int, gid: int) -> None:
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, mode)
    try:
        with os.fdopen(fd, 'wb') as stream:
            stream.write(content)
            stream.flush()
            os.fsync(stream.fileno())
        os.chown(path, uid, gid)
        os.chmod(path, mode)
    except BaseException:
        path.unlink(missing_ok=True)
        raise


def main() -> None:
    if os.geteuid() != 0:
        raise RuntimeError('Staging requires root')
    if subprocess.run(['systemctl', 'is-active', '--quiet', 'polycopier']).returncode == 0:
        raise RuntimeError('Refusing to modify an active live service')
    if LIVE.exists() or UNIT.exists():
        raise RuntimeError('Live directory or unit already exists; inspect it before staging')
    if stat.S_IMODE(SOURCE_CREDENTIALS.stat().st_mode) & 0o077:
        raise RuntimeError('Live credential permissions are too broad')
    credentials = SOURCE_CREDENTIALS.read_text(encoding='utf-8')
    fields = dict(
        line.split('=', 1)
        for line in credentials.splitlines()
        if '=' in line and not line.lstrip().startswith('#')
    )
    required = ('PRIVATE_KEY', 'FUNDER_ADDRESS', 'POLYMARKET_RELAYER_API_KEY',
                'POLYMARKET_RELAYER_API_KEY_ADDRESS', 'CLOB_SIGNATURE_TYPE')
    if any(not fields.get(name, '').strip() for name in required):
        raise RuntimeError('Separate live credentials are incomplete')
    if 'DASHBOARD_TOKEN' in fields:
        raise RuntimeError('Dashboard token must be generated separately for live')
    profile = tomllib.loads(SOURCE_CONFIG.read_text(encoding='utf-8'))
    if not profile['targets']['wallets'] or profile['sizing']['mode'] != 'bank_scaled':
        raise RuntimeError('Live profile is incomplete')
    account = pwd.getpwnam('polycopier')
    LIVE.mkdir(mode=0o700)
    os.chown(LIVE, account.pw_uid, account.pw_gid)
    token = secrets.token_urlsafe(36)
    env = credentials.rstrip() + '\nDASHBOARD_TOKEN=' + token + '\n'
    write_new(LIVE / '.env', env.encode(), 0o600, account.pw_uid, account.pw_gid)
    write_new(LIVE / 'config.toml', SOURCE_CONFIG.read_bytes(), 0o600,
              account.pw_uid, account.pw_gid)
    write_new(LIVE / 'polycopier', SOURCE_BINARY.read_bytes(), 0o750,
              account.pw_uid, account.pw_gid)
    write_new(UNIT, SOURCE_UNIT.read_bytes(), 0o644, 0, 0)
    subprocess.run(['systemd-analyze', 'verify', str(UNIT)], check=True)
    subprocess.run(['systemctl', 'daemon-reload'], check=True)
    if subprocess.run(['systemctl', 'is-active', '--quiet', 'polycopier']).returncode == 0:
        raise RuntimeError('Live service unexpectedly became active')
    print('Isolated live directory and stopped service staged; no trading started.')


if __name__ == '__main__':
    main()
