The trimmed receipt is public Polygon transaction
`0x099aa76ffbab88788d4f07c6a370579c95e7c7c2fabb24e062981c4599b6092c`,
retrieved on 2026-10-11 from a public RPC using activity of the configured target.
Only the target wallet's CTF Exchange V2 event is retained. It contains no
credentials and was not submitted by this copier.

The decoder follows the official contract's event layout and collateral fee
settlement, not an estimated fee rate:
- https://github.com/Polymarket/ctf-exchange-v2/blob/main/src/exchange/mixins/Events.sol
- https://github.com/Polymarket/ctf-exchange-v2/blob/main/src/exchange/mixins/Trading.sol

`receipt_preflight` independently checks that the transaction remains in the
canonical finalized chain. The frozen fixture itself does not prove current
RPC availability, copier order submission, or end-to-end copy latency.
