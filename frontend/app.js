import {
  FailoverRpcProvider,
  JsonRpcProvider,
  yoctoToNear,
} from "https://cdn.jsdelivr.net/npm/near-api-js@7.4.0/+esm";

const REASONS = {
  exceeds_per_payment_limit: "Over the per-payment limit",
  exceeds_daily_limit: "Over the rolling 24 hour limit",
  not_allowlisted: "Recipient is not on the allowlist",
  paused: "The vault is paused",
  amount_must_be_positive: "Amount must be positive",
  insufficient_vault_balance: "The vault cannot cover this payment",
  window_log_full: "Too many payments are still inside the 24 hour window",
  transfer_failed: "The NEAR transfer failed and the spend was reverted",
  method_not_allowed: "The access key cannot call this method, so the node never included a transaction",
};

const $ = (id) => document.getElementById(id);

let deployment;
let demo;
let timer;
let playIndex = -1;

function near(yocto) {
  if (yocto === undefined || yocto === null) return "—";
  const text = yoctoToNear(String(yocto), 4);
  return `${text} NEAR`;
}

function reasonText(reason) {
  if (!reason) return "";
  return REASONS[reason] || reason;
}

function accountUrl(accountId) {
  return `${deployment.explorerAccount}${accountId}`;
}

function txUrl(hash) {
  return `${deployment.explorerTx}${hash}`;
}

async function loadJson(path) {
  const response = await fetch(path);
  if (!response.ok) throw new Error(`${path} returned ${response.status}`);
  return response.json();
}

function provider() {
  return new FailoverRpcProvider(
    deployment.rpcUrls.map((url) => new JsonRpcProvider({ url })),
  );
}

async function callView(rpc, method, args) {
  return rpc.callFunction({
    contractId: deployment.contractId,
    method,
    args,
  });
}

function showNotice(text, isError = false) {
  const notice = $("notice");
  notice.hidden = false;
  notice.innerHTML = `<p>${text}</p>`;
  notice.style.borderColor = isError ? "var(--coral)" : "var(--line)";
}

function renderAttempts() {
  const list = $("attempts");
  list.innerHTML = "";
  for (const attempt of demo.attempts) {
    const item = document.createElement("li");
    const button = document.createElement("button");
    button.type = "button";
    button.className = `attempt${attempt.step === playIndex + 1 ? " active" : ""}`;
    const succeeded = ["allowed", "paused", "unpaused", "withdrawn"].includes(attempt.observed);
    const badgeClass = succeeded ? "allowed" : "blocked";
    const amount = attempt.amountNear ? `${attempt.amountNear} NEAR` : "no NEAR moved";
    button.innerHTML = `
      <span class="step-no">${String(attempt.step).padStart(2, "0")}</span>
      <span>
        <strong>${attempt.title}</strong>
        <p class="detail">${attempt.narrator}</p>
        <p class="detail">${amount}${attempt.reason ? ` · ${reasonText(attempt.reason)}` : ""}${attempt.explorerUrl ? ` · <a href="${attempt.explorerUrl}" target="_blank" rel="noopener">transaction</a>` : " · rejected before inclusion, so there is no transaction hash"}</p>
      </span>
      <span class="badge ${badgeClass}">${attempt.observed}</span>
    `;
    button.addEventListener("click", (event) => {
      if (event.target.closest("a")) return;
      playIndex = attempt.step - 1;
      renderAttempts();
    });
    item.appendChild(button);
    list.appendChild(item);
  }
}

function renderPolicy(policy) {
  $("policy").hidden = false;
  $("per-payment").textContent = near(policy.per_payment_limit);
  $("spent").textContent = near(policy.spent_in_window);
  const spent = Number(yoctoToNear(String(policy.spent_in_window), 6));
  const limit = Number(yoctoToNear(String(policy.daily_limit), 6));
  const pct = limit > 0 ? Math.min(100, (spent / limit) * 100) : 0;
  $("spent-bar").style.width = `${pct}%`;
  $("spent-caption").textContent = `${near(policy.spent_in_window)} of ${near(policy.daily_limit)} used in the last 24 hours. ${near(policy.remaining_in_window)} left.`;
  $("balance").textContent = near(policy.spendable_balance);
  $("paused").textContent = policy.paused ? "Paused" : "Active";
  $("paused").style.color = policy.paused ? "var(--coral)" : "var(--lime)";
  $("roles").textContent = `Owner ${policy.owner_id} · Agent ${policy.agent_id}`;

  const allow = $("allowlist");
  allow.innerHTML = "";
  for (const accountId of policy.allowlist) {
    const item = document.createElement("li");
    item.className = "mono";
    const link = document.createElement("a");
    link.href = accountUrl(accountId);
    link.target = "_blank";
    link.rel = "noopener";
    link.textContent = accountId;
    item.appendChild(link);
    allow.appendChild(item);
  }
  if (policy.allowlist.length === 0) {
    allow.innerHTML = "<li>No recipients yet.</li>";
  }
}

function renderRows(target, rows, empty) {
  target.innerHTML = "";
  if (!rows.length) {
    target.innerHTML = `<p class="muted">${empty}</p>`;
    return;
  }
  for (const row of rows) {
    const card = document.createElement("article");
    const title = row.reason
      ? reasonText(row.reason)
      : `${row.status} payment`;
    card.innerHTML = `
      <strong>${title}</strong>
      <p class="detail mono">${near(row.amount)} → ${row.receiver_id}</p>
      <p class="detail">${row.memo ? `${row.memo} · ` : ""}#${row.id}</p>
    `;
    target.appendChild(card);
  }
}

function renderKeys(agentKeys, contractKeys) {
  const root = $("keys");
  root.innerHTML = "";
  const blocks = [
    ["Agent account", deployment.agentId, agentKeys],
    ["Vault account", deployment.contractId, contractKeys],
  ];
  for (const [label, accountId, keys] of blocks) {
    const box = document.createElement("div");
    box.className = "key";
    const lines = (keys || []).map(describeKey);
    box.innerHTML = `
      <strong>${label}</strong>
      <p class="detail mono">${accountId}</p>
      <p class="detail">${lines.length ? lines.join("<br />") : "No access keys on this account."}</p>
    `;
    root.appendChild(box);
  }
}

function describeKey(key) {
  const permission = key.access_key?.permission;
  const publicKey = key.public_key || "";
  if (permission === "FullAccess" || permission?.FullAccess) {
    return `Full access key ${publicKey}. This key can change the account.`;
  }
  const call = permission?.FunctionCall;
  if (call) {
    const methods = (call.method_names || []).join(", ") || "any method";
    return `Function-call key ${publicKey}. Receiver ${call.receiver_id}. Methods: ${methods}. The allowance pays gas only; the vault holds the funds.`;
  }
  return `${publicKey} ${JSON.stringify(permission)}`;
}

async function refresh() {
  const rpc = provider();
  const [policy, payments, rejections, agentKeys, contractKeys] = await Promise.all([
    callView(rpc, "get_policy", {}),
    callView(rpc, "get_recent_payments", { limit: 20 }),
    callView(rpc, "get_recent_rejections", { limit: 20 }),
    rpc.viewAccessKeyList({ accountId: deployment.agentId }),
    rpc.viewAccessKeyList({ accountId: deployment.contractId }),
  ]);
  renderPolicy(policy);
  renderRows($("payments"), payments, "No payments recorded.");
  renderRows($("rejections"), rejections, "No rejected attempts recorded.");
  renderKeys(agentKeys.keys || agentKeys, contractKeys.keys || contractKeys);
  $("refreshed").textContent = `Updated ${new Date().toLocaleTimeString()}`;
  showNotice("This page does not ask for a wallet or a key. It reads the live testnet contract and replays a recorded run of real transactions.");
}

function startPlayback() {
  const button = $("play");
  if (timer) {
    clearInterval(timer);
    timer = null;
    button.textContent = "Play";
    button.setAttribute("aria-pressed", "false");
    return;
  }
  button.textContent = "Pause";
  button.setAttribute("aria-pressed", "true");
  const advance = () => {
    playIndex = (playIndex + 1) % demo.attempts.length;
    renderAttempts();
    const current = document.querySelector(".attempt.active");
    current?.scrollIntoView({ block: "nearest", behavior: "smooth" });
  };
  advance();
  timer = setInterval(advance, 1600);
}

async function main() {
  try {
    [deployment, demo] = await Promise.all([
      loadJson("./deployment.json"),
      loadJson("./demo-run.json"),
    ]);
  } catch (error) {
    showNotice(`The published demo files are missing (${error.message}).`, true);
    return;
  }

  if (deployment.networkId !== "testnet") {
    showNotice("This page refuses to run unless deployment.json says testnet.", true);
    return;
  }

  $("links").hidden = false;
  $("contract-link").href = accountUrl(deployment.contractId);
  $("contract-link").textContent = deployment.contractId;
  $("agent-link").href = accountUrl(deployment.agentId);
  $("agent-link").textContent = "Agent key";
  $("run-summary").textContent = demo.summary;
  renderAttempts();
  $("play").addEventListener("click", startPlayback);
  $("refresh").addEventListener("click", () => {
    refresh().catch((error) => showNotice(`Could not read testnet: ${error.message}`, true));
  });

  try {
    await refresh();
  } catch (error) {
    showNotice(
      `Live testnet reads failed (${error.message}). The recorded run below is still available, with links to each transaction.`,
      true,
    );
  }

  if (new URLSearchParams(location.search).has("play")) startPlayback();
}

main();
