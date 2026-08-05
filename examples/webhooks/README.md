# Webhooks and alerts, end to end

Two delivery paths share one engine. This walks both, with a receiver you can actually run.

| | `[[webhooks]]` (RFC-0010 Part B) | `[[alerts]]` (RFC-0008 C5) |
|---|---|---|
| Sends | rows of an event table | compliance annotations |
| Triggered by | rows sealing (or hitting the tip) | a flag being raised or retracted |
| Selected by | `table` + optional `where` | `kinds` |
| Signed | when `secret` is set | when the named webhook carries a secret |

Both go through the same durable outbox, so everything below about retries, ordering and
the depth gauge applies to both.

## Run it

Start the receiver:

```sh
python3 examples/webhooks/receiver.py --secret hunter2
```

Point a nest at it, in `nuthatch.toml`:

```toml
[[webhooks]]
name   = "large-transfers"
table  = "token__transfer"
where  = "value_dec > 1000000"      # optional; SQL over that table's columns
url    = "http://127.0.0.1:8099/"
secret = "hunter2"                  # enables X-Nuthatch-Signature
finality = "sealed"                 # default; "tip" is faster and may retract
since  = "registration"             # default; "genesis" or a block number
batch_max = 50
```

Then `nuthatch dev --dir .` and wait for rows to seal. The receiver prints each payload and
whether the signature verified.

For alerts, the shape is smaller because the selector is the annotation kind:

```toml
[[alerts]]
kinds = ["sanction_hit", "threshold_flag"]
url   = "http://127.0.0.1:8099/"
```

## The four things that surprise people

**`since = "registration"` is the default, and it is the one you want.** A webhook added to a
nest that is about to run `--seal-direct` over 40M blocks does *not* fire for all of history:
the cursor starts where you registered it. Set `"genesis"` only if you mean it, and expect the
receiver to be hit hard.

**`finality = "sealed"` never lies; `"tip"` is faster and can retract.** Sealed rows are past
finality, so a delivery is final. Tip deliveries arrive sooner and a reorg can retract one you
already acted on. Pick per webhook, and if you are writing to a ledger, pick sealed.

**Delivery is at-least-once, so make your handler idempotent.** A non-2xx leaves the entry in
the outbox and it retries. If your handler succeeds and *then* fails to reply 200, you will see
that payload again. Key on something stable from the row rather than counting deliveries.

**Verify the signature against the raw body.** nuthatch signs the exact bytes it sends. Parsing
the JSON and re-encoding it to check the HMAC produces different bytes and fails for reasons
that look like a nuthatch bug and are not. See `verify()` in `receiver.py` — twelve lines, and
the only part of that file worth copying.

## Signature scheme

```
X-Nuthatch-Signature: sha256=<hex>
```

HMAC-SHA256 (RFC 2104) over the raw request body, keyed by the webhook's `secret`. It is plain
HMAC despite being hand-rolled in `src/webhooks.rs`, verified here against RFC 4231 vectors and
across every key-length branch including keys longer than the 64-byte block, so your language's
standard library will agree with it. No timestamp is included, so the signature proves origin
and integrity but not freshness — if you need replay protection, dedupe on row identity.

## When it is not arriving

Check the outbox depth before checking anything else:

```sh
curl -s localhost:8288/metrics | grep nuthatch_alert_outbox_depth
```

| Depth | Meaning |
|---|---|
| 0 and rows are sealing | delivered; look at your receiver |
| rising | a sink is down or slow, and it is retrying |
| pinned near 10,000 | at `OUTBOX_MAX`; the oldest undelivered alerts are being shed, loudly |

A dead sink cannot grow the outbox without bound — past `OUTBOX_MAX` the oldest entries are
dropped rather than consuming the disk. That is a deliberate trade and it means a long enough
outage loses alerts. If that matters, alert on the gauge rising, not on the loss.

The numbers behind that, from `src/alerts.rs`, so you can reason about a backlog rather than
guess at it:

| | |
|---|---|
| `OUTBOX_MAX` | 10,000 entries before the oldest are shed |
| `DELIVERY_BATCH` | 100 deliveries attempted per drain |
| `DELIVERY_CONCURRENCY` | 8 POSTs in flight, so one slow sink does not throttle the rest |
| `POLL_INTERVAL` | 2s between drains, and the retry backoff is the same 2s (constant, not exponential) |
| `REQUEST_TIMEOUT` | 10s, so a hanging endpoint cannot wedge the worker |

A failed delivery does not stop the drain, and the backoff being constant means a sink that
comes back is caught up promptly rather than waiting out a doubling window.
