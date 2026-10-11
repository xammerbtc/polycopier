import { useEffect, useState, useRef } from 'react';
import './Operations.css';

export default function Operations({mode = 'simulation'}: {mode?: string}) {
  const [entries, setEntries] = useState<any[]>([]);
  const [lines, setLines] = useState<string[]>([]);
  const [wallet, setWallet] = useState('');
  const [date, setDate] = useState('');
  const [redeeming, setRedeeming] = useState(false);
  const [pendingRedemption,setPendingRedemption]=useState(false);
  const wasPending=useRef(false);
  const [redeemResult, setRedeemResult] = useState('');
  async function redeem() {
    setRedeeming(true);
    try {
      const response = await fetch('/api/redeem', {method:'POST'});
      if (!response.ok) throw new Error(await response.text());
      const result = await response.json();
      setRedeemResult(result.mode === 'simulation' ? `Paper redemption: ${result.settled.length} positions settled; ${result.lookup_failures.length} lookup failures. No on-chain transaction.` : result.settlement_pending ? 'Redemption submitted and awaiting confirmation. The bot is paused.' : `Live redemption: ${result.on_chain_transactions} transactions confirmed. Payout and positions have been reconciled; the bot is paused.`);
    } catch (error) { setRedeemResult(String(error)); }
    finally { setRedeeming(false); }
  }
  useEffect(() => {
    const refresh = () => {
      fetch('/api/ledger').then(r => r.json()).then(v => setEntries(v.entries || [])).catch(() => {});
      fetch('/api/logs').then(r => r.json()).then(v => setLines(v.lines || [])).catch(() => {});
      fetch('/api/redemption').then(r=>r.json()).then(v=>{
        const pending=Object.values(v.records || {}).some((record:any)=>record.status!=='confirmed' || !record.proof);
        setPendingRedemption(pending);
        if(wasPending.current && !pending && mode==='live') setRedeemResult('Redemption confirmed. Payout and positions have been reconciled; the bot is paused.');
        wasPending.current=pending;
      }).catch(()=>{});
    };
    refresh(); const timer = setInterval(refresh, 3000); return () => clearInterval(timer);
  }, [mode]);
  const filtered = entries.filter(e => (!wallet || e.source_wallet.includes(wallet.toLowerCase())) && (!date || e.copied_at.startsWith(date)));
  return <div className="glass-panel">
    <div className="panel-header">Trade history & operations</div>
    <div className="operations-controls" style={{display:'flex', gap:'1rem', flexWrap:'wrap', marginBottom:'1rem'}}>
      <input placeholder="Filter source wallet" value={wallet} onChange={e => setWallet(e.target.value)} />
      <input type="date" value={date} onChange={e => setDate(e.target.value)} />
      <a href="/api/config" download="config.json">Export config</a><a href="/api/ledger" download="ledger.json">Export ledger</a>
      <a href="/api/redemption" download="redemption.json">Redemption journal</a>
      <button disabled={redeeming || (mode==='live' && pendingRedemption)} onClick={redeem} title="Redeem resolved copier positions; live execution requires configured Relayer credentials">{redeeming ? 'Checking resolution…' : `Force Redeem (${mode === 'simulation' ? 'paper' : 'live'})`}</button>
    </div>
    {redeemResult && <p role="status">{redeemResult}</p>}
    {mode==='live' && pendingRedemption && <p role="status">Redemption is pending confirmation or needs attention. The bot is paused; do not retry.</p>}
    <div className="table-container"><table><thead><tr><th>Time (UTC)</th><th>Source wallet</th><th>Token</th><th>Entry</th><th>Size</th><th>Status</th></tr></thead><tbody>
      {filtered.slice().reverse().map((e,i) => <tr key={i}><td>{e.copied_at}</td><td>{e.source_wallet}</td><td>{e.token_id.slice(0,16)}</td><td>{e.entry_price}</td><td>{e.size}</td><td>{e.closed ? 'Closed' : 'Open'}</td></tr>)}
      {!filtered.length && <tr><td colSpan={6}>No copied trades recorded for this filter.</td></tr>}
    </tbody></table></div>
    <div className="panel-header">Recent logs (last 200 lines)</div>
    <pre style={{maxHeight:'260px', overflow:'auto', whiteSpace:'pre-wrap', fontSize:'12px'}}>{lines.join('\n') || 'No warnings recorded.'}</pre>
  </div>;
}
