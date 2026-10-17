"""Bounded maintenance for /opt/polycopier; never touches account state or donor data."""
import argparse
import datetime
import fcntl
import hashlib
import json
import os
import pathlib
import re
import shutil
import subprocess
import tempfile

ROOT = pathlib.Path('/opt/polycopier')
BUILD_DIRS = ('debug', 'tmp', 'release/deps', 'release/build', 'release/.fingerprint', 'release/incremental', 'release/examples')
BUILD_IMAGES = ('114c7a4425406451c2866b6aafe69fe29b1b298832db1277d411ac73c82d04d6',
                '3914072ca0c3b8aad871db9169a651ccfce30cf58303e5d6f2db16d1d8a7e58f',
                'd6aa754f16b3197301076f047b5def2f02ea1dbbc2ca920407d46d7ec7f87b20')

def checked(path, root):
    if path.is_symlink() or root.is_symlink():
        raise RuntimeError('Refusing symlink: ' + str(path))
    resolved = path.resolve()
    if not resolved.is_relative_to(root.resolve()) or resolved == root.resolve():
        raise RuntimeError('Refusing path outside cleanup root: ' + str(path))
    return resolved

def size(path):
    if not path.exists(): return 0
    return sum(p.stat().st_size for p in path.rglob('*') if p.is_file() and not p.is_symlink())

def clean_build(target):
    # Preflight every top-level target before deleting anything.
    candidates = [checked(target / name, target) for name in BUILD_DIRS if (target / name).exists() or (target / name).is_symlink()]
    binary = target / 'release/polycopier'
    before = hashlib.sha256(binary.read_bytes()).hexdigest() if binary.exists() else None
    for path in candidates:
        if not path.is_dir(): raise RuntimeError('Expected cache directory: ' + str(path))
    for path in candidates: shutil.rmtree(path)
    after = hashlib.sha256(binary.read_bytes()).hexdigest() if binary.exists() else None
    if before != after: raise RuntimeError('Runtime binary changed during cache cleanup')
    return [str(p) for p in candidates]

def old_backups(base):
    if not base.exists(): return []
    # Experiment-reset milestones and unknown directories remain protected.
    entries = sorted((p for p in base.iterdir() if p.is_dir() and not p.is_symlink()
                      and re.fullmatch(r'\d{8}T\d{6}Z(?:-fresh-polling)?', p.name)), key=lambda p: p.name, reverse=True)
    return [checked(p, base) for p in entries[3:]]

def command(args):
    return subprocess.run(args, text=True, capture_output=True, timeout=60)

def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--apply', action='store_true')
    args = parser.parse_args()
    if ROOT.resolve() != ROOT or ROOT.is_symlink(): raise RuntimeError('Unexpected project path')
    with (ROOT / '.build.lock').open('a') as lock:
        try: fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            print('Storage maintenance skipped: build is running')
            return
        running = command(['docker', 'ps', '-q', '--filter', 'name=polycopier-build'])
        if running.returncode or running.stdout.strip():
            print('Storage maintenance skipped: build-container state is not safely idle')
            return
        disk_before = shutil.disk_usage(ROOT)
        target = checked(ROOT / 'target', ROOT)
        cache_before = size(target)
        actions = []
        if args.apply:
            if cache_before > 256 * 1024**2:
                actions.extend(clean_build(target))
            for backup in old_backups(ROOT / 'deploy-backups'):
                shutil.rmtree(backup)
                actions.append('obsolete backup: ' + str(backup))
            for digest in BUILD_IMAGES:
                image = command(['docker', 'image', 'inspect', '--format', '{{.Id}}', 'sha256:' + digest])
                if image.returncode: continue
                image_id = image.stdout.strip()
                users = command(['docker', 'ps', '-aq', '--filter', 'ancestor=' + image_id])
                if users.returncode or users.stdout.strip(): continue
                removed = command(['docker', 'image', 'rm', '--no-prune', image_id])
                if removed.returncode == 0: actions.append('unused build image: ' + image_id)
            rotation = command(['logrotate', '--state', '/var/lib/polycopier-storage/logrotate.status', '/etc/logrotate.d/polycopier'])
            if rotation.returncode: raise RuntimeError('Log rotation failed: ' + rotation.stderr)
        disk_after = shutil.disk_usage(ROOT)
        warning = disk_after.used / disk_after.total >= .80 or disk_after.free < 20 * 1024**3
        report = {'checked_at_utc': datetime.datetime.now(datetime.timezone.utc).isoformat(),
                  'applied': args.apply, 'disk_used_percent': round(100 * disk_after.used / disk_after.total, 2),
                  'disk_free_bytes': disk_after.free, 'disk_reclaimed_bytes': disk_after.free - disk_before.free,
                  'build_cache_before_bytes': cache_before, 'build_cache_after_bytes': size(target),
                  'project_bytes': size(ROOT), 'actions': actions, 'warning': warning,
                  'protected': ['paper account state', 'copy ledger', 'risk state', 'credentials', 'running binary', 'experiment-reset milestones', 'other VPS projects']}
        if args.apply:
            with tempfile.NamedTemporaryFile(mode='w', dir=ROOT, prefix='.storage-status-', delete=False) as stream:
                json.dump(report, stream, indent=2)
                stream.write('\n')
                temporary = pathlib.Path(stream.name)
            os.replace(temporary, ROOT / 'storage-status.json')
        print(json.dumps(report, indent=2))
        if warning: raise RuntimeError('Disk alert: usage >=80% or free space <20 GiB')

if __name__ == '__main__': main()
