# RFC-0046: x402 at the counter - selling an operator's data to agents without a gated product

- Status: **Accepted in principle, design only.** The §8 governance question was put to Chief on
  2026-08-30 and answered: **we cross the line deliberately and use x402 wherever it fits.** The
  2026 feature freeze still applies, so nothing is built in nuthatch this year - but the argument in
  §8 is settled rather than open, and a future slice does not have to re-open it.
- Author: Pete (cargopete)
- Date: 2026-08-30
- Depends on: RFC-0045 (which recorded the absence this closes), RFC-0034 (the bounded surface is
  what makes a query priceable), RFC-0008 §C6 (the signing and content-addressing precedent),
  RFC-0016 §4 (the provenance stamp a buyer keeps).
- Origin: Graphtronauts, in the nuthatch 3.0 thread, 2026-08-29: *"Maybe users can also offer
  nuthatch data to agents using x402?"* RFC-0045 had established the day before that nuthatch has no
  x402 anywhere in the tree; this is the design that absence implies.
- Prior art in our own hands: `lodestar/src/lib/x402.ts` is a **working x402 buyer**, and
  `lodestar/src/lib/x402-seller.ts` is the seller half built and tested against it (17 tests,
  2026-08-30). Neither is in this repository and neither is wired to take money.

## Abstract

An agent that wants one answer from a nest currently has two options: run its own nest, or hold GRT,
fund escrow, and speak TAP through a Horizon data service. The first is right for a serious consumer
and the second is right for a Graph-native one. Neither is proportionate to *one question*, and the
gap is exactly where x402 sits: a `402` carrying a price, and a retry carrying a signed USDC
authorisation. One round trip, no accounts, no prior relationship.

This RFC argues **where such a counter may legitimately exist**, which is the harder half, and then
designs settlement so that no third party ever sits in the query path.

## 1. The non-negotiable this appears to violate, and why it does not

`CLAUDE.md` §3 reads, in full: *"No phone-home. No telemetry, no mandatory API tokens, no gated data
services."*

Taken carelessly, "no gated data services" forbids this RFC outright. Taken carefully it does not,
and the distinction is worth stating precisely because it is the whole licence to proceed.

**The rule binds the artefact, not the operator.** The thing we ship is a single binary that anybody
can run over their own RPC endpoint against their own disk, and it must never require a token, never
report anywhere, and never withhold function pending payment to us. That is what makes nuthatch a
public good rather than a funnel, and nothing here touches it. `nuthatch dev` stays exactly as
free, as offline-capable and as ungated as it is today.

**What an operator charges for at their own endpoint is theirs.** This is already settled and
already shipped: the Nuthatch Data Service answers `402 TAP-Receipt header required`, and nobody
considered that a violation, because the paywall is a *deployment* an operator chose, not a property
of the binary. x402 is a second payment method for the same choice.

The test that separates the two, and which any implementation must satisfy:

> Delete every payment feature from the tree and a self-hoster loses nothing. Enable one and the
> binary still runs, unpriced, for anyone who did not.

If a future slice fails that test - a price that cannot be turned off, a settlement path the binary
requires to start, a key we hold - it has crossed from operator choice into gated product, and §3
refuses it.

## 2. Motivation

The buyer this serves is not the buyer TAP serves, and conflating them is why "just use TAP" is the
wrong answer rather than merely an unhelpful one.

| | TAP consumer | x402 agent |
|---|---|---|
| Holds | GRT, escrow, an indexer relationship | a USDC balance |
| Setup | provision, authorise a signer, fund escrow, whitelist an aggregator | none |
| Settles | periodically, via RAVs | per request |
| Wants | a lot of queries, cheaply | one answer, now |
| Knows what The Graph is | yes | very often no |

The last row is the one that matters. An agent evaluating whether some chain fact is true does not
want to join an ecosystem; it wants an answer and a way to pay for it. Every step between those two
is a step at which it goes somewhere else.

There is also a fit with work that already exists, and it is not a coincidence: **RFC-0034's bounded
surface is what makes a query priceable at all.** A declared, parameterised query has a shape and a
cost knowable in advance; arbitrary SQL has a cost profile a caller discovers by trying things,
which is unquotable and, as §1 of that RFC notes, is the liability that motivated bounding it. A
priced endpoint should be an allowlisted one. The two RFCs want each other.

## 3. Goals

- An operator may accept x402 for named queries on a bounded surface, and set the price.
- **No third party in the query path.** No facilitator call, no RPC call, no network egress at all
  between request and response beyond what serving the answer already needs.
- Off by default and absent by default: unconfigured means the endpoint behaves exactly as it does
  today.
- The buyer keeps something citable: the provenance stamp of RFC-0016 §4, and optionally a signed
  receipt.

## 4. Non-goals

- **Not a nuthatch wallet.** The binary never holds a spending key and never submits a transaction.
  Settlement is an operator-side concern with an operator-side key, out of process.
- Not a replacement for TAP. Two doors; see §8.
- Not subscriptions, quotas, or per-consumer accounts. One payment buys one query.
- Not a price oracle. The operator names a number.

## 5. The design, and the constraint that shapes it

### 5.1 The obvious design is wrong

The x402 reference flow is: receive a payment, ask a **facilitator** to verify and settle it, serve
the response. It is a good design for a web service and it is wrong here, for two reasons that are
the same reason twice.

**It puts an outbound third-party call in the data path.** RFC-0045 §3 already settled the rule for
offchain data - *fetching is host-side and out-of-band, never in the data path* - and a facilitator
in the request path is precisely the shape that rule refuses. It also makes someone else's outage
into our `500`, and someone else's latency into our p99, on a surface whose whole claim is that it
answers from local disk.

**It leaks the query pattern to a third party.** A facilitator that sees every payment sees the
timing, size and frequency of every paid question. On a product whose adjacent sibling
([nutcracker](https://github.com/nightswatchhq/nutcracker)) exists specifically because metadata
about queries is the thing worth protecting, that is not a detail to wave through.

### 5.2 Verify at the counter, settle in the back office

The proposal is to split the two operations that the reference flow fuses.

**At request time**, the nest verifies the presented authorisation entirely locally: the EIP-712
signature recovers to the stated payer, the recipient is *our* configured address, the amount is at
least the price, the validity window is open, the network matches. All of that is arithmetic over
bytes already in hand. No network. It then serves the answer and **records the authorisation**.

**Out of band**, on a schedule and in a separate process, an operator-side settler submits the
recorded authorisations. It holds the gas key. The nest does not.

The property this buys: a paid query costs the same as a free one and depends on nothing but the
nest.

### 5.3 What that concedes, stated plainly

A verified authorisation is a promise. Serving before settling means an authorisation that later
fails - the payer's balance moved, the nonce was already spent elsewhere - bought one query that was
never paid for.

This is a real cost and it is bounded, which is the reason to accept it:

- The loss per bad authorisation is **one query's price**, not an account balance.
- The payer is *named* by their own signature, so failures attribute. A per-payer record of settled
  and failed authorisations lets an operator refuse a payer whose promises do not clear. That is a
  credit limit of one query against an unknown counterparty, which is roughly what any shop extends.
- The alternative - checking a balance on-chain before serving - is an RPC call in the query path,
  which is the thing §5.1 refuses, and it is not even sound: a balance checked at request time can
  be gone at settlement time.

The honest framing is that this is a **shop, not a vending machine**. It hands over the goods on a
signature and collects afterwards, and it stops serving people whose signatures do not clear.

### 5.4 Where the price is quoted

The `402` body follows the wire format observed live from The Graph's gateway on 2026-08-18 and
already parsed by our own buyer - `payment-required` header, base64 JSON, an `accepts` array of
price tags, payment returned in `Payment-Signature`. Selling in a format we have proven we can buy
in is worth more than selling in one we have only read about, and it means an agent that can already
pay The Graph can already pay us.

## 6. Implementation sketch

Nothing is proposed for this year. If it is ever unfrozen, the slices are:

- **Slice 0 - the boundary test.** Before any feature: a test asserting that with payment absent the
  binary's behaviour is byte-identical to today, and that no payment code can be reached without
  explicit configuration. §1's test made mechanical. If this cannot be written, the design is wrong.
- **Slice 1 - verification.** Pure function, no I/O: authorisation in, verdict out. Ports directly
  from the tested TypeScript in `lodestar/src/lib/x402-seller.ts`. Testing is a signature made by an
  *independent* EIP-712 implementation, for the reason in §9.
- **Slice 2 - the counter.** A mount option, alongside RFC-0034's surface: price, recipient, network.
  `402` with a challenge when a paid mount is called without payment; serve when verified. Records
  the authorisation and nothing else.
- **Slice 3 - the back office.** A separate binary or an operator script that drains recorded
  authorisations and settles them, plus the per-payer record §5.3 needs. **Not part of the nest.**

## 7. Testing

- The hand-written EIP-712 digest is the part most likely to be quietly wrong, and it fails in the
  worst direction: a bad construction recovers to *some* address rather than erroring, so it presents
  as a forged payment rather than as our bug. It is verified against an independent implementation of
  the same standard, not against itself. This is the same trap and the same remedy as the RCA hashing
  in `weaver`, where it cost a day.
- Every field is asserted against **our** configuration rather than the payment's own claims. A
  beautifully signed payment to somebody else is not a payment to us, and a test that reads `payTo`
  out of the payment proves nothing.
- The typehash is recomputed in a test rather than trusted as a constant.
- Negative tests assert specific refusals. A generic "rejected" passes for the wrong reason.

## 8. The strategic question, which is not ours to settle alone

**Revenue over x402 does not flow through Horizon.** No TAP receipts, no RAVs, no
`GraphTallyCollector`. Nothing an indexer earns from, and nothing the protocol sees.

For a project whose adjacent work exists to strengthen those rails, that deserves a decision rather
than a drift. The defensible position is two doors, said out loud: TAP for consumers inside the
protocol, x402 as an on-ramp for agents outside it, with the second understood as recruitment rather
than as a bypass. The indefensible one is reaching for x402 because it is easier and quietly routing
around the thing the rest of the work is for.

**Answered, 2026-08-30: two doors, deliberately.** x402 is adopted wherever it fits, and the
reasoning is not that Horizon does not matter but that an agent which has never heard of The Graph
is not a consumer Horizon was ever going to capture. A payment rail nobody uses routes nothing.

That makes the second door an on-ramp rather than a bypass, and the obligation that comes with the
decision is to keep saying so out loud: the moment x402 starts carrying revenue that *would* have
gone through TAP, this section needs revisiting rather than quietly not mentioning it.

The question was recorded here before it was answered, which is the only reason it could be answered
rather than drifted into.

## 9. Risks

| Risk | Mitigation |
|---|---|
| A wrong EIP-712 digest reads as a forged payment | Verified against an independent implementation (§7) |
| Unsettled authorisations | Bounded to one query's price; per-payer record; §5.3 |
| Drift from operator choice into gated product | Slice 0's boundary test; §1 |
| A facilitator in the query path | Refused by design; §5.1 |
| Payment metadata leaking to a third party | Same refusal; verification is local |
| x402 revenue undermining Horizon adoption | Not an engineering risk. §8, and a decision owed in public |

## 10. Alternatives considered

- **Facilitator in the request path.** The reference design. Refused: §5.1.
- **Pre-settlement - require the payer to transfer first, then present proof.** Sound, and it
  removes the credit risk entirely, but it needs an on-chain read to check the transfer landed, which
  is the same call in the same path. Worth revisiting if a nest ever has a trusted local chain view
  cheap enough to consult, which is not absurd given what a nest already is.
- **Deposit accounts.** A payer tops up, queries draw down. Removes per-request settlement and is
  strictly better for a repeat buyer - and it is a custody relationship, an account system, and a
  liability. Out of scope, and if it is ever wanted, TAP already is this.
- **Do nothing.** Entirely defensible while the freeze holds. The cost is that the agent audience
  RFC-0015 and nutcracker both court has no way to pay for a single answer, and the answer to
  Graphtronauts' question stays "someday".

## 11. Open questions

1. Two doors, or is bypassing Horizon a line not to cross? Governance, and §8 says why it is not
   ours alone.
2. Facilitator or operator-side settler. This RFC argues the latter; the former is one config away
   for an operator who disagrees, and that is the right place for the disagreement to live.
3. Price. The Graph's own gateway lists 0.01 USDC per query. A named nuthatch query is a different
   good, probably worth less per call.
4. Does a paid answer carry a signed receipt by default? `tattler` already produces one over a named,
   pinned query, and "you paid for this and here is proof of what you got" is a better product than
   either half alone.
5. Which surface is priceable. This RFC assumes RFC-0034 named queries only, and free-form SQL never.
   That is a defensible default and worth arguing rather than assuming.
