#!/usr/bin/env python3
"""Render a cli2 ClientConfig + supervisor.toml from a LIVE chain binding.

Reads operator-box's `/mesh/inventory` and turns one existing `chain_binding`
into the two files the cli2 pair needs. Nothing is invented here: every hop's
address, camouflage and pubkey comes from the live peer table, and the selector
tokens come from the binding itself — so a config that renders is a config the
mesh currently agrees with, and one that cannot render says which fact is
missing instead of dialling a guess.

It BUILDS NOTHING on the fleet. Use an existing chain id from `GET /chains`.

    python mirage2app/scripts/render-client-config.py \
        --chain auto-0837b318 --out /tmp/stage0 \
        --worker-binary /opt/mirage/mirage-chain-worker \
        --tokens /opt/mirage/capability-tokens.txt

0x02 note: the fleet runs SignedCapability, which the client cannot mint. The
rendered config points `capability_tokens_path` at --tokens; fill that file
with one hex token per line from operator-box `POST /credential/issue`, or the
exit refuses every dial by design.
"""

import argparse
import json
import os
import pathlib
import sys
import urllib.request

OPBOX = "http://127.0.0.1:19460"
DEFAULT_KEY = os.path.expanduser("~/.mirage-operator/api-key.raw")


def fetch(path: str, key: str) -> dict:
    req = urllib.request.Request(
        OPBOX + path, headers={"Authorization": "Bearer " + key}
    )
    with urllib.request.urlopen(req, timeout=40) as r:
        return json.load(r)


def collect_peers(inv: dict) -> dict:
    """node_id -> peer row, merged over every machine's view (newest wins)."""
    peers = {}
    for m in inv.get("machines", []):
        block = m.get("peers") or {}
        for row in block.get("peers", []):
            nid = row.get("node_id")
            if not nid:
                continue
            prev = peers.get(nid)
            if prev is None or row.get("updated_at_ms", 0) >= prev.get("updated_at_ms", 0):
                peers[nid] = row
    return peers


def find_binding(inv: dict, chain_id: str) -> dict:
    for m in inv.get("machines", []):
        for entry in (m.get("chain_bindings") or {}).get("chains", []):
            b = entry.get("binding") or {}
            if b.get("chain_id") == chain_id:
                return b
    raise SystemExit(f"chain {chain_id} is in no machine's bindings — pick one from GET /chains")


def hop(peer_row: dict, node_id: str, role: str, selector: str | None) -> dict:
    face = peer_row.get("chain_face")
    if not face:
        raise SystemExit(
            f"node {node_id} publishes no chain face — the box has not been through "
            f"persona apply, or gossip has not carried it yet"
        )
    camo = face.get("camouflage") or {}
    h = {
        "addr": f"{peer_row['addr']}:{face['port']}",
        "public_key": peer_row.get("pubkey_hex"),
        "node_id": node_id,
        "role": role,
        "camouflage": camo,
    }
    if camo.get("type") == "tls_real" and camo.get("sni"):
        h["sni"] = camo["sni"]
    if selector:
        h["selector_token_hex"] = selector
    # HopConfig.relay_mode has no default and no shim: every hop must declare
    # it. OnionPeel is the only surviving variant (Transparent and CryptoVerify
    # were removed 2026-09-02), so the binding's own leg value and this are the
    # same thing; the exit's leg is not in `legs[]`, which is why it is spelled
    # rather than copied.
    h["relay_mode"] = {"mode": "onion_peel"}
    return h


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--chain", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--client-id", default="stage0-lab")
    ap.add_argument("--socks5", default="127.0.0.1:19480")
    ap.add_argument("--worker-binary", required=True)
    ap.add_argument("--tokens", required=True, help="path ON THE CLIENT BOX to the 0x02 token file")
    ap.add_argument("--key-file", default=DEFAULT_KEY)
    args = ap.parse_args()

    key = pathlib.Path(args.key_file).read_text(encoding="utf-8").strip()
    inv = fetch("/mesh/inventory", key)
    binding = find_binding(inv, args.chain)
    peers = collect_peers(inv)

    hops = []
    legs = binding.get("legs") or []
    for i, leg in enumerate(legs):
        nid = leg["node_id"]
        if nid not in peers:
            raise SystemExit(f"leg node {nid} is in no peer table")
        hops.append(hop(peers[nid], nid, "entry" if i == 0 else "middle", leg.get("selector_token_hex")))
    ex = binding["exit"]
    nid = ex["node_id"]
    if nid not in peers:
        raise SystemExit(f"exit node {nid} is in no peer table")
    hops.append(hop(peers[nid], nid, "exit", ex.get("selector_token_hex")))

    cfg = {
        "client_id": args.client_id,
        "mesh_id": binding["mesh_id"],
        "chains": [
            {
                "chain_id": binding["chain_id"],
                "hops": hops,
                "handshake": binding.get("handshake", "telescopic"),
                "reply": binding.get("onion_reply_mode", "onion"),
                "protocol": "Mirage",
                "priority": 10,
                "socks5_bind": args.socks5,
            }
        ],
        "routing": {},
        "capability_tokens_path": args.tokens,
    }

    out = pathlib.Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    cfg_path = out / f"client-config-{binding['chain_id']}.json"
    cfg_path.write_text(json.dumps(cfg, indent=2), encoding="utf-8")

    sup = out / f"supervisor-{binding['chain_id']}.toml"
    sup.write_text(
        "\n".join(
            [
                "# monitor_url and ota_pubkey are REQUIRED fields of the supervisor's",
                "# Config with no defaults, and both are legacy: monitor_url feeds the",
                "# OTA poll and an HTTP peer-table bootstrap against a control plane that",
                "# is wire-only now, and its failure is a warning, not an error. Every hop",
                "# below carries an explicit public_key, so an empty bootstrap costs",
                "# nothing here.",
                'monitor_url = "http://127.0.0.1:19410"',
                'ota_pubkey = "unused-in-this-harness"',
                f'worker_binary = "{args.worker_binary}"',
                "",
                "[[chains]]",
                f'id = "{binding["chain_id"]}"',
                f'socks5_bind = "{args.socks5}"',
                f'config_path = "{cfg_path.name}"',
                "",
            ]
        ),
        encoding="utf-8",
    )

    print(f"chain   : {binding['chain_id']} ({binding['mesh_id']}, {len(hops)} hops)")
    for h in hops:
        print(f"  {h['role']:<6} {h['node_id']}  {h['addr']}  {h['camouflage'].get('type')}")
    print(f"config  : {cfg_path}")
    print(f"supervisor: {sup}")
    print("tokens  : fill", args.tokens, "with 0x02 tokens or every dial is refused at the exit")


if __name__ == "__main__":
    sys.exit(main())
