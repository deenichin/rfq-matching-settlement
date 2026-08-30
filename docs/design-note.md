# If quotes live for days instead of seconds

The data structures do not care. What the maker has written does.

Reserved capital sits on a per-account chain ordered by expiry, reclaimed inline by the next
command touching that account. No sweeper, no timer wheel, no global expiry structure — a claim
costs what it frees, whether it lived five seconds or a week
(`core/ledger.rs::release_expired_reclaims_the_expired_prefix_and_leaves_the_total_equal_to_the_chain_sum`).

The cost is irrevocability. Quotes cannot be cancelled and replacement may only improve, so the
expiry a maker chooses is their only exposure control. A firm one-sided price the counterparty
may take at their discretion is an option, and its value scales with time and volatility. At
five seconds it is worth nothing, which is why irrevocability was cheap to insist on. At a day
it can exceed the spread being quoted, and makers respond by widening or not quoting — so long
TTLs degrade the prices the venue exists to produce.

It also turns the anti-griefing argument inside out. Sitting on a request costs the requester
today; at day scale it is the whole point — hold a spread of free options and accept only what
has moved in your favour. Systematic adverse selection, with no instrument against it, because
the maker can neither pull the quote nor widen it.

Which is why `CancelQuote` is a rejected transition rather than an absent one
(`market.rs::cancel_quote_is_a_rejected_transition_not_an_absent_one`): the command exists and
the quote is looked up, so permitting cancellation is a policy change at a call site that is
already there. Likewise the timescale itself — both TTLs and the withdrawal delay are
configuration, coupled by an inequality checked at boot, so an incoherent answer refuses to
start rather than quietly under-collateralising.

Two things would need building. Nothing persists: a restart loses seconds of quotes at second
scale and voids every standing commitment at day scale. Deterministic replay from the command
log already reproduces state byte for byte
(`single_writer.rs::several_client_threads_submit_and_the_channel_serialises_them`) — nothing
writes that log anywhere durable. And `min_horizon` must exceed `max_request_ttl +
max_settling_time`, so day-scale requests push it past a day and a contract becomes untradeable
in the final day before its event, when a prediction market is busiest.

Reclamation is an implementation detail and capital lockup is a business decision, but
irrevocability is what settles it. Firm quotes, no last look, no maker able to renege — a
strong property, negligible at seconds and prohibitive at days. Going to days means pricing
that option or allowing the cancellation the design refused, and the second changes what the
venue is.
