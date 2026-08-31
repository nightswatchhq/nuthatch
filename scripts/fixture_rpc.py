#!/usr/bin/env python3
"""A scripted, controllable JSON-RPC endpoint for the cross-nest adoption acceptance run
(docs/verification.md, GH #569).

Deterministic, no network, stdlib only. It plays the same "same headers, different logs" chain
tests/common/tape.rs uses in-process, except over real HTTP - so the *actual* nuthatch binary can be
pointed at it with `--rpc` / `rpc_urls`, not just cargo test.

Two block-hash-and-timestamp-stable but log-divergent modes, switched live via /control/mode:
  full    - blocks 1..HISTORY_TIP each carry one ERC-20 Transfer log on CONTRACT.
  pruned  - the exact same block headers (same hashes, same timestamps - the chain still "walks"),
            and zero logs anywhere, ever. A provider that kept the chain but dropped its receipts.

Control surface (not JSON-RPC, plain POST/GET so a docs walkthrough can `curl` it directly):
  POST /control/mode        {"mode": "full"|"pruned"}
  POST /control/tip         {"number": N}            - eth_blockNumber / "latest"
  POST /control/finalized   {"number": N}            - eth_getBlockByNumber("finalized", ...)
  POST /control/reset_calls                            - clear the eth_getLogs call log
  GET  /control/calls                                  - every eth_getLogs call since the last reset
  GET  /control/state                                  - current mode/tip/finalized, for debugging

Deliberately not a general-purpose chain simulator - just enough of `eth_chainId`, `eth_blockNumber`,
`eth_getBlockByNumber` and `eth_getLogs` to run one contract, one event, over a small range, which is
all the acceptance run needs.
"""
import argparse
import hashlib
import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

CHAIN_ID = 42161  # arbitrum-one, per src/chains.rs
BASE_TS = 1_700_000_000
HISTORY_TIP = 8  # blocks 1..=HISTORY_TIP each carry one Transfer in "full" mode
TRANSFER_TOPIC0 = "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef"


def block_hash(n: int) -> str:
    # A pure function of height alone - identical in both modes, so "full" and "pruned" are
    # genuinely the same walkable chain and only their logs differ.
    return "0x" + hashlib.sha256(f"nuthatch-fixture-block:{n}".encode()).hexdigest()


def topic_address(addr: str) -> str:
    h = addr.removeprefix("0x")
    return "0x" + h.rjust(64, "0")


class State:
    def __init__(self, contract: str):
        self.lock = threading.Lock()
        self.contract = contract.lower()
        self.mode = "full"
        self.tip = HISTORY_TIP
        self.finalized = 0
        self.calls = []  # eth_getLogs calls since the last reset: {"from": int, "to": int}


STATE: State = None  # set in main()


def block_header(n: int) -> dict:
    return {
        "number": hex(n),
        "hash": block_hash(n),
        "parentHash": block_hash(n - 1) if n > 0 else "0x" + "00" * 32,
        "timestamp": hex(BASE_TS + n),
        "logsBloom": "0x" + "00" * 256,
    }


def resolve_tag(tag) -> int:
    with STATE.lock:
        tip, finalized = STATE.tip, STATE.finalized
    if isinstance(tag, str) and not tag.startswith("0x"):
        if tag in ("latest", "pending"):
            return tip
        if tag == "finalized":
            return finalized
        if tag == "earliest":
            return 0
    return int(tag, 16)


def transfer_log(n: int) -> dict:
    return {
        "address": STATE.contract,
        "topics": [
            TRANSFER_TOPIC0,
            topic_address("0x" + "1" * 40),
            topic_address("0x" + "2" * 40),
        ],
        "data": "0x" + (100 * n).to_bytes(32, "big").hex(),
        "blockNumber": hex(n),
        "blockHash": block_hash(n),
        "transactionHash": "0x" + hashlib.sha256(f"tx:{n}".encode()).hexdigest(),
        "logIndex": "0x0",
    }


def handle_call(method: str, params: list):
    if method == "eth_chainId":
        return hex(CHAIN_ID)
    if method == "eth_blockNumber":
        with STATE.lock:
            return hex(STATE.tip)
    if method == "eth_getBlockByNumber":
        n = resolve_tag(params[0])
        return block_header(n)
    if method == "eth_getLogs":
        f = params[0]
        from_block = int(f.get("fromBlock", "0x0"), 16)
        to_block = int(f.get("toBlock", "0x0"), 16)
        with STATE.lock:
            STATE.calls.append({"from": from_block, "to": to_block})
            mode = STATE.mode
        if mode == "pruned":
            return []
        addresses = {a.lower() for a in f.get("address", [])} if f.get("address") else None
        out = []
        for n in range(max(from_block, 1), min(to_block, HISTORY_TIP) + 1):
            if addresses is None or STATE.contract in addresses:
                out.append(transfer_log(n))
        return out
    raise ValueError(f"unsupported method {method}")


class Handler(BaseHTTPRequestHandler):
    def log_message(self, fmt, *args):
        pass  # the /control/calls log is the record that matters here

    def _read_json(self):
        length = int(self.headers.get("content-length", 0))
        return json.loads(self.rfile.read(length) or b"{}")

    def _reply(self, status: int, body: dict):
        payload = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def do_POST(self):
        if self.path.startswith("/control/"):
            return self._control_post()
        body = self._read_json()
        if isinstance(body, list):
            out = []
            for req in body:
                try:
                    result = handle_call(req["method"], req.get("params", []))
                    out.append({"jsonrpc": "2.0", "id": req.get("id"), "result": result})
                except Exception as e:  # noqa: BLE001 - a JSON-RPC error object, not a 500
                    out.append(
                        {"jsonrpc": "2.0", "id": req.get("id"), "error": {"code": -32000, "message": str(e)}}
                    )
            return self._reply(200, out)
        try:
            result = handle_call(body["method"], body.get("params", []))
            return self._reply(200, {"jsonrpc": "2.0", "id": body.get("id"), "result": result})
        except Exception as e:  # noqa: BLE001
            return self._reply(200, {"jsonrpc": "2.0", "id": body.get("id"), "error": {"code": -32000, "message": str(e)}})

    def _control_post(self):
        body = self._read_json()
        if self.path == "/control/mode":
            with STATE.lock:
                STATE.mode = body["mode"]
            return self._reply(200, {"mode": STATE.mode})
        if self.path == "/control/tip":
            with STATE.lock:
                STATE.tip = int(body["number"])
            return self._reply(200, {"tip": STATE.tip})
        if self.path == "/control/finalized":
            with STATE.lock:
                STATE.finalized = int(body["number"])
            return self._reply(200, {"finalized": STATE.finalized})
        if self.path == "/control/reset_calls":
            with STATE.lock:
                STATE.calls = []
            return self._reply(200, {"reset": True})
        return self._reply(404, {"error": f"no such control endpoint {self.path}"})

    def do_GET(self):
        if self.path == "/control/calls":
            with STATE.lock:
                return self._reply(200, {"calls": list(STATE.calls)})
        if self.path == "/control/state":
            with STATE.lock:
                return self._reply(
                    200,
                    {"mode": STATE.mode, "tip": STATE.tip, "finalized": STATE.finalized, "contract": STATE.contract},
                )
        return self._reply(404, {"error": f"no such control endpoint {self.path}"})


def main():
    global STATE
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=8945)
    ap.add_argument("--contract", required=True)
    # Startup-settable tip/finality, and a bindable address. All three default to today's behaviour,
    # so the existing callers (scripts/cross-nest-adoption.sh, tests/authoring_eval_board.rs) are
    # untouched - they pin via /control/* after start, which still works.
    #
    # These exist for the authoring eval's enforced-isolation mode (#1050): the subject runs on an
    # `--internal` Docker network so it has no route to the internet, which also means the runner on
    # the host has no route to *it*. The control endpoints are therefore unreachable there, and the
    # chain has to arrive already pinned.
    ap.add_argument("--tip", type=int, default=None, help="initial eth_blockNumber / latest")
    ap.add_argument("--finalized", type=int, default=None, help="initial finalized block")
    ap.add_argument("--bind", default="127.0.0.1",
                    help="address to listen on; 0.0.0.0 to be reachable from a container")
    args = ap.parse_args()
    STATE = State(args.contract)
    if args.tip is not None:
        STATE.tip = args.tip
    if args.finalized is not None:
        STATE.finalized = args.finalized
    server = ThreadingHTTPServer((args.bind, args.port), Handler)
    print(f"fixture-rpc listening on http://{args.bind}:{args.port}/ contract={STATE.contract} "
          f"tip={STATE.tip} finalized={STATE.finalized}", flush=True)
    server.serve_forever()


if __name__ == "__main__":
    main()
