//! Agent Allowance Guard.
//!
//! The contract account is a vault. The owner funds it and sets a spending
//! policy. The agent may call [`Contract::pay`] only, and only inside that
//! policy. A limited NEAR function-call access key on the agent account is
//! what makes the restriction hold even if the agent process is compromised:
//! the key is not allowed to call `withdraw`, `pause`, or `set_limits`.

use near_sdk::json_types::{U128, U64};
use near_sdk::{
    env, near, require, serde_json, AccountId, Gas, NearToken, Promise, PromiseError,
    PromiseOrValue,
};

const VERSION: &str = "1.0.0";
const EVENT_STANDARD: &str = "agent-allowance-guard";

/// Rolling window. "Spent today" means successful payments inside this window.
const WINDOW_NS: u64 = 24 * 60 * 60 * 1_000_000_000;

/// In-window payments kept for the rolling sum. A 129th payment that is still
/// inside the window is rejected, so flooding cannot erase older spend and
/// sneak past the daily limit.
const MAX_SPEND_ENTRIES: usize = 128;

/// Recent payments and rejections retained for view methods.
const MAX_LOG_ENTRIES: usize = 64;

const MAX_ALLOWLIST: usize = 32;
const MAX_MEMO_BYTES: usize = 120;

/// Left untouched so storage staking can grow a little after a payment.
/// 0.05 NEAR.
const SAFETY_BUFFER_YOCTO: u128 = 50_000_000_000_000_000_000_000;

const SETTLEMENT_GAS: Gas = Gas::from_tgas(20);

#[derive(Clone, Copy, PartialEq, Eq)]
enum BlockReason {
    AmountMustBePositive,
    Paused,
    NotAllowlisted,
    ExceedsPerPaymentLimit,
    ExceedsDailyLimit,
    InsufficientVaultBalance,
    WindowLogFull,
    TransferFailed,
}

impl BlockReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::AmountMustBePositive => "amount_must_be_positive",
            Self::Paused => "paused",
            Self::NotAllowlisted => "not_allowlisted",
            Self::ExceedsPerPaymentLimit => "exceeds_per_payment_limit",
            Self::ExceedsDailyLimit => "exceeds_daily_limit",
            Self::InsufficientVaultBalance => "insufficient_vault_balance",
            Self::WindowLogFull => "window_log_full",
            Self::TransferFailed => "transfer_failed",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PaymentStatus {
    Pending,
    Settled,
    Reverted,
}

impl PaymentStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Settled => "settled",
            Self::Reverted => "reverted",
        }
    }
}

#[near(serializers = [borsh])]
#[derive(Clone)]
struct SpendEntry {
    timestamp_ns: u64,
    amount: u128,
    payment_id: u64,
    active: bool,
}

#[near(serializers = [borsh])]
#[derive(Clone)]
struct StoredPayment {
    id: u64,
    receiver_id: AccountId,
    amount: u128,
    timestamp_ns: u64,
    memo: Option<String>,
    status: u8,
}

#[near(serializers = [borsh])]
#[derive(Clone)]
struct StoredRejection {
    id: u64,
    receiver_id: AccountId,
    amount: u128,
    timestamp_ns: u64,
    memo: Option<String>,
    reason: String,
}

#[near(contract_state)]
pub struct Contract {
    owner_id: AccountId,
    agent_id: AccountId,
    paused: bool,
    per_payment_limit: u128,
    daily_limit: u128,
    allowlist: Vec<AccountId>,
    spends: Vec<SpendEntry>,
    payments: Vec<StoredPayment>,
    rejections: Vec<StoredRejection>,
    next_payment_id: u64,
    next_rejection_id: u64,
    payment_count: u64,
    rejection_count: u64,
}

/// The generated contract entry points load state with `unwrap_or_default`.
/// Panicking here means a call before `new` cannot silently create an empty vault.
impl Default for Contract {
    fn default() -> Self {
        env::panic_str("contract is not initialized, call new");
    }
}

#[near(serializers = [json])]
pub struct PaymentOutcome {
    pub allowed: bool,
    pub reason: Option<String>,
    pub receiver_id: AccountId,
    pub amount: U128,
    pub memo: Option<String>,
    pub spent_in_window: U128,
    pub daily_limit: U128,
    pub payment_id: Option<U64>,
    pub rejection_id: Option<U64>,
}

#[near(serializers = [json])]
pub struct PolicyView {
    pub version: String,
    pub contract_id: AccountId,
    pub owner_id: AccountId,
    pub agent_id: AccountId,
    pub paused: bool,
    pub per_payment_limit: U128,
    pub daily_limit: U128,
    pub window_ns: U64,
    pub spent_in_window: U128,
    pub remaining_in_window: U128,
    pub spendable_balance: U128,
    pub allowlist: Vec<AccountId>,
    pub payment_count: U64,
    pub rejection_count: U64,
}

#[near(serializers = [json])]
pub struct SpentTodayView {
    pub window_ns: U64,
    pub as_of_timestamp_ns: U64,
    pub spent: U128,
    pub daily_limit: U128,
    pub remaining: U128,
}

#[near(serializers = [json])]
pub struct PaymentView {
    pub id: U64,
    pub receiver_id: AccountId,
    pub amount: U128,
    pub timestamp_ns: U64,
    pub memo: Option<String>,
    pub status: String,
}

#[near(serializers = [json])]
pub struct RejectionView {
    pub id: U64,
    pub receiver_id: AccountId,
    pub amount: U128,
    pub timestamp_ns: U64,
    pub memo: Option<String>,
    pub reason: String,
}

impl PaymentStatus {
    fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::Pending,
            1 => Self::Settled,
            _ => Self::Reverted,
        }
    }

    fn to_u8(self) -> u8 {
        match self {
            Self::Pending => 0,
            Self::Settled => 1,
            Self::Reverted => 2,
        }
    }
}

#[near]
impl Contract {
    /// Create the vault. `owner_id` and `agent_id` must differ: the predecessor
    /// of a call is how the contract tells them apart.
    #[init]
    pub fn new(
        owner_id: AccountId,
        agent_id: AccountId,
        per_payment_limit: U128,
        daily_limit: U128,
    ) -> Self {
        require!(
            owner_id != agent_id,
            "owner and agent must be different accounts"
        );
        validate_limits(per_payment_limit.0, daily_limit.0);
        Self {
            owner_id,
            agent_id,
            paused: false,
            per_payment_limit: per_payment_limit.0,
            daily_limit: daily_limit.0,
            allowlist: Vec::new(),
            spends: Vec::new(),
            payments: Vec::new(),
            rejections: Vec::new(),
            next_payment_id: 1,
            next_rejection_id: 1,
            payment_count: 0,
            rejection_count: 0,
        }
    }

    /// Accept a deposit into the vault. A plain NEAR transfer to the account
    /// also increases the balance; this method records a funding event.
    #[payable]
    pub fn fund(&mut self) -> U128 {
        let amount = env::attached_deposit().as_yoctonear();
        require!(amount > 0, "attach NEAR to fund the vault");
        emit(
            "vault_funded",
            &serde_json_map(&[
                ("funder_id", env::predecessor_account_id().to_string()),
                ("amount", amount.to_string()),
            ]),
        );
        U128(amount)
    }

    pub fn set_limits(&mut self, per_payment_limit: U128, daily_limit: U128) {
        self.assert_owner();
        validate_limits(per_payment_limit.0, daily_limit.0);
        self.per_payment_limit = per_payment_limit.0;
        self.daily_limit = daily_limit.0;
        emit(
            "policy_updated",
            &serde_json_map(&[
                ("per_payment_limit", per_payment_limit.0.to_string()),
                ("daily_limit", daily_limit.0.to_string()),
            ]),
        );
    }

    pub fn set_agent(&mut self, agent_id: AccountId) {
        self.assert_owner();
        require!(
            agent_id != self.owner_id,
            "owner and agent must be different accounts"
        );
        self.agent_id = agent_id.clone();
        emit(
            "agent_updated",
            &serde_json_map(&[("agent_id", agent_id.to_string())]),
        );
    }

    pub fn add_recipient(&mut self, account_id: AccountId) {
        self.assert_owner();
        require!(
            !self.is_allowlisted(&account_id),
            "recipient already allowlisted"
        );
        require!(self.allowlist.len() < MAX_ALLOWLIST, "allowlist is full");
        self.allowlist.push(account_id.clone());
        emit(
            "recipient_added",
            &serde_json_map(&[("account_id", account_id.to_string())]),
        );
    }

    pub fn remove_recipient(&mut self, account_id: AccountId) {
        self.assert_owner();
        let before = self.allowlist.len();
        self.allowlist.retain(|id| id != &account_id);
        require!(
            self.allowlist.len() != before,
            "recipient is not allowlisted"
        );
        emit(
            "recipient_removed",
            &serde_json_map(&[("account_id", account_id.to_string())]),
        );
    }

    pub fn pause(&mut self) {
        self.assert_owner();
        if self.paused {
            return;
        }
        self.paused = true;
        emit("paused", &serde_json_map(&[]));
    }

    pub fn unpause(&mut self) {
        self.assert_owner();
        if !self.paused {
            return;
        }
        self.paused = false;
        emit("unpaused", &serde_json_map(&[]));
    }

    /// Send vault NEAR to the owner. Ignores the agent policy: the owner holds
    /// the funds. Cannot spend the storage reserve.
    pub fn withdraw(&mut self, amount: U128) -> Promise {
        self.assert_owner();
        require!(amount.0 > 0, "amount must be positive");
        require!(
            amount.0 <= self.spendable_yocto(),
            "insufficient vault balance"
        );
        emit(
            "withdrawn",
            &serde_json_map(&[
                ("receiver_id", self.owner_id.to_string()),
                ("amount", amount.0.to_string()),
            ]),
        );
        Promise::new(self.owner_id.clone()).transfer(NearToken::from_yoctonear(amount.0))
    }

    /// Agent payment. Policy misses do not transfer NEAR and do not panic:
    /// the attempt is stored so `get_recent_rejections` can show it.
    /// Calls from any account other than the agent panic.
    pub fn pay(
        &mut self,
        receiver_id: AccountId,
        amount: U128,
        memo: Option<String>,
    ) -> PromiseOrValue<PaymentOutcome> {
        if env::predecessor_account_id() != self.agent_id {
            deny("only the agent can call pay");
        }
        let memo = normalize_memo(memo);
        let amount = amount.0;
        let now = env::block_timestamp();

        if let Some(reason) = self.block_reason(&receiver_id, amount, now) {
            return PromiseOrValue::Value(self.reject(receiver_id, amount, memo, reason, now));
        }

        let payment_id = self.next_payment_id;
        self.next_payment_id += 1;
        self.payment_count += 1;
        self.spends.push(SpendEntry {
            timestamp_ns: now,
            amount,
            payment_id,
            active: true,
        });
        self.push_payment(StoredPayment {
            id: payment_id,
            receiver_id: receiver_id.clone(),
            amount,
            timestamp_ns: now,
            memo: memo.clone(),
            status: PaymentStatus::Pending.to_u8(),
        });

        // Count the spend before the transfer so a second overlapping call
        // cannot pass the same daily-limit check.
        let spent = self.spent_in_window(now);
        emit(
            "payment_allowed",
            &payment_event(
                &receiver_id,
                amount,
                memo.clone(),
                None,
                Some(payment_id),
                spent,
            ),
        );

        let settlement = Self::ext(env::current_account_id())
            .with_static_gas(SETTLEMENT_GAS)
            .on_pay_settled(payment_id, receiver_id.clone(), U128(amount));
        PromiseOrValue::Promise(
            Promise::new(receiver_id)
                .transfer(NearToken::from_yoctonear(amount))
                .then(settlement),
        )
    }

    /// Runs only as the callback of `pay`. Reverts the rolling spend if the
    /// NEAR transfer receipt fails.
    #[private]
    pub fn on_pay_settled(
        &mut self,
        payment_id: u64,
        receiver_id: AccountId,
        amount: U128,
        #[callback_result] result: Result<(), PromiseError>,
    ) -> PaymentOutcome {
        let known = self.spends.iter().any(|spend| {
            spend.active && spend.payment_id == payment_id && spend.amount == amount.0
        });
        require!(known, "unknown payment settlement");
        let now = env::block_timestamp();

        if result.is_err() {
            self.deactivate_spend(payment_id);
            self.set_payment_status(payment_id, PaymentStatus::Reverted);
            return self.reject(
                receiver_id,
                amount.0,
                None,
                BlockReason::TransferFailed,
                now,
            );
        }

        self.set_payment_status(payment_id, PaymentStatus::Settled);
        let spent = self.spent_in_window(now);
        PaymentOutcome {
            allowed: true,
            reason: None,
            receiver_id,
            amount,
            memo: None,
            spent_in_window: U128(spent),
            daily_limit: U128(self.daily_limit),
            payment_id: Some(U64(payment_id)),
            rejection_id: None,
        }
    }

    pub fn get_policy(&self) -> PolicyView {
        let now = env::block_timestamp();
        let spent = self.spent_in_window(now);
        PolicyView {
            version: VERSION.to_string(),
            contract_id: env::current_account_id(),
            owner_id: self.owner_id.clone(),
            agent_id: self.agent_id.clone(),
            paused: self.paused,
            per_payment_limit: U128(self.per_payment_limit),
            daily_limit: U128(self.daily_limit),
            window_ns: U64(WINDOW_NS),
            spent_in_window: U128(spent),
            remaining_in_window: U128(self.daily_limit.saturating_sub(spent)),
            spendable_balance: U128(self.spendable_yocto()),
            allowlist: self.allowlist.clone(),
            payment_count: U64(self.payment_count),
            rejection_count: U64(self.rejection_count),
        }
    }

    pub fn get_spent_today(&self) -> SpentTodayView {
        let now = env::block_timestamp();
        let spent = self.spent_in_window(now);
        SpentTodayView {
            window_ns: U64(WINDOW_NS),
            as_of_timestamp_ns: U64(now),
            spent: U128(spent),
            daily_limit: U128(self.daily_limit),
            remaining: U128(self.daily_limit.saturating_sub(spent)),
        }
    }

    pub fn get_allowlist(&self) -> Vec<AccountId> {
        self.allowlist.clone()
    }

    pub fn get_recent_payments(&self, limit: Option<u32>) -> Vec<PaymentView> {
        recent(self.payments.iter().map(payment_view), limit)
    }

    pub fn get_recent_rejections(&self, limit: Option<u32>) -> Vec<RejectionView> {
        recent(self.rejections.iter().map(rejection_view), limit)
    }
}

impl Contract {
    fn assert_owner(&self) {
        if env::predecessor_account_id() != self.owner_id {
            deny("only the owner can call this method");
        }
    }

    fn is_allowlisted(&self, account_id: &AccountId) -> bool {
        self.allowlist.iter().any(|id| id == account_id)
    }

    fn spendable_yocto(&self) -> u128 {
        let storage = env::storage_byte_cost().as_yoctonear() * u128::from(env::storage_usage());
        let locked = storage.saturating_add(SAFETY_BUFFER_YOCTO);
        env::account_balance().as_yoctonear().saturating_sub(locked)
    }

    fn spent_in_window(&self, now: u64) -> u128 {
        self.spends
            .iter()
            .filter(|spend| spend.active && in_window(spend.timestamp_ns, now))
            .fold(0u128, |total, spend| total.saturating_add(spend.amount))
    }

    fn prune_spends(&mut self, now: u64) {
        self.spends
            .retain(|spend| spend.active && in_window(spend.timestamp_ns, now));
    }

    fn block_reason(
        &mut self,
        receiver_id: &AccountId,
        amount: u128,
        now: u64,
    ) -> Option<BlockReason> {
        if amount == 0 {
            return Some(BlockReason::AmountMustBePositive);
        }
        if self.paused {
            return Some(BlockReason::Paused);
        }
        if !self.is_allowlisted(receiver_id) {
            return Some(BlockReason::NotAllowlisted);
        }
        if amount > self.per_payment_limit {
            return Some(BlockReason::ExceedsPerPaymentLimit);
        }
        self.prune_spends(now);
        if self.spent_in_window(now).saturating_add(amount) > self.daily_limit {
            return Some(BlockReason::ExceedsDailyLimit);
        }
        if amount > self.spendable_yocto() {
            return Some(BlockReason::InsufficientVaultBalance);
        }
        if self.spends.len() >= MAX_SPEND_ENTRIES {
            return Some(BlockReason::WindowLogFull);
        }
        None
    }

    fn reject(
        &mut self,
        receiver_id: AccountId,
        amount: u128,
        memo: Option<String>,
        reason: BlockReason,
        now: u64,
    ) -> PaymentOutcome {
        let rejection_id = self.next_rejection_id;
        self.next_rejection_id += 1;
        self.rejection_count += 1;
        push_capped(
            &mut self.rejections,
            StoredRejection {
                id: rejection_id,
                receiver_id: receiver_id.clone(),
                amount,
                timestamp_ns: now,
                memo: memo.clone(),
                reason: reason.as_str().to_string(),
            },
        );
        let spent = self.spent_in_window(now);
        emit(
            "payment_blocked",
            &payment_event(
                &receiver_id,
                amount,
                memo.clone(),
                Some(reason.as_str()),
                None,
                spent,
            ),
        );
        PaymentOutcome {
            allowed: false,
            reason: Some(reason.as_str().to_string()),
            receiver_id,
            amount: U128(amount),
            memo,
            spent_in_window: U128(spent),
            daily_limit: U128(self.daily_limit),
            payment_id: None,
            rejection_id: Some(U64(rejection_id)),
        }
    }

    fn push_payment(&mut self, payment: StoredPayment) {
        push_capped(&mut self.payments, payment);
    }

    fn deactivate_spend(&mut self, payment_id: u64) {
        for spend in &mut self.spends {
            if spend.payment_id == payment_id {
                spend.active = false;
            }
        }
    }

    fn set_payment_status(&mut self, payment_id: u64, status: PaymentStatus) {
        for payment in &mut self.payments {
            if payment.id == payment_id {
                payment.status = status.to_u8();
            }
        }
    }
}

/// A spend stays in the window until its age reaches 24h. At exactly 24h it
/// drops, so the owner can predict when budget comes back.
fn in_window(timestamp_ns: u64, now: u64) -> bool {
    now.saturating_sub(timestamp_ns) < WINDOW_NS
}

/// Abort the receipt. The reason is logged first so explorers show it even
/// when the runtime reports the abort as a wasm trap.
fn deny(message: &'static str) -> ! {
    env::log_str(message);
    env::panic_str(message);
}

fn validate_limits(per_payment_limit: u128, daily_limit: u128) {
    require!(per_payment_limit > 0, "per-payment limit must be positive");
    require!(
        daily_limit >= per_payment_limit,
        "daily limit must cover one payment"
    );
}

fn normalize_memo(memo: Option<String>) -> Option<String> {
    let memo = memo?;
    let memo = memo.trim();
    if memo.is_empty() {
        return None;
    }
    require!(
        memo.len() <= MAX_MEMO_BYTES,
        "memo is longer than 120 bytes"
    );
    Some(memo.to_string())
}

fn push_capped<T>(log: &mut Vec<T>, item: T) {
    if log.len() >= MAX_LOG_ENTRIES {
        log.remove(0);
    }
    log.push(item);
}

fn recent<T>(items: impl DoubleEndedIterator<Item = T>, limit: Option<u32>) -> Vec<T> {
    let limit = limit.unwrap_or(20).clamp(1, MAX_LOG_ENTRIES as u32) as usize;
    items.rev().take(limit).collect()
}

fn payment_view(payment: &StoredPayment) -> PaymentView {
    PaymentView {
        id: U64(payment.id),
        receiver_id: payment.receiver_id.clone(),
        amount: U128(payment.amount),
        timestamp_ns: U64(payment.timestamp_ns),
        memo: payment.memo.clone(),
        status: PaymentStatus::from_u8(payment.status).as_str().to_string(),
    }
}

fn rejection_view(rejection: &StoredRejection) -> RejectionView {
    RejectionView {
        id: U64(rejection.id),
        receiver_id: rejection.receiver_id.clone(),
        amount: U128(rejection.amount),
        timestamp_ns: U64(rejection.timestamp_ns),
        memo: rejection.memo.clone(),
        reason: rejection.reason.clone(),
    }
}

fn payment_event(
    receiver_id: &AccountId,
    amount: u128,
    memo: Option<String>,
    reason: Option<&str>,
    payment_id: Option<u64>,
    spent_in_window: u128,
) -> serde_json::Value {
    serde_json::json!({
        "receiver_id": receiver_id,
        "amount": amount.to_string(),
        "memo": memo,
        "reason": reason,
        "payment_id": payment_id,
        "spent_in_window": spent_in_window.to_string(),
    })
}

fn serde_json_map(pairs: &[(&str, String)]) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    for (key, value) in pairs {
        map.insert((*key).to_string(), serde_json::Value::String(value.clone()));
    }
    serde_json::Value::Object(map)
}

fn emit(event: &str, data: &serde_json::Value) {
    let body = serde_json::json!({
        "standard": EVENT_STANDARD,
        "version": VERSION,
        "event": event,
        "data": [data],
    });
    env::log_str(&format!("EVENT_JSON:{body}"));
}

#[cfg(test)]
mod tests;
