//! What the engine may know about a settlement transaction (SPEC §8).
//!
//! A synchronous call has two outcomes. A transaction submitted to a network you do not
//! control has **three**, because between submission and inclusion there is an interval in
//! which no local answer exists: the RPC times out, the response is lost, the transaction
//! sits in the mempool underpriced, the including block is orphaned, or the process dies
//! mid-send.
//!
//! Every local guess during that interval is wrong:
//!
//! | Guess | Consequence |
//! |---|---|
//! | assume failure, release the claims | the maker requotes the same capital, the transaction lands → **duplicated** |
//! | assume success, record the escrow | the transaction reverts → an escrow that exists nowhere on chain → **invented** |
//! | hold indefinitely | the node never received it → capital claimed forever → **stuck** |
//!
//! The resolution is not to guess. The bundle carries a nonce that is unique and
//! deterministic without hashing, and the request sits in `Settling` with its claims held
//! until the nonce's fate is definitively known. Idempotency turns "unknown" from a
//! catastrophe into a delay.
//!
//! This type lives in `core` because the engine acts on it. It is delivered *to* the engine
//! by a poller, in a command, like every other fact about the outside world — the engine
//! never calls custody to ask (§13.1).

/// The fate of a **nonce**, against final chain state.
///
/// **Not the outcome of whichever submission most recently carried it.** That distinction is
/// the whole of §8.1, and the failure it prevents is subtle enough to be worth restating:
/// the engine submits, the transaction is included, the acknowledgement is lost. The engine
/// correctly retries — that is what an idempotent nonce is for. The chain refuses the retry
/// because the nonce is already consumed, and a naive implementation reports `Reverted`. The
/// engine concludes the settlement failed and releases the committed claims, for a
/// settlement that actually succeeded.
///
/// Every component told the truth; the retry genuinely did revert. The damage is a permanent
/// split between the layers — the engine shows the capital free and will admit quotes
/// against it, while custody holds it in escrow backing a live position. **Conservation
/// cannot detect this**, because each layer stays internally consistent and the sums balance
/// on both sides of a model that has come apart.
///
/// Two reverts that look identical mean opposite things. Reverted on insufficient funds
/// means the trade never happened. Reverted on a consumed nonce means **the trade already
/// happened**: a retry bouncing off its own nonce is evidence the original succeeded.
///
/// **Monotonic and terminal.** Once a nonce reaches `Settled` or `Reverted` that answer is
/// immutable, and a later submission cannot move it back to `Pending` or `Unknown`. This is
/// oracle monotonicity (§10.1) one layer down: a status that can regress lets a later,
/// less-informed observation overwrite an earlier, better-informed one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum TxStatus {
    /// No local answer exists. The node may never have received it; it may be in a mempool.
    /// **Never a reason to release anything** (§8.3).
    Unknown,
    /// Received and not yet included.
    Pending,
    /// Included and applied. Terminal.
    Settled,
    /// Included and reverted, or refused. Terminal.
    Reverted,
}

impl TxStatus {
    /// Whether this answer is final. Only a final answer may move a request out of
    /// `Settling`; everything else means keep holding and keep polling.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Settled | Self::Reverted)
    }
}
