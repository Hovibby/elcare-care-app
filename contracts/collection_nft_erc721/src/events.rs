//! events.rs — Approval event emitters for NormalNFT721.
//!
//! Three event topics are defined:
//!
//! | Topic constant      | Emitted by                | Meaning                                       |
//! |---------------------|---------------------------|-----------------------------------------------|
//! | `approval_set`      | `approve`                 | Per-token approval granted (with optional expiry). |
//! | `appr_all_set`      | `set_approval_for_all`    | Collection-level operator grant/revoke.       |
//! | `appr_revoked`      | `revoke_all_approvals`    | All per-token approvals revoked by owner.     |
//!
//! All events are emitted via `env.events().publish(topics, data)`.
//! Indexers can filter on the topic symbols to reconstruct the full
//! approval state of every token without re-reading contract storage.

use soroban_sdk::{symbol_short, Address, Env};

/// Emitted when a per-token approval is set or updated.
///
/// Topics : `("approval_set", owner)`
/// Data   : `(approved, token_id, expires_at)`
///
/// * `expires_at == 0` → the approval never expires (sentinel `NO_EXPIRY`).
#[allow(deprecated)]
pub fn emit_approval_set(
    env: &Env,
    owner: &Address,
    approved: &Address,
    token_id: u64,
    expires_at: u32,
) {
    env.events().publish(
        (symbol_short!("aprvl_set"), owner.clone()),
        (approved.clone(), token_id, expires_at),
    );
}

/// Emitted when an operator's collection-wide approval is granted or revoked.
///
/// Topics : `("appr_all_set", owner)`
/// Data   : `(operator, approved, expires_at)`
///
/// * `approved == false` → revocation; `expires_at` is always 0 in that case.
/// * `expires_at == 0`   → the grant never expires (sentinel `NO_EXPIRY`).
#[allow(deprecated)]
pub fn emit_approval_for_all_set(
    env: &Env,
    owner: &Address,
    operator: &Address,
    approved: bool,
    expires_at: u32,
) {
    env.events().publish(
        (symbol_short!("aprall_st"), owner.clone()),
        (operator.clone(), approved, expires_at),
    );
}

/// Emitted when the token owner calls `revoke_all_approvals(token_id)`.
///
/// Topics : `("aprvl_rvkd", owner)`
/// Data   : `token_id`
///
/// Indexers that cache per-token approvals should treat this as a full
/// invalidation of the `Approved` and `ApprovedExpiry` entries for the token.
#[allow(deprecated)]
pub fn emit_approval_revoked(env: &Env, owner: &Address, token_id: u64) {
    env.events().publish(
        (symbol_short!("aprvl_rvkd"), owner.clone()),
        token_id,
    );
}
