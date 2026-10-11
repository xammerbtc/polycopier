# Isolated VPS deployment

This deployment runs **DRY-RUN / SIMULATION**. The $200 balance and all
positions/PnL displayed by this deployment are paper values. No CLOB
authentication, signed orders, cancellations, or on-chain transactions occur.

## Installed layout

- Source/build/UI/state: `/opt/polycopier`
- Service account: `polycopier` (no login shell)
- Service: `polycopier-preview.service`
- Dashboard: `127.0.0.1:3000`, configurable through `DASHBOARD_BIND`
- Token: `/opt/polycopier/.env`, mode 0600
- Donor wallet credentials: `/opt/polycopier/.env.credentials`, root-only 0600;
  this file is **not loaded** by the preview service
- Public tunables: `config.toml`
- Paper state: `simulation_state.json` and `sim_copy_ledger.json`
- Daily volume / circuit-breaker state: `runtime-risk.json`
- Logs: journald and `polycopier.log`

The existing arbigab deployment has separate services, files and ports. Its
service lifecycle and risk settings are not changed by this installer.

## Build and install on Ubuntu

Prerequisites: Docker, Python 3, Node/npm, systemd, OpenSSL development/runtime
libraries appropriate to the Ubuntu host. Build in the Bookworm Rust image;
record its immutable digest and retain `Cargo.lock` for repeatable builds.

```bash
cd /opt/polycopier
bash deploy/build-vps.sh
# Only after tests, clippy, release build and frontend build succeed:
flock -n /opt/arbigab/operator_control/locks/production.lock bash deploy/install-preview.sh
sudo install -m 644 deploy/logrotate-preview /etc/logrotate.d/polycopier-preview
python3 deploy/verify-preview.py
```

Use the VPS's existing SSH connection to access the dashboard:

```bash
ssh -N -L 13000:127.0.0.1:3000 arbigab-vps
```

Open `http://127.0.0.1:13000`. Username: `polycopier`. Password is the
`DASHBOARD_TOKEN` stored on the VPS. Retrieve it through your own secure SSH
session; do not paste it into issues, commits, reports, or chat logs. Token
values are never returned by the dashboard API.

For a reverse proxy, keep the backend bound to localhost, use HTTPS, preserve
the Host header, and keep the API's Basic authentication enabled. Example:

```nginx
location / {
    proxy_pass http://127.0.0.1:3000;
    proxy_set_header Host $host;
    proxy_set_header X-Forwarded-Proto $scheme;
}
```

## Live settings

Wallets can include an optional scalar: `0xADDRESS:0.01`. A scalar of `0`
temporarily disables that wallet while keeping it in the configuration.
Set `scanner.max_entries_per_cycle=0` to disable catch-up entries while
continuing to display target portfolios. Target changes are consumed by polling
every cycle and RTDS heartbeat every five seconds. Risk/sizing/execution settings
are consumed before evaluating the next event. Config writes are atomic and
invalid settings are rejected with HTTP 400.

RTDS subscribes to the public `activity/trades` feed and filters target wallet
addresses locally. Polling remains active to cover outages. The combined stream
deduplicates by transaction, wallet, token, side, price and size, retaining
different token fills from one transaction. Heartbeats run every five seconds;
45 seconds without inbound messages triggers reconnect with bounded backoff.

The <1.5-second latency goal requires measured target fills. A connected socket
alone does not prove that latency, and target event timestamps have only
second-level precision. Observe actual events before making any latency claim.

## API endpoints

All paths except `/health` require Basic authentication.

| Method | Path | Purpose |
|---|---|---|
| GET | `/health` | Process status, execution mode, websocket connection, poll timestamp, active listener targets |
| GET | `/api/state` | Balance, paper/live mode, PnL, volume, positions, signal feed and source mapping |
| GET | `/api/config` | Public tunables and configured targets; JSON export |
| POST | `/api/config` | Validate and atomically save tunables, consumed without restarting |
| GET | `/api/env` | Public funder address and blank private-key field for compatibility |
| POST | `/api/control` | `{"action":"pause"}`, `stop`, `start`, or `resume` |
| GET | `/api/ledger` | Copy history and ledger export |
| GET | `/api/logs` | Last 200 file-log lines |
| GET | `/api/metrics` | Target-stream counters and last order latency observations |
| POST | `/api/redeem` | Settle paper positions, or explicitly enabled live redemption through the official SDK |
| GET | `/api/redemption` | Live credential status and nonsecret redemption journal |
| GET | `/api/executions` | Copier order IDs and durable submission intents |
| POST | `/api/action/restart` | Replace only the polycopier process, preserving CLI simulation flags |

The secret-writing `/api/env` POST is disabled. Credential changes belong in a
secure server-side workflow. Force Redeem in preview verifies a closed binary
market with exactly one winner through Gamma/CLOB, credits paper payout, persists
state, and closes ledger records. Lookup failures and ambiguous resolutions leave
positions unchanged. Repeated requests cannot pay out a removed position twice.
This preview never submits an on-chain redemption. The live SDK bridge uses
`@polymarket/client` 0.13.0 with viem 2.57.4 and an isolated, pinned Node 24
runtime installed by `deploy/install-redeemer.sh`. It supports the wallet and
protocol routes selected by the official SDK. It only calls redemption; it does
not create approvals, transfer funds elsewhere, or place orders.

Live Redeem requires live execution mode, `LIVE_REDEEM_ENABLED=true`, the signer
and funder, and `POLYMARKET_RELAYER_API_KEY` plus
`POLYMARKET_RELAYER_API_KEY_ADDRESS`. Keep these in a root-only environment file
consumed by systemd; never paste values into chat or commits. The Relayer key is
distinct from a CLOB API key. The donor credentials currently provide the
signer/funder only. Do not enable live without resolving shared-wallet order
ownership and signature-type compatibility.

The endpoint selects resolved holdings present in this copier's active ledger,
serializes requests, and persists intent before SDK submission. It waits for a
confirmed transaction hash before closing ledger records. An uncertain result
leaves `pending_reconciliation` in `redemption-journal.json` and blocks automatic
retry. Reconcile using the wallet's on-chain/Relayer history before any manual
change to that journal; deleting pending intent is not an idempotency mechanism.
Live execution remains unverified until credentials and on-chain preflight are
available. SDK mock tests prove call routing and confirmation/error handling,
not a successful live redemption.

Live CLOB startup preserves all wallet orders. Exact CLOB order IDs registered
in `execution-journal.json` are the only orders the copier watcher may cancel.
Tokens with another bot's open orders and holdings outside this copier's ledger
are excluded from execution. Intent is persisted before submitting a signed
order. Ambiguous network/persistence outcomes pause execution and block live
startup or resume until reconciled; reserved margin is not blindly refunded.
The donor is a Gnosis Safe: use `CLOB_SIGNATURE_TYPE=2` in its staging environment.
`0/eoa`, `1/proxy`, and `2/safe` are explicit supported selections.

After a successful build, root can run `target/release/live_preflight
/opt/polycopier/.env.credentials` to check CLOB authentication, the balance
endpoint, and reported IP trading availability. It does not submit, cancel, or
redeem orders and never prints credentials. This check does not certify live
order fills, approvals, gasless redemption, or a separate funded account.

Metrics distinguish websocket and polling observations. Fill timestamps have
one-second precision, so fill-to-dispatch is reported as an upper bound, while
receive-to-dispatch uses a monotonic clock. These are dispatch observations,
not confirmation of a live order accepted by the exchange. A null fill-latency
field means the latest dispatch has no matching observed target fill.
Acceptance metrics cover the submitter's completed acknowledgement, include
SDK preparation time, and carry `execution_mode`; paper acknowledgements are
not live exchange acceptances. HTTP clients are reused between metadata queries.
Synthetic catch-up and position-sweep orders are counted as dispatches but have
null fill-latency fields; their generated timestamps cannot prove RTDS latency.

## Dependency audit

The locked Rust dependency audit on 2026-10-11 found zero advisories classified
as vulnerabilities, plus five unmaintained-crate warnings and three soundness
warnings in `lru` 0.12.5/0.16.4 via ratatui/alloy-provider. The panic-safety
warning requires panicking key destructors; these consumers use Rect/Layout,
B256, and block-number keys. The ratatui cache does not use LRU IterMut. These
observations limit applicability; they do not erase upstream warnings. Replacing
those dependencies requires a separate compatibility upgrade. Frontend npm audit
after compatible updates reports zero vulnerabilities.

## Operations and rollback

```bash
systemctl status polycopier-preview
journalctl -u polycopier-preview --since '30 minutes ago'
curl -fsS http://127.0.0.1:3000/health
systemctl stop polycopier-preview
```

Graceful shutdown pauses new execution, allows up to 25 seconds for in-flight
simulation orders, then persists the paper state. Invalid or unreadable persisted
state fails startup instead of silently resetting balances. Configuration reload
preserves the execution mode and credentials.

Before an update, preserve the existing binary and `web/dist`, together with
the config and runtime state, in a private timestamped directory. Build and test
the update before restarting this service. Rollback restores that binary/UI and
restarts `polycopier-preview`; retain current ledgers and snapshots. Stop only
the new service if verification fails. Do not restart the existing trading bot.

Live trading has not been enabled or certified by this preview deployment.
Live startup completes authoritative wallet and open-order reads before starting
the strategy. Failed reads stop startup rather than imply an empty wallet.
Resolved holdings remain visible for redemption. Live SELL size is bounded by
confirmed copier fills; wallet refresh cannot enlarge that inventory using
another bot's purchases.

The `live_fills` decoder reads exact shares, collateral and fees from finalized
CTF Exchange V2 `OrderFilled` events for journal-owned order IDs. Regression
tests include a real public target-wallet receipt, duplicate events, partial
fills, foreign orders, finality and canonical-header checks. The read-only
`target/release/receipt_preflight` checks the public fixture against Polygon.
Set `POLYGON_RPC_URL` for a different Polygon provider if needed. No credentials
are loaded by that preflight. This does not prove a copier order was executed.
Live startup and a five-second collector paginate authenticated CLOB trades,
select confirmed transactions for exact journal-owned order IDs, and verify
finalized Polygon receipts before recording `settlement-fills.json`. Atomic
ledger recovery uses the source wallet persisted before submission. The live
dashboard and unrealized PnL use this confirmed inventory, excluding other bots'
holdings. Collection errors or a snapshot older than 60 seconds block execution
and resume. Zero-filled entries close only after explicit CLOB CANCELED status.
Cancellation alone never counts as a loss.

Closed-position outcomes include actual collateral fees. The strategy replays
them into `runtime-risk.json` every two seconds and before each live signal.
Persisted outcome IDs prevent double-counting on restart; persistence failures
block execution. Partial sells do not increment the consecutive-loss counter.
Live SELL acceptance keeps the ledger open and does not book realized profit.
Confirmed fill and fee reconciliation remains required before enabling live
trading; wallet position refresh alone cannot establish execution prices or
attribute concurrent trades by other bots using the same wallet.

Use `target/release/relayer_preflight /opt/polycopier/.env.credentials` as root
to check the supplied Relayer API key with the authenticated read-only
`GET /transactions` endpoint. It prints no keys, wallet addresses or transaction
contents, and submits no transactions. Credentials remain root-only (0600).
Preview does not load them. A confirmed live redemption currently blocks fill
reconciliation until redemption payout accounting is completed; gasless Redeem
and actual copier order latency are not certified by these checks.
