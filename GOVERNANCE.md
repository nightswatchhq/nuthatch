# Governance & scope

nuthatch is a free, `MIT OR Apache-2.0` public good with a single maintainer. This
document states how it can be sustained, what stays out of scope regardless of who's paying, and the
neutrality guarantees that make it safe to depend on. See [RFC-0006](docs/rfcs/0006-grant-funding.md)
for the full reasoning.

## Sustainability

No grant has been awarded. Grants remain a possible source of funding for open-source work, but no
roadmap item depends on one. Nightswatch also operates a [hosted nest service](https://platform.nuthatch-indexer.com)
using the published binary. Its plans and billing are separate from this repository.

The boundary in [RFC-0006](docs/rfcs/0006-grant-funding.md) still applies: the same milestone must
not be funded twice, and a funding arrangement must not give an operator control of the open-source
core.

## Neutrality (the guarantee you can depend on)

**No operator has exclusivity, a private fork, partner-only features in the core, or roadmap veto.**
Any operator may host nuthatch; a partner's edge is partnership, being first, and priority support of
*their own* integration - never a gate on anyone else. The permissive licence makes capture of *this*
project impossible: anyone can run, fork, host or embed the exact same software, and the maintainer
holds no rights over it that anyone else lacks.

Stated plainly, because the relicence changed this and pretending otherwise would be dishonest: a
permissive licence also allows a **closed** derivative. Someone may take nuthatch, extend it privately
and sell it without publishing anything. That is a deliberate trade - maximal adoption and zero
friction for embedders, in exchange for giving up the copyleft protection that would have forbidden it.
What remains guaranteed is that the upstream project stays open and stays free; what is not guaranteed
is that every derivative does.

### Operator-partnership disclosure

> Nightswatch operates [platform.nuthatch-indexer.com](https://platform.nuthatch-indexer.com)
> with the published nuthatch binary. The platform's accounts, plans, billing and customer isolation
> live outside this repository. Hosting confers no exclusivity, private core features or roadmap veto.

## The dividing line: core vs operator layer

nuthatch ships the **guards and signals** that make it safe to run as a service; it does **not** grow
the operator's product into the binary. Concretely (RFC-0005 §6):

| In the core (nuthatch) | The operator's layer (a gateway in front) |
|---|---|
| `/sql` resource guards (timeout, row cap, concurrency) - bound *how much* | Authentication - decides *who* may query |
| `/metrics`, `/health`, structured logs | Metering, billing, quotas |
| Bind posture + a loud warning off-localhost | Multi-tenancy, per-tenant isolation |
| Config/data stability contract | The hosted product itself |

## What we will not do - for funding or partnership

Non-negotiable regardless of who asks or pays:

- **No token**, no decentralised-network features, no staking.
- **No telemetry / phone-home**; no mandatory API tokens or gated data services in the data path.
- **The core stays permissively licensed** (`MIT OR Apache-2.0`) and is never relicensed to anything
  more restrictive or proprietary; **no private forks**; **no partner-only features** in core.
- **No roadmap veto** for any funder or partner (input is welcome; veto is not).
- **No auth / metering / multi-tenancy in core** - that is the operator layer, and it is contractual.

If a funder or partner requires any item on this list, we decline that term rather than the principle.

## Release integrity & key custody

Releases are the supply chain an operator depends on. Signing/release-key custody, and a named
successor/escrow arrangement for those keys, are tracked as an open governance item (RFC-0006 Q3) to
be settled while the project is small. Until then: releases are cut from tagged commits on `main`,
published to GitHub Releases (with per-artifact SHA-256) and crates.io, and reproducible from the
pinned toolchain.
