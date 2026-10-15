#!/usr/bin/env python3
"""Read-only health and configuration check for the $20,000 paper run."""
import base64
import json
from pathlib import Path
import urllib.request


ROOT = Path("/opt/polycopier")
env = {}
for raw in (ROOT / ".env").read_text(encoding="utf-8").splitlines():
    if "=" in raw and not raw.lstrip().startswith("#"):
        key, value = raw.split("=", 1)
        env[key.strip()] = value.strip().strip("\"'")
auth = "Basic " + base64.b64encode(
    ("polycopier:" + env["DASHBOARD_TOKEN"]).encode()
).decode()


def get(path: str) -> dict:
    request = urllib.request.Request(
        "http://127.0.0.1:3000" + path,
        headers={"Authorization": auth},
    )
    with urllib.request.urlopen(request, timeout=10) as response:
        return json.load(response)


state = get("/api/state")
config = get("/api/config")
health = get("/health")
expected = {
    "0x2d2d2b32e5199edf833594a99b9c54e5668ab69d": ("1.0", "berlinskies"),
    "0x5268527977f700f9bf9b6d5cd843859e4e70135d": ("1.0", "HomeRunHazard"),
    "0xfe787d2da716d60e8acff57fb87eb13cd4d10319": ("1.0", "ferrariChampions2026"),
    "0x88a132c7b2d1901d783ce3307adb36c78428618d": ("1.0", "BYD77"),
    "0xb27baeb3c12d59bd6ca5abc637dec878d5267b22": ("1.0", "BYD777"),
    "0x23d81ba9371e576015c1e562db09c689f56b0288": ("1.0", "flawfence"),
    "0x88c4919de76e526d55a32c1f8afb439dd1f1129a": ("1.0", "Renan2030"),
    "0xe8381d4226f67bdd6fdc447c2406e4c0f5082b52": ("1.0", "erghr"),
}
targets = {}
for entry in config["targets"]["wallets"]:
    address, scalar, name = entry.split(":", 2)
    targets[address.lower()] = (scalar, name)

assert state["mode"] == "simulation" and not state["paused"]
assert state["starting_balance"] == "20000"
assert config["sizing"]["mode"] == "fixed"
assert float(config["execution"]["max_trade_size_usd"]) == 5.0
assert targets == expected, "Runtime target roster does not match the experiment roster"
assert float(config["scanner"]["max_entry_price"]) == 0.98
assert health["status"] == "ok" and health["websocket_connected"]
assert isinstance(state["positions"], dict)

print(
    json.dumps(
        {
            "mode": state["mode"],
            "paused": state["paused"],
            "starting_bank": state["starting_balance"],
            "paper_cash": state["total_balance"],
            "equity": state["equity"],
            "total_pnl": state["total_pnl"],
            "realized_pnl": state["realized_pnl"],
            "positions": len(state["positions"]),
            "priced_positions": state["priced_positions"],
            "unpriced_positions": len(state["positions"]) - state["priced_positions"],
            "closed_positions_waiting": sum(
                1
                for token, details in state["position_details"].items()
                if token in state["positions"] and details.get("closed")
            ),
            "sizing": config["sizing"]["mode"],
            "fixed_order_cap_usd": config["execution"]["max_trade_size_usd"],
            "targets": len(targets),
            "health": health["status"],
            "websocket_connected": health["websocket_connected"],
        },
        indent=2,
    )
)
