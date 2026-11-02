//! Sandbox tests against the compiled contract wasm.
//!
//! The rolling 24h clock is covered by the contract unit tests. These tests
//! check real NEAR transfers, rejected calls, and a function-call access key
//! that can sign `pay` and cannot sign `withdraw`.

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use anyhow::Context;
use near_workspaces::network::Sandbox;
use near_workspaces::result::ExecutionFinalResult;
use near_workspaces::types::{AccessKey, Gas, KeyType, NearToken, SecretKey};
use near_workspaces::{Account, AccountId, Contract, Worker};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::OnceCell;

const ONE_NEAR: u128 = 1_000_000_000_000_000_000_000_000;
const PER_PAYMENT: u128 = 500_000_000_000_000_000_000_000;
const DAILY: u128 = 1_000_000_000_000_000_000_000_000;
const PAY_GAS: Gas = Gas::from_tgas(100);

static WASM: OnceCell<Arc<Vec<u8>>> = OnceCell::const_new();

#[derive(Debug, Deserialize)]
struct PaymentOutcome {
    allowed: bool,
    reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PolicyView {
    paused: bool,
    spent_in_window: String,
    agent_id: String,
    allowlist: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct RejectionView {
    reason: String,
}

struct Env {
    worker: Worker<Sandbox>,
    contract: Contract,
    owner: Account,
    agent: Account,
    vendor: Account,
    stranger: Account,
}

async fn wasm() -> &'static [u8] {
    WASM.get_or_init(|| async {
        let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("workspace root")
            .to_path_buf();
        // Separate target dir so this build does not deadlock on the cargo
        // lock held by the test run itself.
        let target_dir = workspace.join("target/near-wasm");
        let output = Command::new("cargo")
            .current_dir(&workspace)
            .env("CARGO_TARGET_DIR", &target_dir)
            .args([
                "build",
                "-p",
                "agent-allowance-guard",
                "--release",
                "--target",
                "wasm32-unknown-unknown",
            ])
            .output()
            .expect("spawn cargo to build the contract wasm");
        if !output.status.success() {
            panic!(
                "contract wasm build failed\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let path = target_dir
            .join("wasm32-unknown-unknown/release")
            .join("agent_allowance_guard.wasm");
        Arc::new(
            std::fs::read(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display())),
        )
    })
    .await
}

async fn setup() -> anyhow::Result<Env> {
    let worker = near_workspaces::sandbox().await?;
    let contract = worker.dev_deploy(wasm().await).await?;
    let owner = worker.dev_create_account().await?;
    let agent = worker.dev_create_account().await?;
    let vendor = worker.dev_create_account().await?;
    let stranger = worker.dev_create_account().await?;

    contract
        .call("new")
        .args_json(json!({
            "owner_id": owner.id(),
            "agent_id": agent.id(),
            "per_payment_limit": PER_PAYMENT.to_string(),
            "daily_limit": DAILY.to_string(),
        }))
        .transact()
        .await?
        .into_result()?;

    owner
        .call(contract.id(), "add_recipient")
        .args_json(json!({ "account_id": vendor.id() }))
        .transact()
        .await?
        .into_result()?;

    owner
        .call(contract.id(), "fund")
        .args_json(json!({}))
        .deposit(NearToken::from_near(20))
        .transact()
        .await?
        .into_result()?;

    Ok(Env {
        worker,
        contract,
        owner,
        agent,
        vendor,
        stranger,
    })
}

async fn pay(
    caller: &Account,
    contract_id: &AccountId,
    receiver: &AccountId,
    amount: u128,
) -> anyhow::Result<ExecutionFinalResult> {
    Ok(caller
        .call(contract_id, "pay")
        .args_json(json!({
            "receiver_id": receiver,
            "amount": amount.to_string(),
            "memo": "sandbox",
        }))
        .gas(PAY_GAS)
        .transact()
        .await?)
}

fn parse_outcome(result: ExecutionFinalResult) -> anyhow::Result<(PaymentOutcome, Vec<String>)> {
    let success = result.into_result()?;
    let logs = success
        .logs()
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>();
    let outcome = success.json().context("payment outcome json")?;
    Ok((outcome, logs))
}

async fn yocto_balance(account: &Account) -> anyhow::Result<u128> {
    Ok(account.view_account().await?.balance.as_yoctonear())
}

#[tokio::test]
async fn allowed_payment_transfers_near() -> anyhow::Result<()> {
    let env = setup().await?;
    let before_vendor = yocto_balance(&env.vendor).await?;
    let before_contract = yocto_balance(env.contract.as_account()).await?;

    let result = pay(
        &env.agent,
        env.contract.id(),
        env.vendor.id(),
        PER_PAYMENT / 2,
    )
    .await?;
    let (outcome, logs) = parse_outcome(result)?;
    assert!(outcome.allowed, "{outcome:?}");
    assert!(
        logs.iter()
            .any(|line| line.contains("\"event\":\"payment_allowed\"")),
        "missing NEP-297 payment_allowed log: {logs:?}"
    );

    let after_vendor = yocto_balance(&env.vendor).await?;
    let after_contract = yocto_balance(env.contract.as_account()).await?;
    assert_eq!(after_vendor - before_vendor, PER_PAYMENT / 2);
    assert!(before_contract - after_contract >= PER_PAYMENT / 2);

    let policy: PolicyView = env
        .contract
        .view("get_policy")
        .args_json(json!({}))
        .await?
        .json()?;
    assert_eq!(policy.spent_in_window, (PER_PAYMENT / 2).to_string());
    assert_eq!(policy.agent_id, env.agent.id().to_string());
    assert!(!policy.paused);
    assert_eq!(policy.allowlist, vec![env.vendor.id().to_string()]);
    Ok(())
}

#[tokio::test]
async fn over_per_payment_limit_moves_no_funds() -> anyhow::Result<()> {
    let env = setup().await?;
    let before = yocto_balance(&env.vendor).await?;
    let (outcome, logs) = parse_outcome(
        pay(
            &env.agent,
            env.contract.id(),
            env.vendor.id(),
            PER_PAYMENT + 1,
        )
        .await?,
    )?;
    assert!(!outcome.allowed);
    assert_eq!(outcome.reason.as_deref(), Some("exceeds_per_payment_limit"));
    assert!(logs.iter().any(|line| line.contains("payment_blocked")));
    assert_eq!(yocto_balance(&env.vendor).await?, before);

    let rejections: Vec<RejectionView> = env
        .contract
        .view("get_recent_rejections")
        .args_json(json!({ "limit": 5 }))
        .await?
        .json()?;
    assert_eq!(rejections[0].reason, "exceeds_per_payment_limit");
    Ok(())
}

#[tokio::test]
async fn over_daily_limit_moves_no_funds() -> anyhow::Result<()> {
    let env = setup().await?;
    parse_outcome(pay(&env.agent, env.contract.id(), env.vendor.id(), PER_PAYMENT).await?)?;
    parse_outcome(pay(&env.agent, env.contract.id(), env.vendor.id(), PER_PAYMENT).await?)?;
    let before = yocto_balance(&env.vendor).await?;
    let (outcome, _) =
        parse_outcome(pay(&env.agent, env.contract.id(), env.vendor.id(), 1).await?)?;
    assert!(!outcome.allowed);
    assert_eq!(outcome.reason.as_deref(), Some("exceeds_daily_limit"));
    assert_eq!(yocto_balance(&env.vendor).await?, before);

    let policy: PolicyView = env
        .contract
        .view("get_policy")
        .args_json(json!({}))
        .await?
        .json()?;
    assert_eq!(policy.spent_in_window, DAILY.to_string());
    Ok(())
}

#[tokio::test]
async fn non_allowlisted_recipient_is_blocked() -> anyhow::Result<()> {
    let env = setup().await?;
    let before = yocto_balance(&env.stranger).await?;
    let (outcome, _) =
        parse_outcome(pay(&env.agent, env.contract.id(), env.stranger.id(), 1).await?)?;
    assert!(!outcome.allowed);
    assert_eq!(outcome.reason.as_deref(), Some("not_allowlisted"));
    assert_eq!(yocto_balance(&env.stranger).await?, before);
    Ok(())
}

#[tokio::test]
async fn paused_vault_blocks_payment_until_unpaused() -> anyhow::Result<()> {
    let env = setup().await?;
    env.owner
        .call(env.contract.id(), "pause")
        .transact()
        .await?
        .into_result()?;
    let (outcome, _) =
        parse_outcome(pay(&env.agent, env.contract.id(), env.vendor.id(), 1).await?)?;
    assert_eq!(outcome.reason.as_deref(), Some("paused"));

    env.owner
        .call(env.contract.id(), "unpause")
        .transact()
        .await?
        .into_result()?;
    let (outcome, _) =
        parse_outcome(pay(&env.agent, env.contract.id(), env.vendor.id(), 1).await?)?;
    assert!(outcome.allowed);
    Ok(())
}

#[tokio::test]
async fn non_agent_caller_cannot_pay() -> anyhow::Result<()> {
    let env = setup().await?;
    for caller in [&env.owner, &env.stranger] {
        let result = pay(caller, env.contract.id(), env.vendor.id(), 1).await?;
        assert!(
            result.is_failure(),
            "{} should not be able to pay",
            caller.id()
        );
        let text = format!("{result:?}");
        assert!(
            text.contains("only the agent can call pay"),
            "unexpected error for {}: {text}",
            caller.id()
        );
    }
    Ok(())
}

#[tokio::test]
async fn owner_methods_reject_the_agent_and_owner_can_withdraw() -> anyhow::Result<()> {
    let env = setup().await?;
    let cases = [
        ("pause", json!({})),
        (
            "set_limits",
            json!({"per_payment_limit": "1", "daily_limit": "1"}),
        ),
        ("add_recipient", json!({"account_id": env.stranger.id()})),
        ("set_agent", json!({"agent_id": env.stranger.id()})),
        ("withdraw", json!({"amount": "1"})),
    ];
    for (method, args) in cases {
        let result = env
            .agent
            .call(env.contract.id(), method)
            .args_json(args)
            .transact()
            .await?;
        assert!(result.is_failure(), "{method} should fail for the agent");
        let text = format!("{result:?}");
        assert!(
            text.contains("only the owner can call this method"),
            "{method}: {text}"
        );
    }

    let before = yocto_balance(&env.owner).await?;
    env.owner
        .call(env.contract.id(), "withdraw")
        .args_json(json!({ "amount": ONE_NEAR.to_string() }))
        .gas(PAY_GAS)
        .transact()
        .await?
        .into_result()?;
    let after = yocto_balance(&env.owner).await?;
    assert!(
        after + ONE_NEAR / 10 > before + ONE_NEAR,
        "owner balance {before} -> {after}, expected about {ONE_NEAR} more minus gas"
    );

    let stranger = env
        .stranger
        .call(env.contract.id(), "withdraw")
        .args_json(json!({ "amount": "1" }))
        .transact()
        .await?;
    assert!(stranger.is_failure());
    let text = format!("{stranger:?}");
    assert!(
        text.contains("only the owner can call this method"),
        "{text}"
    );
    Ok(())
}

#[tokio::test]
async fn missing_receiver_reverts_the_spend() -> anyhow::Result<()> {
    let env = setup().await?;
    let missing: AccountId = "missing-vendor.test".parse()?;
    env.owner
        .call(env.contract.id(), "add_recipient")
        .args_json(json!({ "account_id": missing }))
        .transact()
        .await?
        .into_result()?;

    let (outcome, _) = parse_outcome(pay(&env.agent, env.contract.id(), &missing, 1).await?)?;
    assert!(!outcome.allowed);
    assert_eq!(outcome.reason.as_deref(), Some("transfer_failed"));

    let policy: PolicyView = env
        .contract
        .view("get_policy")
        .args_json(json!({}))
        .await?
        .json()?;
    assert_eq!(policy.spent_in_window, "0");
    Ok(())
}

#[tokio::test]
async fn function_call_key_can_pay_and_cannot_withdraw() -> anyhow::Result<()> {
    let env = setup().await?;
    let limited_key = SecretKey::from_random(KeyType::ED25519);
    let full_key = env.agent.secret_key().public_key();

    env.agent
        .batch(env.agent.id())
        .add_key(
            limited_key.public_key(),
            AccessKey::function_call_access(
                env.contract.id(),
                &["pay"],
                Some(NearToken::from_near(5)),
            ),
        )
        .delete_key(full_key)
        .transact()
        .await?
        .into_result()?;

    let keys = env.agent.view_access_keys().await?;
    assert_eq!(
        keys.len(),
        1,
        "agent should have only the function-call key: {keys:?}"
    );
    match &keys[0].access_key.permission {
        near_workspaces::types::AccessKeyPermission::FunctionCall(permission) => {
            assert_eq!(permission.method_names, vec!["pay".to_string()]);
            assert_eq!(permission.receiver_id, env.contract.id().to_string());
        }
        other => panic!("expected a function-call key, got {other:?}"),
    }

    let limited = Account::from_secret_key(env.agent.id().clone(), limited_key, &env.worker);
    let before = yocto_balance(&env.vendor).await?;
    let (outcome, _) = parse_outcome(
        pay(
            &limited,
            env.contract.id(),
            env.vendor.id(),
            PER_PAYMENT / 5,
        )
        .await?,
    )?;
    assert!(outcome.allowed, "{outcome:?}");
    assert_eq!(yocto_balance(&env.vendor).await? - before, PER_PAYMENT / 5);

    // The protocol rejects the transaction before the contract runs, so
    // workspaces returns a broadcast error rather than a failed receipt.
    let withdraw = limited
        .call(env.contract.id(), "withdraw")
        .args_json(json!({ "amount": "1" }))
        .transact()
        .await;
    let withdraw_text = format!("{withdraw:?}");
    assert!(
        withdraw_text.contains("MethodNameMismatch") && withdraw_text.contains("withdraw"),
        "expected the access key to reject withdraw, got {withdraw_text}"
    );

    let pause = limited
        .call(env.contract.id(), "pause")
        .args_json(json!({}))
        .transact()
        .await;
    let pause_text = format!("{pause:?}");
    assert!(
        pause_text.contains("MethodNameMismatch") && pause_text.contains("pause"),
        "expected the access key to reject pause, got {pause_text}"
    );
    Ok(())
}
