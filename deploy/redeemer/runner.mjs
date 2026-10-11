import {pathToFileURL} from 'node:url';

export function validateRequest(request, env) {
  if (env.POLYCOPIER_MODE !== 'live' || env.LIVE_REDEEM_ENABLED !== 'true') throw new Error('live_redemption_disabled');
  if (typeof request?.market_id !== 'string' || !/^\d{1,30}$/.test(request.market_id)) throw new Error('invalid_market_id');
  if (!/^(0x)?[a-fA-F0-9]{64}$/.test(env.PRIVATE_KEY || '')) throw new Error('missing_signer');
  if (!/^0x[a-fA-F0-9]{40}$/.test(env.FUNDER_ADDRESS || '')) throw new Error('missing_wallet');
  if (!env.POLYMARKET_RELAYER_API_KEY || !/^0x[a-fA-F0-9]{40}$/.test(env.POLYMARKET_RELAYER_API_KEY_ADDRESS || '')) throw new Error('missing_relayer_credentials');
}

export async function redeem(request, env, dependencies) {
  validateRequest(request, env);
  const signerKey = env.PRIVATE_KEY.startsWith('0x') ? env.PRIVATE_KEY : `0x${env.PRIVATE_KEY}`;
  const client = await dependencies.createSecureClient({
    ...(dependencies.production ? {environment:{...dependencies.production,rpc:env.POLYGON_RPC_URL || 'https://polygon-bor-rpc.publicnode.com'}} : {}),
    signer: dependencies.privateKey(signerKey), wallet: env.FUNDER_ADDRESS,
    apiKey: dependencies.relayerApiKey({key:env.POLYMARKET_RELAYER_API_KEY,address:env.POLYMARKET_RELAYER_API_KEY_ADDRESS}),
  });
  // The official SDK identifies CTF/V2 and Safe/Proxy/Deposit wallet routes.
  // No approvals, deployments, transfers, or order calls are made here.
  const transaction = await client.redeemPositions({marketId:request.market_id});
  const outcome = await transaction.wait();
  if (!/^0x[a-fA-F0-9]{64}$/.test(outcome?.transactionHash || '')) throw new Error('missing_confirmed_receipt');
  return {status:'confirmed',market_id:request.market_id,transaction_hash:outcome.transactionHash};
}

async function main() {
  if (Number(process.versions.node.split('.')[0]) < 24) throw new Error('node_24_required');
  let input = '';
  for await (const chunk of process.stdin) { input += chunk; if (input.length > 2048) throw new Error('request_too_large'); }
  const request = JSON.parse(input);
  validateRequest(request, process.env);
  const sdk = await import('@polymarket/client');
  const viem = await import('@polymarket/client/viem');
  const result = await redeem(request, process.env, {...sdk,...viem});
  process.stdout.write(JSON.stringify(result)+'\n');
}
if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  main().catch(() => { process.stderr.write('Redemption failed; reconcile the persisted intent before retrying.\n'); process.exitCode=1; });
}
