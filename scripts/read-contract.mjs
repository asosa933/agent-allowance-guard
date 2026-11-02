/**
 * Read the published testnet vault. No key, no wallet, no mainnet.
 *
 *   node scripts/read-contract.mjs
 */

import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { FailoverRpcProvider, JsonRpcProvider, yoctoToNear } from "near-api-js";

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const deployment = JSON.parse(
  fs.readFileSync(path.join(ROOT, "frontend/deployment.json"), "utf8"),
);

if (deployment.networkId !== "testnet") {
  console.error("deployment.json is not testnet");
  process.exit(1);
}
for (const url of deployment.rpcUrls) {
  if (url.includes("mainnet")) {
    console.error(`refusing mainnet RPC ${url}`);
    process.exit(1);
  }
}

const provider = new FailoverRpcProvider(
  deployment.rpcUrls.map((url) => new JsonRpcProvider({ url })),
);

const policy = await provider.callFunction({
  contractId: deployment.contractId,
  method: "get_policy",
  args: {},
});
const rejections = await provider.callFunction({
  contractId: deployment.contractId,
  method: "get_recent_rejections",
  args: { limit: 10 },
});
const agentKeys = await provider.viewAccessKeyList({ accountId: deployment.agentId });

const report = {
  contractId: deployment.contractId,
  paused: policy.paused,
  perPaymentNear: yoctoToNear(String(policy.per_payment_limit), 4),
  dailyLimitNear: yoctoToNear(String(policy.daily_limit), 4),
  spentInWindowNear: yoctoToNear(String(policy.spent_in_window), 4),
  remainingNear: yoctoToNear(String(policy.remaining_in_window), 4),
  allowlist: policy.allowlist,
  agentId: policy.agent_id,
  ownerId: policy.owner_id,
  rejectionReasons: rejections.map((row) => row.reason),
  agentKeys: (agentKeys.keys || []).map((key) => ({
    publicKey: key.public_key,
    permission: key.access_key?.permission,
  })),
};

console.log(JSON.stringify(report, null, 2));
