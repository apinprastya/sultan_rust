//! The customer domain (spec `docs/spec/2.5-customers.md`, phase M1): phone, code, tax id and
//! profile field rules, when a membership tier applies, the automatic tier selection, the
//! masked receipt line, the merge rules and the customer balance for saved change.
//!
//! Pure: no IO, no clock and no random number generator. Today's date, the branch time zone and
//! a source of random bytes (for [`CustomerCode::generate`]) are passed in. Uniqueness (code,
//! phone among non-archived customers), permissions and where an action may run (HQ or branch)
//! are checked by the database and server.
//!
//! Group, tier and customer ids are plain [`uuid::Uuid`]s here; typed ids come with the
//! database phase.

mod balance;
mod code;
mod error;
mod fields;
mod merge;
mod phone;
mod receipt;
mod tier;

pub use balance::{
    BalanceEntry, BalanceEntryKind, BalanceMove, BalanceSettings, SavedChangeCash, check_erase,
    ledger_balance, max_savable, merge_balance, pay_with_balance, payout, reverse_sale,
    save_change, saved_change_cash,
};
pub use code::{CODE_ALPHABET, CODE_LEN, CustomerCode};
pub use error::CustomerError;
pub use fields::{Birthday, MAX_CUSTOMER_NAME_LEN, TaxId, normalize_email, normalize_name};
pub use merge::{CustomerProfile, MergeParty, TierAssignment, check_merge, merge_profiles};
pub use phone::{MAX_PHONE_DIGITS, MIN_PHONE_DIGITS, Phone};
pub use receipt::{RECEIPT_SEPARATOR, ReceiptCustomer, short_name};
pub use tier::{AutoTier, TierRule, TierState, auto_tier, tier_applies, tier_applies_at};
