# Demo video script (60–90 seconds)

Record the GitHub Pages demo in a browser, full screen, with the network tab closed. No wallet popup should appear. Speak the lines below, or use them as captions.

Target length: about 75 seconds.

## 0:00–0:08 — The problem

Open https://asosa933.github.io/agent-allowance-guard/

Say: "An AI agent that can spend without a limit can be talked into draining the wallet. This vault lets the agent pay, and stops everything else."

Point at the "Testnet only" stamp and the contract account link.

## 0:08–0:20 — The policy

Scroll to the four figures.

Say: "The owner set a 0.5 NEAR cap on each payment and a 1 NEAR cap across any 24 hours. The agent key on chain can call only `pay`."

Point at the agent access key panel. The method list should say `pay`.

## 0:20–0:55 — The recorded run

Click **Play**. Let the steps advance. Pause on these four:

1. The 0.3 NEAR payment marked **allowed**. Click its transaction link and show the testnet explorer for one second.
2. The 0.6 NEAR payment marked **blocked** — over the per-payment limit.
3. The payment to `not-allowlisted.testnet` marked **blocked**.
4. The withdraw attempt marked **rejected**. Say: "That failure is the protocol, not the contract. The key is not allowed to call withdraw."

## 0:55–1:15 — Live state

Scroll to "Rejected attempts" and "Rolling 24 hours".

Say: "Those blocks are stored on the contract. About 0.8 NEAR of the daily 1 NEAR is used. The owner can still pause the vault or withdraw. The agent cannot."

End on the contract account link. Do not open a wallet, a terminal, or mainnet.
