"""Render the loopback-only dashboard in Chrome and detect blank or stalled UI."""

import json
import shutil
import subprocess
import tempfile


def main() -> None:
    chrome = shutil.which('google-chrome')
    if not chrome:
        raise SystemExit('google-chrome is required for the VPS browser smoke test')
    with tempfile.TemporaryDirectory(prefix='polycopier-browser-') as profile:
        result = subprocess.run(
            [chrome, '--headless=new', '--no-sandbox', '--disable-dev-shm-usage',
             '--no-proxy-server', '--disable-background-networking',
             '--no-first-run', f'--user-data-dir={profile}',
             '--virtual-time-budget=7000', '--dump-dom',
             'http://127.0.0.1:13000/'],
            capture_output=True, text=True, timeout=35,
        )
    page = result.stdout
    checks = {
        'chrome_exited_cleanly': result.returncode == 0,
        'app_rendered': len(page) > 5000 and 'Polycopier' in page,
        'financial_panel_rendered': 'Portfolio equity' in page,
        'positions_rendered': 'Target positions' in page,
        'skip_diagnostics_rendered': 'Signals skipped (all reasons)' in page
                                    and 'Recent skip reasons' in page,
        'no_loading_stall': 'Loading Control Center' not in page,
        'no_dashboard_timeout': 'Dashboard update delayed' not in page,
    }
    print(json.dumps({'checks': checks, 'rendered_dom_bytes': len(page)}, indent=2))
    if not all(checks.values()):
        raise SystemExit(1)


if __name__ == '__main__':
    main()
