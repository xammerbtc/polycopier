import {test} from 'node:test';
import assert from 'node:assert/strict';
import {redeem,validateRequest} from './runner.mjs';
const env = {POLYCOPIER_MODE:'live',LIVE_REDEEM_ENABLED:'true',PRIVATE_KEY:'1'.repeat(64),FUNDER_ADDRESS:'0x'+'2'.repeat(40),POLYMARKET_RELAYER_API_KEY:'test-only',POLYMARKET_RELAYER_API_KEY_ADDRESS:'0x'+'3'.repeat(40)};
test('preview and absent credentials cannot invoke the SDK', async () => {
  let called=false;
  const deps={createSecureClient:()=>{called=true;}};
  for (const bad of [{...env,POLYCOPIER_MODE:'simulation'},{...env,LIVE_REDEEM_ENABLED:'false'},{...env,POLYMARKET_RELAYER_API_KEY:''}]) await assert.rejects(redeem({market_id:'42'},bad,deps));
  assert.equal(called,false);
  for (const market_id of ['../secret','0x123','42;exec','']) assert.throws(()=>validateRequest({market_id},env));
});
test('passes exact wallet to SDK and waits for a confirmed transaction', async () => {
  let waited=false,submitted;
  const deps={privateKey:k=>({key:k}),relayerApiKey:c=>c,createSecureClient:async config=>{
    assert.equal(config.wallet,env.FUNDER_ADDRESS);
    return {redeemPositions:async request=>{submitted=request;return {wait:async()=>{waited=true;return {transactionHash:'0x'+'4'.repeat(64)};}};}};
  }};
  const receipt=await redeem({market_id:'42'},env,deps);
  assert.deepEqual(submitted,{marketId:'42'});assert.equal(waited,true);assert.equal(receipt.status,'confirmed');
  assert.equal(JSON.stringify(receipt).includes(env.PRIVATE_KEY),false);
});
test('missing confirmation is a failure, never a synthetic success', async () => {
  const deps={privateKey:()=>({}),relayerApiKey:()=>({}),createSecureClient:async()=>({redeemPositions:async()=>({wait:async()=>({transactionHash:null})})})};
  await assert.rejects(redeem({market_id:'42'},env,deps),/missing_confirmed_receipt/);
});
