"""Read-only readiness gate for the isolated $1,000 live profile."""

import importlib.util
import base64
import hashlib
import json
import os
import pathlib
import pwd
import re
import stat
import subprocess
import sys
import tomllib
import urllib.request
from decimal import Decimal

ROOT = pathlib.Path('/opt/polycopier')
LIVE = pathlib.Path('/opt/polycopier-live')
UNIT = pathlib.Path('/etc/systemd/system/polycopier.service')
BANK = Decimal(1000)


def preflight_status(returncode: int, stdout: str) -> tuple[bool, str | None, bool]:
    balance = re.search(r'Available CLOB USDC: \$(\d+(?:\.\d+)?)', stdout)
    available = balance.group(1) if balance else None
    connected = returncode == 0 and available is not None
    funded = connected and Decimal(available) >= BANK
    return connected, available, funded


def source_sample_sufficient(capacity: dict) -> bool:
    return (
        capacity.get('lots_with_source_notional', 0) >= 20
        and capacity.get('source_lots_meeting_five_share_minimum_at_paper_price', 0) >= 20
        and Decimal(capacity.get('source_five_share_feasibility_pct', '0')) >= Decimal('80')
        and capacity.get('known_source_sample_span_seconds', 0) >= 86400
        and capacity.get('known_source_peak_with_reserve_fits_bank', False)
    )


def call(*args: str) -> subprocess.CompletedProcess:
    return subprocess.run(args, capture_output=True, text=True, timeout=35)


def main() -> None:
    prestart = sys.argv[1:] == ['--prestart']
    if sys.argv[1:] and not prestart:
        raise SystemExit('Usage: verify-live-stage.py [--prestart]')
    report = {}
    owner = pwd.getpwnam('polycopier').pw_uid
    env = LIVE / '.env'
    profile_file = LIVE / 'config.toml'
    binary = LIVE / 'polycopier'
    active_state = call('systemctl', 'show', '--value', '-p', 'ActiveState', 'polycopier')
    enabled_state = call('systemctl', 'show', '--value', '-p', 'UnitFileState', 'polycopier')
    load_state = call('systemctl', 'show', '--value', '-p', 'LoadState', 'polycopier')
    fragment_path = call('systemctl', 'show', '--value', '-p', 'FragmentPath', 'polycopier')
    report['service_stopped'] = (
        active_state.returncode == 0
        and active_state.stdout.strip() in (('inactive', 'activating') if prestart else ('inactive',))
    )
    report['service_disabled'] = (
        enabled_state.returncode == 0
        and enabled_state.stdout.strip() in (('disabled', 'enabled') if prestart else ('disabled',))
    )
    report['expected_unit_loaded'] = (
        load_state.returncode == 0 and load_state.stdout.strip() == 'loaded'
        and fragment_path.returncode == 0 and fragment_path.stdout.strip() == str(UNIT)
    )
    report['paper_active'] = call('systemctl', 'is-active', '--quiet', 'polycopier-preview').returncode == 0
    report['isolated_files'] = (
        LIVE.is_dir() and stat.S_IMODE(LIVE.stat().st_mode) == 0o700
        and all(path.is_file() and path.stat().st_uid == owner for path in (env, profile_file, binary))
        and stat.S_IMODE(env.stat().st_mode) == 0o600
        and stat.S_IMODE(profile_file.stat().st_mode) == 0o600
        and os.access(binary, os.X_OK)
        and UNIT.read_bytes() == (ROOT / 'deploy/polycopier.service').read_bytes()
    ) if all(path.exists() for path in (LIVE, env, profile_file, binary, UNIT)) else False
    built_binary = ROOT / 'target/release/polycopier'
    if binary.is_file() and built_binary.is_file():
        with binary.open('rb') as staged, built_binary.open('rb') as built:
            report['staged_binary_matches_build'] = (
                hashlib.file_digest(staged, 'sha256').digest()
                == hashlib.file_digest(built, 'sha256').digest()
            )
    else:
        report['staged_binary_matches_build'] = False
    fields = dict(line.split('=', 1) for line in env.read_text().splitlines()
                  if '=' in line and not line.lstrip().startswith('#')) if env.is_file() else {}
    preview_env = ROOT / '.env'
    preview_fields = dict(line.split('=', 1) for line in preview_env.read_text().splitlines()
                          if '=' in line and not line.lstrip().startswith('#')) if preview_env.is_file() else {}
    report['dashboard_auth'] = len(fields.get('DASHBOARD_TOKEN', '')) >= 24
    identity_fields = ('PRIVATE_KEY', 'FUNDER_ADDRESS',
                       'POLYMARKET_RELAYER_API_KEY', 'POLYMARKET_RELAYER_API_KEY_ADDRESS')
    report['separate_credentials'] = (
        all(fields.get(name) for name in identity_fields)
        and all(not preview_fields.get(name)
                or fields[name].strip('"\'').lower() != preview_fields[name].strip('"\'').lower()
                for name in identity_fields)
        and bool(fields.get('CLOB_SIGNATURE_TYPE'))
    )
    profile = tomllib.loads(profile_file.read_text()) if profile_file.is_file() else {}
    report['staged_profile_matches'] = (
        profile_file.is_file()
        and profile_file.read_bytes() == (ROOT / 'deploy/config.live-1000.toml').read_bytes()
        and profile.get('sizing', {}).get('mode') == 'bank_scaled'
        and len(profile.get('targets', {}).get('wallets', [])) == 1
    )
    report['selected_leader_is_top'] = False
    report['selected_leader_has_evidence'] = False
    report['leader_current_rating'] = None
    report['leader_closed_entries'] = None
    try:
        bind = preview_fields.get('DASHBOARD_BIND', '127.0.0.1:3000')
        if not bind.startswith('127.0.0.1:') or not preview_fields.get('DASHBOARD_TOKEN'):
            raise ValueError('Paper dashboard authentication unavailable')
        auth = base64.b64encode(
            ('polycopier:' + preview_fields['DASHBOARD_TOKEN']).encode()
        ).decode()
        request = urllib.request.Request(
            'http://' + bind + '/api/state', headers={'Authorization': 'Basic ' + auth}
        )
        with urllib.request.urlopen(request, timeout=15) as response:
            paper_state = json.load(response)
        ranked = paper_state.get('target_performance', [])
        selected = profile.get('targets', {}).get('wallets', [''])[0].split(':', 1)[0].lower()
        leader = ranked[0] if ranked else {}
        report['selected_leader_is_top'] = leader.get('address', '').lower() == selected
        report['leader_current_rating'] = leader.get('rating')
        report['leader_closed_entries'] = leader.get('closed_entries')
        report['selected_leader_has_evidence'] = (
            report['selected_leader_is_top']
            and int(leader.get('rating') or 0) >= 4
            and int(leader.get('closed_entries') or 0) >= 20
            and Decimal(str(leader.get('roi_pct') or 0)) > 0
        )
    except (OSError, ValueError, KeyError, IndexError, json.JSONDecodeError):
        pass
    unit_workdir = call('systemctl', 'show', '--value', '-p', 'WorkingDirectory', 'polycopier').stdout.strip()
    report['unit_uses_live_directory'] = unit_workdir == str(LIVE)
    source = ROOT / 'deploy/analyze-live-capacity.py'
    spec = importlib.util.spec_from_file_location('capacity', source)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    with (ROOT / 'sim_copy_ledger.json').open() as stream:
        ledger = json.load(stream)
    capacity = module.analyze(ledger, profile, BANK) if profile else {}
    report['historical_capacity_with_reserve'] = capacity.get('historical_peak_with_reserve_fits_bank', False)
    report['historical_peak_proxy_usd'] = capacity.get('peak_full_lot_proxy_usd')
    report['source_notional_lots'] = capacity.get('lots_with_source_notional', 0)
    report['observed_source_lots_with_notional'] = capacity.get('observed_source_lots_with_notional', 0)
    report['source_five_share_feasibility_pct'] = capacity.get('source_five_share_feasibility_pct', '0')
    report['source_lots_meeting_five_share_minimum_at_paper_price'] = capacity.get(
        'source_lots_meeting_five_share_minimum_at_paper_price', 0)
    report['source_lots_below_one_dollar_minimum'] = capacity.get(
        'source_lots_below_one_dollar_minimum', 0)
    report['legacy_proxy_lots'] = capacity.get('legacy_lots_using_paper_cost_proxy', 0)
    report['source_sample_span_hours'] = round(capacity.get('known_source_sample_span_seconds', 0) / 3600, 1)
    report['source_sample_capacity_with_reserve'] = capacity.get('known_source_peak_with_reserve_fits_bank', False)
    report['source_sample_sufficient'] = source_sample_sufficient(capacity)
    try:
        preflight_command = (str(ROOT / 'target/release/live_preflight'),
                             str(env), str(profile_file), '0')
        if os.geteuid() != owner:
            preflight_command = ('runuser', '-u', 'polycopier', '--', *preflight_command)
        preflight = subprocess.run(
            preflight_command,
            cwd=LIVE, capture_output=True, text=True, timeout=35,
        )
        connected, available, funded = preflight_status(
            preflight.returncode, preflight.stdout)
        report['clob_auth_and_geoblock'] = connected
        report['available_clob_usdc'] = available
        report['clob_collateral_sufficient'] = funded
    except subprocess.TimeoutExpired:
        report['clob_auth_and_geoblock'] = False
        report['available_clob_usdc'] = None
        report['clob_collateral_sufficient'] = False
    report['clob_auth_geoblock_and_funding'] = (
        report['clob_auth_and_geoblock'] and report['clob_collateral_sufficient'])
    ready = all(value is True for key, value in report.items() if isinstance(value, bool))
    report['ready_to_consider_activation'] = ready
    print(json.dumps(report, indent=2))
    if not ready:
        sys.exit(1)


if __name__ == '__main__':
    main()
