// Read-only authentication and approval checks for an already deployed Safe.
import {statSync} from 'node:fs';
import * as sdk from '@polymarket/client';
import {privateKey} from '@polymarket/client/viem';
import {encodeFunctionData} from 'viem';

async function main() {
  const path=process.argv[2] || '/opt/polycopier/.env.credentials';
  if((statSync(path).mode & 0o077)!==0) throw new Error('credential_permissions');
  process.loadEnvFile(path);
  const wallet=process.env.FUNDER_ADDRESS;
  if(!/^0x[a-fA-F0-9]{40}$/.test(wallet || '')) throw new Error('wallet_missing');
  const environment={...sdk.production,rpc:process.env.POLYGON_RPC_URL || 'https://polygon-bor-rpc.publicnode.com'};
  async function rpc(method,params) {
    const response=await fetch(environment.rpc,{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({jsonrpc:'2.0',id:1,method,params}),signal:AbortSignal.timeout(15000)});
    if(!response.ok) throw new Error('rpc_read_failed');
    const value=await response.json();
    if(value.id!==1 || value.error || value.result===undefined) throw new Error('invalid_rpc_result');
    return value.result;
  }
  if(await rpc('eth_chainId',[])!=='0x89') throw new Error('wrong_chain');
  const code=await rpc('eth_getCode',[wallet,'latest']);
  if(typeof code!=='string' || code==='0x' || code==='0x0') throw new Error('wallet_not_deployed');
  const rawKey=process.env.PRIVATE_KEY || '';
  const signer=privateKey(rawKey.startsWith('0x')?rawKey:`0x${rawKey}`);
  const client=await sdk.createSecureClient({environment,signer,wallet,apiKey:sdk.relayerApiKey({key:process.env.POLYMARKET_RELAYER_API_KEY,address:process.env.POLYMARKET_RELAYER_API_KEY_ADDRESS})});
  if(client.account.wallet.toLowerCase()!==wallet.toLowerCase() || client.account.walletType!==sdk.WalletType.GNOSIS_SAFE) throw new Error('safe_wallet_mismatch');
  const abi=[{type:'function',name:'isApprovedForAll',stateMutability:'view',inputs:[{name:'owner',type:'address'},{name:'operator',type:'address'}],outputs:[{type:'bool'}]}];
  const approvals={};
  for(const [name,operator] of [['standard',environment.contracts.collateralAdapter],['negative_risk',environment.contracts.negRiskCollateralAdapter]]) {
    const data=encodeFunctionData({abi,functionName:'isApprovedForAll',args:[wallet,operator]});
    const result=await rpc('eth_call',[{to:environment.contracts.conditionalTokens,data},'latest']);
    if(!/^0x[0-9a-fA-F]{64}$/.test(result)) throw new Error('invalid_approval_result');
    approvals[name]=BigInt(result)===1n;
  }
  process.stdout.write(JSON.stringify({secure_client_auth:'passed',configured_safe:'verified',redemption_approvals:approvals,on_chain_transactions:0})+'\n');
}
main().catch(()=>{process.stderr.write('SecureClient read-only preflight failed; no transactions submitted.\n');process.exitCode=1;});
