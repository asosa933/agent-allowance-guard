use super::*;
use near_sdk::test_utils::{get_logs, VMContextBuilder};
use near_sdk::testing_env;

const CONTRACT_ID: &str = "guard.test";
const OWNER_ID: &str = "owner.test";
const AGENT_ID: &str = "agent.test";
const VENDOR_ID: &str = "vendor.test";
const OTHER_ID: &str = "other.test";

const ONE_NEAR: u128 = 1_000_000_000_000_000_000_000_000;
const T0: u64 = 1_700_000_000_000_000_000;

fn account(name: &str) -> AccountId {
    name.parse().unwrap()
}

fn set_caller(predecessor: &str, timestamp: u64, balance: u128) {
    let context = VMContextBuilder::new()
        .current_account_id(account(CONTRACT_ID))
        .signer_account_id(account(predecessor))
        .predecessor_account_id(account(predecessor))
        .block_timestamp(timestamp)
        .account_balance(NearToken::from_yoctonear(balance))
        .prepaid_gas(Gas::from_tgas(300))
        .build();
    testing_env!(context);
}

fn vault() -> Contract {
    set_caller(OWNER_ID, T0, 50 * ONE_NEAR);
    let mut contract = Contract::new(
        account(OWNER_ID),
        account(AGENT_ID),
        U128(ONE_NEAR),
        U128(2 * ONE_NEAR),
    );
    contract.add_recipient(account(VENDOR_ID));
    contract
}

fn pay(contract: &mut Contract, receiver: &str, amount: u128) -> PaymentOutcome {
    set_caller(AGENT_ID, T0, 50 * ONE_NEAR);
    match contract.pay(account(receiver), U128(amount), Some("invoice".to_string())) {
        PromiseOrValue::Value(outcome) => outcome,
        PromiseOrValue::Promise(_) => panic!("expected a policy decision, got a transfer"),
    }
}

fn pay_at(contract: &mut Contract, amount: u128, timestamp: u64) -> PaymentOutcome {
    set_caller(AGENT_ID, timestamp, 50 * ONE_NEAR);
    match contract.pay(account(VENDOR_ID), U128(amount), None) {
        PromiseOrValue::Value(outcome) => outcome,
        PromiseOrValue::Promise(_) => panic!("expected a policy decision, got a transfer"),
    }
}

fn allow(contract: &mut Contract, amount: u128) {
    set_caller(AGENT_ID, T0, 50 * ONE_NEAR);
    let outcome = contract.pay(account(VENDOR_ID), U128(amount), None);
    assert!(matches!(outcome, PromiseOrValue::Promise(_)));
}

fn event_names() -> Vec<String> {
    get_logs()
        .into_iter()
        .filter_map(|line| {
            let json = line.strip_prefix("EVENT_JSON:")?;
            let value: serde_json::Value = serde_json::from_str(json).ok()?;
            value["event"].as_str().map(str::to_string)
        })
        .collect()
}

#[test]
fn allowed_payment_is_counted_and_logged() {
    let mut contract = vault();
    set_caller(AGENT_ID, T0, 50 * ONE_NEAR);
    let outcome = contract.pay(
        account(VENDOR_ID),
        U128(ONE_NEAR / 2),
        Some("  gpu  ".into()),
    );
    assert!(matches!(outcome, PromiseOrValue::Promise(_)));
    let spent = contract.get_spent_today();
    assert_eq!(spent.spent.0, ONE_NEAR / 2);
    assert_eq!(spent.remaining.0, 2 * ONE_NEAR - ONE_NEAR / 2);
    let payments = contract.get_recent_payments(None);
    assert_eq!(payments.len(), 1);
    assert_eq!(payments[0].status, "pending");
    assert_eq!(payments[0].memo.as_deref(), Some("gpu"));
    assert_eq!(payments[0].amount.0, ONE_NEAR / 2);
    assert_eq!(event_names(), vec!["payment_allowed".to_string()]);
    let policy = contract.get_policy();
    assert_eq!(policy.agent_id, account(AGENT_ID));
    assert_eq!(policy.owner_id, account(OWNER_ID));
    assert!(!policy.paused);
    assert_eq!(policy.payment_count.0, 1);
}

#[test]
fn exact_per_payment_limit_is_allowed_and_one_yocto_over_is_blocked() {
    let mut contract = vault();
    allow(&mut contract, ONE_NEAR);
    assert_eq!(contract.get_spent_today().spent.0, ONE_NEAR);

    let mut contract = vault();
    let blocked = pay(&mut contract, VENDOR_ID, ONE_NEAR + 1);
    assert!(!blocked.allowed);
    assert_eq!(blocked.reason.as_deref(), Some("exceeds_per_payment_limit"));
    assert_eq!(contract.get_spent_today().spent.0, 0);
    assert_eq!(
        contract.get_recent_rejections(None)[0].reason,
        "exceeds_per_payment_limit"
    );
    assert_eq!(event_names(), vec!["payment_blocked".to_string()]);
}

#[test]
fn daily_limit_counts_the_rolling_sum_including_the_boundary() {
    let mut contract = vault();
    allow(&mut contract, ONE_NEAR);
    allow(&mut contract, ONE_NEAR);
    assert_eq!(contract.get_spent_today().spent.0, 2 * ONE_NEAR);
    let blocked = pay(&mut contract, VENDOR_ID, 1);
    assert!(!blocked.allowed);
    assert_eq!(blocked.reason.as_deref(), Some("exceeds_daily_limit"));
    assert_eq!(contract.get_recent_payments(None).len(), 2);
    assert_eq!(contract.get_policy().rejection_count.0, 1);
}

#[test]
fn rolling_window_drops_spend_after_24_hours() {
    set_caller(OWNER_ID, T0, 50 * ONE_NEAR);
    let mut contract = Contract::new(
        account(OWNER_ID),
        account(AGENT_ID),
        U128(2 * ONE_NEAR),
        U128(2 * ONE_NEAR),
    );
    contract.add_recipient(account(VENDOR_ID));
    set_caller(AGENT_ID, T0, 50 * ONE_NEAR);
    assert!(matches!(
        contract.pay(account(VENDOR_ID), U128(2 * ONE_NEAR), None),
        PromiseOrValue::Promise(_)
    ));
    let blocked = pay_at(&mut contract, 1, T0 + WINDOW_NS - 1);
    assert_eq!(blocked.reason.as_deref(), Some("exceeds_daily_limit"));

    set_caller(AGENT_ID, T0 + WINDOW_NS, 50 * ONE_NEAR);
    assert!(matches!(
        contract.pay(account(VENDOR_ID), U128(2 * ONE_NEAR), None),
        PromiseOrValue::Promise(_)
    ));
    assert_eq!(contract.get_spent_today().spent.0, 2 * ONE_NEAR);
}

#[test]
fn non_allowlisted_recipient_is_blocked() {
    let mut contract = vault();
    let blocked = pay(&mut contract, OTHER_ID, ONE_NEAR / 10);
    assert!(!blocked.allowed);
    assert_eq!(blocked.reason.as_deref(), Some("not_allowlisted"));
    assert!(contract.get_recent_payments(None).is_empty());
}

#[test]
fn paused_vault_blocks_an_otherwise_valid_payment() {
    let mut contract = vault();
    set_caller(OWNER_ID, T0, 50 * ONE_NEAR);
    contract.pause();
    contract.pause();
    let blocked = pay(&mut contract, VENDOR_ID, ONE_NEAR / 10);
    assert_eq!(blocked.reason.as_deref(), Some("paused"));

    set_caller(OWNER_ID, T0, 50 * ONE_NEAR);
    contract.unpause();
    allow(&mut contract, ONE_NEAR / 10);
    assert_eq!(contract.get_spent_today().spent.0, ONE_NEAR / 10);
}

#[test]
#[should_panic(expected = "only the agent can call pay")]
fn non_agent_cannot_pay() {
    let mut contract = vault();
    set_caller(OWNER_ID, T0, 50 * ONE_NEAR);
    let _ = contract.pay(account(VENDOR_ID), U128(1), None);
}

#[test]
#[should_panic(expected = "only the agent can call pay")]
fn stranger_cannot_pay() {
    let mut contract = vault();
    set_caller(OTHER_ID, T0, 50 * ONE_NEAR);
    let _ = contract.pay(account(VENDOR_ID), U128(1), None);
}

#[test]
#[should_panic(expected = "only the owner can call this method")]
fn agent_cannot_withdraw() {
    let mut contract = vault();
    set_caller(AGENT_ID, T0, 50 * ONE_NEAR);
    let _ = contract.withdraw(U128(1));
}

#[test]
#[should_panic(expected = "only the owner can call this method")]
fn agent_cannot_pause_or_change_policy() {
    let mut contract = vault();
    set_caller(AGENT_ID, T0, 50 * ONE_NEAR);
    contract.pause();
}

#[test]
#[should_panic(expected = "only the owner can call this method")]
fn agent_cannot_add_recipient() {
    let mut contract = vault();
    set_caller(AGENT_ID, T0, 50 * ONE_NEAR);
    contract.add_recipient(account(OTHER_ID));
}

#[test]
#[should_panic(expected = "only the owner can call this method")]
fn stranger_cannot_set_limits() {
    let mut contract = vault();
    set_caller(OTHER_ID, T0, 50 * ONE_NEAR);
    contract.set_limits(U128(1), U128(1));
}

#[test]
fn owner_can_withdraw_within_spendable_balance() {
    let mut contract = vault();
    set_caller(OWNER_ID, T0, 50 * ONE_NEAR);
    let promise = contract.withdraw(U128(ONE_NEAR));
    assert!(matches!(promise, Promise { .. }));
    assert_eq!(event_names(), vec!["withdrawn".to_string()]);
}

#[test]
#[should_panic(expected = "insufficient vault balance")]
fn owner_cannot_withdraw_the_storage_reserve() {
    let mut contract = vault();
    set_caller(OWNER_ID, T0, SAFETY_BUFFER_YOCTO);
    let _ = contract.withdraw(U128(1));
}

#[test]
fn insufficient_vault_balance_blocks_the_payment() {
    let mut contract = vault();
    set_caller(AGENT_ID, T0, SAFETY_BUFFER_YOCTO + 10);
    let outcome = match contract.pay(account(VENDOR_ID), U128(11), None) {
        PromiseOrValue::Value(outcome) => outcome,
        PromiseOrValue::Promise(_) => panic!("payment should have been blocked"),
    };
    assert_eq!(
        outcome.reason.as_deref(),
        Some("insufficient_vault_balance")
    );
}

#[test]
fn zero_amount_is_rejected_and_logged() {
    let mut contract = vault();
    let blocked = pay(&mut contract, VENDOR_ID, 0);
    assert_eq!(blocked.reason.as_deref(), Some("amount_must_be_positive"));
    assert_eq!(contract.get_recent_rejections(Some(5)).len(), 1);
}

#[test]
fn allowlist_add_and_remove() {
    let mut contract = vault();
    set_caller(OWNER_ID, T0, 50 * ONE_NEAR);
    contract.add_recipient(account(OTHER_ID));
    assert_eq!(contract.get_allowlist().len(), 2);
    contract.remove_recipient(account(VENDOR_ID));
    assert_eq!(contract.get_allowlist(), vec![account(OTHER_ID)]);
    let blocked = pay(&mut contract, VENDOR_ID, 1);
    assert_eq!(blocked.reason.as_deref(), Some("not_allowlisted"));
    set_caller(AGENT_ID, T0, 50 * ONE_NEAR);
    assert!(matches!(
        contract.pay(account(OTHER_ID), U128(1), None),
        PromiseOrValue::Promise(_)
    ));
}

#[test]
#[should_panic(expected = "daily limit must cover one payment")]
fn limits_must_cover_a_single_payment() {
    let mut contract = vault();
    set_caller(OWNER_ID, T0, 50 * ONE_NEAR);
    contract.set_limits(U128(5), U128(4));
}

#[test]
#[should_panic(expected = "owner and agent must be different accounts")]
fn agent_cannot_be_the_owner() {
    let mut contract = vault();
    set_caller(OWNER_ID, T0, 50 * ONE_NEAR);
    contract.set_agent(account(OWNER_ID));
}

#[test]
fn failed_transfer_reverts_the_rolling_spend() {
    let mut contract = vault();
    allow(&mut contract, ONE_NEAR);
    assert_eq!(contract.get_spent_today().spent.0, ONE_NEAR);

    set_caller(CONTRACT_ID, T0, 50 * ONE_NEAR);
    let outcome = contract.on_pay_settled(
        1,
        account(VENDOR_ID),
        U128(ONE_NEAR),
        Err(PromiseError::Failed),
    );
    assert!(!outcome.allowed);
    assert_eq!(outcome.reason.as_deref(), Some("transfer_failed"));
    assert_eq!(contract.get_spent_today().spent.0, 0);
    assert_eq!(contract.get_recent_payments(None)[0].status, "reverted");
    assert_eq!(
        contract.get_recent_rejections(None)[0].reason,
        "transfer_failed"
    );
}

#[test]
fn settled_transfer_keeps_the_spend() {
    let mut contract = vault();
    allow(&mut contract, ONE_NEAR / 5);
    set_caller(CONTRACT_ID, T0, 50 * ONE_NEAR);
    let outcome = contract.on_pay_settled(1, account(VENDOR_ID), U128(ONE_NEAR / 5), Ok(()));
    assert!(outcome.allowed);
    assert_eq!(contract.get_spent_today().spent.0, ONE_NEAR / 5);
    assert_eq!(contract.get_recent_payments(None)[0].status, "settled");
}

#[test]
fn window_log_full_rejects_instead_of_forgetting_spend() {
    set_caller(OWNER_ID, T0, 50 * ONE_NEAR);
    let mut contract = Contract::new(account(OWNER_ID), account(AGENT_ID), U128(1), U128(1_000));
    contract.add_recipient(account(VENDOR_ID));
    for _ in 0..128 {
        set_caller(AGENT_ID, T0, 50 * ONE_NEAR);
        assert!(matches!(
            contract.pay(account(VENDOR_ID), U128(1), None),
            PromiseOrValue::Promise(_)
        ));
    }
    let blocked = pay(&mut contract, VENDOR_ID, 1);
    assert_eq!(blocked.reason.as_deref(), Some("window_log_full"));
    assert_eq!(contract.get_spent_today().spent.0, 128);

    set_caller(AGENT_ID, T0 + WINDOW_NS, 50 * ONE_NEAR);
    assert!(matches!(
        contract.pay(account(VENDOR_ID), U128(1), None),
        PromiseOrValue::Promise(_)
    ));
    assert_eq!(contract.get_spent_today().spent.0, 1);
}

#[test]
#[should_panic(expected = "memo is longer than 120 bytes")]
fn oversized_memo_is_rejected() {
    let mut contract = vault();
    set_caller(AGENT_ID, T0, 50 * ONE_NEAR);
    let _ = contract.pay(account(VENDOR_ID), U128(1), Some("x".repeat(121)));
}

#[test]
fn recent_views_return_newest_first() {
    let mut contract = vault();
    let first = pay(&mut contract, OTHER_ID, 1);
    let second = pay(&mut contract, OTHER_ID, 2);
    let recent = contract.get_recent_rejections(Some(1));
    assert_eq!(recent.len(), 1);
    assert_eq!(recent[0].id, second.rejection_id.unwrap());
    assert!(first.rejection_id.unwrap().0 < second.rejection_id.unwrap().0);
}

#[test]
#[should_panic(expected = "attach NEAR to fund the vault")]
fn fund_requires_a_deposit() {
    let mut contract = vault();
    set_caller(OWNER_ID, T0, 50 * ONE_NEAR);
    let _ = contract.fund();
}

#[test]
fn fund_emits_an_event() {
    let mut contract = vault();
    let context = VMContextBuilder::new()
        .current_account_id(account(CONTRACT_ID))
        .signer_account_id(account(OWNER_ID))
        .predecessor_account_id(account(OWNER_ID))
        .block_timestamp(T0)
        .account_balance(NearToken::from_yoctonear(50 * ONE_NEAR))
        .attached_deposit(NearToken::from_near(1))
        .prepaid_gas(Gas::from_tgas(30))
        .build();
    testing_env!(context);
    assert_eq!(contract.fund().0, ONE_NEAR);
    assert_eq!(event_names(), vec!["vault_funded".to_string()]);
}
