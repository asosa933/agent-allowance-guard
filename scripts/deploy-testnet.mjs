/**
 * Create a testnet vault, deploy Agent Allowance Guard, and record a demo run.
 *
 * Testnet only. Refuses any RPC whose status is not chain_id "testnet".
 * Writes secret keys only under ~/.near-credentials/testnet/. Never prints them.
 *
 * Usage (from the repository root, after `npm install` in scripts/):
 *   node scripts/deploy-testnet.mjs --publish-demo
 */

import { execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

import {
  Account,
  JsonRpcProvider,
  KeyPair,
  actions,
  base64Decode,
  nearToYocto,
  teraToGas,
} from "near-api-js";

const RPC_URL = "https://rpc.testnet.near.org";
const FAUCET_URL = "https://helper.testnet.near.org/account";
const EXPLORER_TX = "https://testnet.nearblocks.io/txns/";
const EXPLORER_ACCOUNT = "https://testnet.nearblocks.io/address/";
const HERE = path.dirname(fileURLToPath(import.meta.url));
const ROOT = path.resolve(HERE, "..");
const CRED_DIR = path.join(os.homedir(), ".near-credentials", "testnet");
const PUBLISH = process.argv.includes("--publish-demo");

function redact(value) {
  return String(value).replace(/ed25519:[1-9A-HJ-NP-Za-km-z]{80,}/g, "ed25519:[redacted]");
}

function log(message) {
  console.log(redact(message));
}

function fail(message) {
  console.error(redact(message));
  process.exit(1);
}

function yocto(near) {
  return nearToYocto(near).toString();
}

function readJson(file) {
  return JSON.parse(fs.readFileSync(file, "utf8"));
}

function writeJson(file, value) {
  const text = `${JSON.stringify(value, null, 2)}\n`;
  if (/ed25519:[1-9A-HJ-NP-Za-km-z]{80,}/.test(text)) {
    fail(`refusing to write ${file}: it contains a private key`);
  }
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.writeFileSync(file, text);
}

function credentialPath(accountId) {
  return path.join(CRED_DIR, `${accountId}.json`);
}

function saveKey(accountId, keyPair) {
  if (!CRED_DIR.startsWith(os.homedir())) fail("credential directory escaped the home directory");
  if (CRED_DIR.startsWith(ROOT)) fail("refusing to store keys inside the repository");
  fs.mkdirSync(CRED_DIR, { recursive: true });
  const file = credentialPath(accountId);
  const body = {
    account_id: accountId,
    public_key: keyPair.getPublicKey().toString(),
    private_key: keyPair.toString(),
  };
  fs.writeFileSync(file, `${JSON.stringify(body, null, 2)}\n`, { mode: 0o600 });
  return file;
}

function loadOrCreateKey(accountId) {
  const file = credentialPath(accountId);
  if (fs.existsSync(file)) {
    const saved = readJson(file);
    return KeyPair.fromString(saved.private_key);
  }
  const keyPair = KeyPair.fromRandom("ed25519");
  saveKey(accountId, keyPair);
  return keyPair;
}

async function assertTestnet(provider) {
  if (RPC_URL.includes("mainnet")) fail("RPC url contains mainnet");
  const status = await provider.viewNodeStatus();
  const chainId = status.chain_id;
  if (chainId !== "testnet") fail(`refusing to continue, chain_id=${chainId}`);
}

async function accountExists(provider, accountId) {
  try {
    await provider.viewAccount({ accountId });
    return true;
  } catch {
    return false;
  }
}

async function createFromFaucet(accountId, publicKey) {
  const response = await fetch(FAUCET_URL, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({
      newAccountId: accountId,
      newAccountPublicKey: publicKey,
    }),
  });
  const text = await response.text();
  if (!response.ok) {
    throw new Error(`faucet ${response.status}: ${text.slice(0, 300)}`);
  }
  log(`faucet created ${accountId}`);
}

async function waitForAccount(provider, accountId) {
  for (let attempt = 1; attempt <= 20; attempt += 1) {
    if (await accountExists(provider, accountId)) return;
    await new Promise((resolve) => setTimeout(resolve, 1500));
  }
  fail(`timed out waiting for ${accountId}`);
}

function buildWasm() {
  const wasmPath = path.join(
    ROOT,
    "target/wasm32-unknown-unknown/release/agent_allowance_guard.wasm",
  );
  log("building contract wasm");
  execSync(
    "cargo build -p agent-allowance-guard --release --target wasm32-unknown-unknown",
    { cwd: ROOT, stdio: "inherit" },
  );
  if (!fs.existsSync(wasmPath)) fail(`wasm missing at ${wasmPath}`);
  return fs.readFileSync(wasmPath);
}

function txHashOf(response) {
  return response?.transaction?.hash || response?.transaction_outcome?.id || null;
}

function logsOf(response) {
  const logs = [];
  for (const receipt of response?.receipts_outcome || []) {
    for (const line of receipt.outcome?.logs || []) logs.push(line);
  }
  return logs;
}

function failureText(response) {
  const status = response?.status;
  if (status && typeof status === "object" && status.Failure) {
    return JSON.stringify(status.Failure);
  }
  return "";
}

function returnValue(response) {
  const status = response?.status;
  if (!status || typeof status !== "object" || status.SuccessValue === undefined) return null;
  if (!status.SuccessValue) return null;
  const bytes = base64Decode(status.SuccessValue);
  const text = new TextDecoder().decode(bytes);
  if (!text) return null;
  try {
    return JSON.parse(text);
  } catch {
    return text;
  }
}

function summarize(response) {
  const status = response?.status;
  const success = Boolean(status && typeof status === "object" && "SuccessValue" in status);
  return {
    hash: txHashOf(response),
    success,
    logs: logsOf(response),
    value: success ? returnValue(response) : null,
    failure: success ? "" : failureText(response),
  };
}

async function submit(account, methodName, args, gasNear = "80") {
  try {
    const response = await account.signAndSendTransaction({
      receiverId: contractId,
      actions: [actions.functionCall(methodName, args, teraToGas(gasNear), 0n)],
      throwOnFailure: false,
    });
    return summarize(response);
  } catch (error) {
    return {
      hash: null,
      success: false,
      logs: [],
      value: null,
      failure: redact(error?.message || error),
    };
  }
}

let contractId = "";

async function main() {
  const provider = new JsonRpcProvider({ url: RPC_URL });
  await assertTestnet(provider);

  const stateFile = path.join(CRED_DIR, "agent-allowance-guard.json");
  let state = fs.existsSync(stateFile) ? readJson(stateFile) : null;
  if (!state) {
    const suffix = Math.random().toString(36).slice(2, 8);
    contractId = `aag-${suffix}.testnet`;
    const rootKey = loadOrCreateKey(contractId);
    try {
      await createFromFaucet(contractId, rootKey.getPublicKey().toString());
    } catch (error) {
      fail(redact(error.message || error));
    }
    state = {
      contractId,
      ownerId: `owner.${contractId}`,
      agentId: `agent.${contractId}`,
      vendorId: `vendor.${contractId}`,
    };
    writeJson(stateFile, state);
  }
  contractId = state.contractId;
  const { ownerId, agentId, vendorId } = state;
  log(`vault ${contractId}`);

  await waitForAccount(provider, contractId);
  const rootKey = loadOrCreateKey(contractId);
  const root = new Account(contractId, provider, rootKey.toString());
  const ownerKey = loadOrCreateKey(ownerId);
  const agentKey = loadOrCreateKey(agentId);
  const vendorKey = loadOrCreateKey(vendorId);

  const code = await provider.viewContractCode({ contractId }).catch(() => null);
  const hasCode = Boolean(code?.code?.length);
  if (!hasCode) {
    const wasm = buildWasm();
    log(`deploying ${wasm.length} bytes`);
    await root.deployContract(wasm);
    await root.callFunction({
      contractId,
      methodName: "new",
      args: {
        owner_id: ownerId,
        agent_id: agentId,
        per_payment_limit: yocto("0.5"),
        daily_limit: yocto("1"),
      },
      gas: teraToGas("30"),
    });
    log("contract initialized");
  }

  if (!(await accountExists(provider, ownerId))) {
    log(`creating ${ownerId}`);
    await root.createSubAccount({
      accountOrPrefix: "owner",
      publicKey: ownerKey.getPublicKey().toString(),
      nearToTransfer: yocto("1"),
    });
  }
  if (!(await accountExists(provider, vendorId))) {
    log(`creating ${vendorId}`);
    await root.createSubAccount({
      accountOrPrefix: "vendor",
      publicKey: vendorKey.getPublicKey().toString(),
      nearToTransfer: yocto("0.5"),
    });
  }
  if (!(await accountExists(provider, agentId))) {
    log(`creating ${agentId} with a function-call key only`);
    await root.signAndSendTransaction({
      receiverId: agentId,
      actions: [
        actions.createAccount(),
        actions.transfer(nearToYocto("1.2")),
        actions.addFunctionCallAccessKey(
          agentKey.getPublicKey(),
          contractId,
          ["pay"],
          nearToYocto("0.5"),
        ),
      ],
    });
  }

  const owner = new Account(ownerId, provider, ownerKey.toString());
  const agent = new Account(agentId, provider, agentKey.toString());
  const policy = await provider.callFunction({
    contractId,
    method: "get_policy",
    args: {},
  });
  if (!policy.allowlist.includes(vendorId)) {
    await owner.callFunction({
      contractId,
      methodName: "add_recipient",
      args: { account_id: vendorId },
      gas: teraToGas("20"),
    });
    log("vendor allowlisted");
  }

  if (policy.payment_count !== "0" && policy.payment_count !== 0) {
    log("demo payments already exist; not sending them again");
  } else {
    const steps = [
      {
        title: "Pay 0.3 NEAR to the allowlisted vendor",
        narrator: "The agent pays a 0.3 NEAR invoice. That is under the 0.5 NEAR per-payment cap.",
        who: agent,
        signer: "agent",
        method: "pay",
        args: { receiver_id: vendorId, amount: yocto("0.3"), memo: "vendor invoice 0.3" },
        amountNear: "0.3",
        receiverId: vendorId,
        expect: "allowed",
      },
      {
        title: "Pay 0.6 NEAR to the same vendor",
        narrator: "The agent tries 0.6 NEAR. The per-payment limit is 0.5, so the vault refuses.",
        who: agent,
        signer: "agent",
        method: "pay",
        args: { receiver_id: vendorId, amount: yocto("0.6"), memo: "oversized invoice" },
        amountNear: "0.6",
        receiverId: vendorId,
        expect: "blocked",
        expectReason: "exceeds_per_payment_limit",
      },
      {
        title: "Pay 0.4 NEAR to the vendor",
        narrator: "0.4 NEAR is inside the per-payment cap. Spent today becomes 0.7 of 1.0.",
        who: agent,
        signer: "agent",
        method: "pay",
        args: { receiver_id: vendorId, amount: yocto("0.4"), memo: "vendor invoice 0.4" },
        amountNear: "0.4",
        receiverId: vendorId,
        expect: "allowed",
      },
      {
        title: "Pay another 0.4 NEAR",
        narrator: "0.7 plus 0.4 would pass 1 NEAR in the rolling day. The vault blocks it.",
        who: agent,
        signer: "agent",
        method: "pay",
        args: { receiver_id: vendorId, amount: yocto("0.4"), memo: "would exceed the day" },
        amountNear: "0.4",
        receiverId: vendorId,
        expect: "blocked",
        expectReason: "exceeds_daily_limit",
      },
      {
        title: "Pay 0.1 NEAR to an unknown account",
        narrator: "The agent names a recipient the owner never allowlisted.",
        who: agent,
        signer: "agent",
        method: "pay",
        args: {
          receiver_id: "not-allowlisted.testnet",
          amount: yocto("0.1"),
          memo: "unknown recipient",
        },
        amountNear: "0.1",
        receiverId: "not-allowlisted.testnet",
        expect: "blocked",
        expectReason: "not_allowlisted",
      },
      {
        title: "Owner pauses the vault",
        narrator: "The owner hits pause. The agent key cannot do this.",
        who: owner,
        signer: "owner",
        method: "pause",
        args: {},
        expect: "paused",
      },
      {
        title: "Agent pays while the vault is paused",
        narrator: "The same 0.1 NEAR payment that would otherwise be valid is blocked.",
        who: agent,
        signer: "agent",
        method: "pay",
        args: { receiver_id: vendorId, amount: yocto("0.1"), memo: "while paused" },
        amountNear: "0.1",
        receiverId: vendorId,
        expect: "blocked",
        expectReason: "paused",
      },
      {
        title: "Owner unpauses the vault",
        narrator: "The owner lifts the pause. The policy limits stay in place.",
        who: owner,
        signer: "owner",
        method: "unpause",
        args: {},
        expect: "unpaused",
      },
      {
        title: "Agent pays 0.1 NEAR after the pause",
        narrator: "An in-policy payment works again. Spent today is 0.8 of 1.0.",
        who: agent,
        signer: "agent",
        method: "pay",
        args: { receiver_id: vendorId, amount: yocto("0.1"), memo: "after unpause" },
        amountNear: "0.1",
        receiverId: vendorId,
        expect: "allowed",
      },
      {
        title: "Agent key tries to withdraw the vault",
        narrator: "The function-call key is limited to pay. The protocol rejects withdraw.",
        who: agent,
        signer: "agent",
        method: "withdraw",
        args: { amount: "1" },
        expect: "rejected",
        expectText: "withdraw",
      },
      {
        title: "Owner tries to call pay",
        narrator: "The owner holds the funds, but pay accepts only the agent account.",
        who: owner,
        signer: "owner",
        method: "pay",
        args: { receiver_id: vendorId, amount: yocto("0.1"), memo: "owner is not the agent" },
        amountNear: "0.1",
        receiverId: vendorId,
        expect: "rejected",
        expectText: "only the agent can call pay",
      },
      {
        title: "Owner withdraws 0.05 NEAR",
        narrator: "The owner can still take funds back. The agent cannot.",
        who: owner,
        signer: "owner",
        method: "withdraw",
        args: { amount: yocto("0.05") },
        amountNear: "0.05",
        expect: "withdrawn",
      },
    ];

    const attempts = [];
    for (const [index, step] of steps.entries()) {
      log(`step ${index + 1}: ${step.title}`);
      const result = await submit(step.who, step.method, step.args, step.method === "pay" ? "100" : "40");
      const observed = classify(step, result);
      const attempt = {
        step: index + 1,
        title: step.title,
        narrator: step.narrator,
        signer: step.signer,
        method: step.method,
        receiverId: step.receiverId || null,
        amountYocto: step.args.amount || null,
        amountNear: step.amountNear || null,
        expected: step.expect,
        observed: observed.observed,
        reason: observed.reason,
        txHash: result.hash,
        explorerUrl: result.hash ? `${EXPLORER_TX}${result.hash}` : null,
        success: result.success,
        logs: result.logs,
      };
      attempts.push(attempt);
      if (observed.observed !== step.expect) {
        writeJson(path.join(ROOT, "frontend/demo-run.partial.json"), { attempts });
        fail(`step ${index + 1} observed ${observed.observed}, expected ${step.expect}. ${result.failure}`);
      }
      if (step.expectReason && observed.reason !== step.expectReason) {
        fail(`step ${index + 1} reason ${observed.reason}, expected ${step.expectReason}`);
      }
    }

    const finalPolicy = await provider.callFunction({
      contractId,
      method: "get_policy",
      args: {},
    });
    const demo = {
      networkId: "testnet",
      contractId,
      ownerId,
      agentId,
      vendorId,
      recordedAt: new Date().toISOString(),
      summary:
        "A recorded testnet run. The agent key calls pay. Out-of-policy payments stay in the vault, and the key cannot withdraw or pause.",
      attempts,
      finalPolicy,
    };

    const deployment = {
      networkId: "testnet",
      rpcUrls: [RPC_URL, "https://test.rpc.fastnear.com"],
      contractId,
      ownerId,
      agentId,
      vendorId,
      explorerAccount: EXPLORER_ACCOUNT,
      explorerTx: EXPLORER_TX,
      perPaymentYocto: yocto("0.5"),
      dailyLimitYocto: yocto("1"),
      agentPublicKey: agentKey.getPublicKey().toString(),
    };

    if (PUBLISH) {
      writeJson(path.join(ROOT, "frontend/deployment.json"), deployment);
      writeJson(path.join(ROOT, "frontend/demo-run.json"), demo);
      log("wrote frontend/deployment.json and frontend/demo-run.json");
    } else {
      log("skipping frontend files (pass --publish-demo to write them)");
    }
  }

  const keys = await provider.viewAccessKeyList({ accountId: contractId });
  const fullKeys = (keys.keys || []).filter((key) => {
    const permission = key.access_key?.permission;
    return permission === "FullAccess" || permission?.FullAccess;
  });
  if (PUBLISH && fullKeys.length > 0) {
    log("removing the vault account full-access key so the contract account cannot redeploy or sign");
    await root.deleteKey(rootKey.getPublicKey().toString());
    log("vault account key removed");
  }

  log(`done ${contractId}`);
  log(`explorer ${EXPLORER_ACCOUNT}${contractId}`);
}

function classify(step, result) {
  const blob = `${result.failure}\n${result.logs.join("\n")}`;
  if (step.expect === "rejected") {
    const needle = step.expectText || "only the agent can call pay";
    if (blob.includes(needle)) {
      return { observed: "rejected", reason: needle };
    }
    return { observed: "unexpected", reason: blob.slice(0, 240) };
  }
  if (result.value && typeof result.value === "object" && "allowed" in result.value) {
    return {
      observed: result.value.allowed ? "allowed" : "blocked",
      reason: result.value.reason || null,
    };
  }
  if (step.expect === "paused" && result.success) return { observed: "paused", reason: null };
  if (step.expect === "unpaused" && result.success) return { observed: "unpaused", reason: null };
  if (step.expect === "withdrawn" && result.success) return { observed: "withdrawn", reason: null };
  if (!result.success) return { observed: "rejected", reason: result.failure.slice(0, 180) };
  return { observed: "unknown", reason: null };
}

main().catch((error) => fail(redact(error?.stack || error)));
