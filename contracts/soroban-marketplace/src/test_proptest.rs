//! test_proptest.rs — Property-based tests for the bid-history ring-buffer.
//!
//! # What is being tested
//!
//! `storage::append_bid_record` implements a bounded ring-buffer over a
//! `Vec<BidRecord>` stored in contract persistent storage.  When the Vec has
//! fewer than `cap` entries, new records are simply appended.  When the Vec is
//! full (`len >= cap`), the oldest entry (index 0) is evicted before the new
//! record is pushed.
//!
//! ## O(n) note
//! Eviction calls `Vec::remove(0)` which shifts all remaining elements left by
//! one position — O(n) in the number of stored records.  For the allowed cap
//! range of 1–`MAX_BID_HISTORY_CAP` (200) this is acceptable:
//!   - Maximum shift: 199 elements × small fixed `BidRecord` struct.
//!   - Total cost per bid at max-cap: ~200 read/write ops inside the Soroban
//!     `Vec`, well within the per-transaction compute budget.
//! A deque would eliminate the shift but Soroban's `Vec` does not expose that
//! API.  The linear-shift implementation is intentional and documented here so
//! reviewers can assess the tradeoff consciously.
//!
//! # Property
//! For any N bids placed on an auction with cap C, the stored bid count is
//! always exactly `min(N, C)`.
//!
//! The proptest generates:
//!   - `cap`  ∈ [1, 200]  (the full admin-configurable range)
//!   - `bids` ∈ [0, 400]  (deliberately exceeds cap to exercise eviction)

extern crate std;

use proptest::prelude::*;
use soroban_sdk::{testutils::Address as _, Address, Env};

use crate::{
    storage::{append_bid_record, load_auction_bids, MAX_BID_HISTORY_CAP},
    types::BidRecord,
};

/// Run one trial: place `bid_count` bids with cap `cap` and assert the stored
/// history length equals `min(bid_count, cap)`.
fn run_ring_buffer_trial(cap: u32, bid_count: usize) {
    let env = Env::default();
    env.mock_all_auths();

    // Use a fixed auction_id; isolated per-test Env means no cross-test bleed.
    let auction_id: u64 = 1;

    let address = Address::generate(&env);

    for i in 0..bid_count {
        let record = BidRecord {
            bidder: address.clone(),
            amount: (i as i128 + 1) * 100,
            ledger: i as u32 + 1,
        };
        append_bid_record(&env, auction_id, &record, cap);
    }

    let history = load_auction_bids(&env, auction_id);
    let expected = std::cmp::min(bid_count as u32, cap);
    assert_eq!(
        history.len(),
        expected,
        "cap={cap}, bid_count={bid_count}: expected {expected} stored records, got {}",
        history.len()
    );
}

proptest! {
    /// The ring-buffer never stores more than `cap` entries regardless of how
    /// many bids are placed.
    #[test]
    fn ring_buffer_len_is_always_min_n_cap(
        cap  in 1u32..=MAX_BID_HISTORY_CAP,
        bids in 0usize..=400,
    ) {
        run_ring_buffer_trial(cap, bids);
    }

    /// Edge case: cap == 1 means only the most recent bid is ever kept.
    #[test]
    fn ring_buffer_cap_one_keeps_only_latest_bid(
        bids in 1usize..=100,
    ) {
        let env = Env::default();
        env.mock_all_auths();
        let auction_id: u64 = 42;
        let address = Address::generate(&env);
        for i in 0..bids {
            append_bid_record(
                &env,
                auction_id,
                &BidRecord { bidder: address.clone(), amount: i as i128 + 1, ledger: i as u32 },
                1,
            );
        }
        let history = load_auction_bids(&env, auction_id);
        prop_assert_eq!(history.len(), 1u32);
        // The single retained record must be the LAST bid placed.
        let last = history.get(0).unwrap();
        prop_assert_eq!(last.amount, bids as i128);
    }

    /// When bid_count <= cap, no eviction occurs and all records are retained
    /// in insertion order.
    #[test]
    fn ring_buffer_no_eviction_when_bids_lte_cap(
        cap  in 1u32..=MAX_BID_HISTORY_CAP,
        bids in 0usize..=200,
    ) {
        // Only test the no-eviction case.
        prop_assume!(bids as u32 <= cap);
        run_ring_buffer_trial(cap, bids);

        // Additionally verify ordering: amounts should be 100, 200, … (ascending).
        let env = Env::default();
        env.mock_all_auths();
        let auction_id: u64 = 99;
        let address = Address::generate(&env);
        for i in 0..bids {
            append_bid_record(
                &env,
                auction_id,
                &BidRecord { bidder: address.clone(), amount: (i as i128 + 1) * 100, ledger: i as u32 },
                cap,
            );
        }
        let history = load_auction_bids(&env, auction_id);
        for (idx, record) in history.iter().enumerate() {
            prop_assert_eq!(
                record.amount,
                (idx as i128 + 1) * 100,
                "record at index {idx} should have amount {} but got {}",
                (idx as i128 + 1) * 100,
                record.amount
            );
        }
    }
}
