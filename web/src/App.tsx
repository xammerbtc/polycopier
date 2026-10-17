import { useEffect, useState } from 'react';
import SettingsManager from './SettingsManager';
import SetupWizard from './SetupWizard';
import Operations from './Operations';
import type { FeedTrade, Order, Position, PositionSourceAllocation, StateResponse, TargetPosition, TargetPerformance } from './types';

const skipLabels: Record<string, string> = {
  minimum_order: 'Order minimum',
  no_liquidity: 'No liquidity',
  book_unavailable: 'Book unavailable',
  unowned_sell: 'No owned shares',
  stale_signal: 'Stale signal',
  fee_lookup: 'Fee lookup',
  insufficient_cash: 'Insufficient cash',
  price_limit: 'Price limit',
  other: 'Other',
};

function App() {
  const [state, setState] = useState<StateResponse | null>(null);
  const [activeTab, setActiveTab] = useState<'dashboard' | 'settings'>('dashboard');
  const [setupRequired, setSetupRequired] = useState(false);
  const [targetLimit, setTargetLimit] = useState(30);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [retryKey, setRetryKey] = useState(0);
  const [additionalTargets, setAdditionalTargets] = useState<TargetPosition[]>([]);
  const [loadingMoreTargets, setLoadingMoreTargets] = useState(false);
  const [lastStateAt, setLastStateAt] = useState<number | null>(null);
  const [saleType, setSaleType] = useState<'market' | 'limit'>('market');
  const [saleLimit, setSaleLimit] = useState('');
  const [saleMessage, setSaleMessage] = useState('');
  const [saleBusy, setSaleBusy] = useState(false);

  useEffect(() => {
    let inFlight = false;
    let disposed = false;
    const controller = new AbortController();
    let metricsInFlight = false;
    const fetchState = async () => {
      if (inFlight || disposed) return;
      inFlight = true;
      const requestController = new AbortController();
      const abortOnDispose = () => requestController.abort();
      controller.signal.addEventListener('abort', abortOnDispose, {once:true});
      const timeout = window.setTimeout(() => requestController.abort(), 10000);
      try {
        const res = await fetch('/api/state', {signal:requestController.signal});
        if (!res.ok) throw new Error(`State request failed (${res.status})`);
        const data: StateResponse & { status?: string } = await res.json();
        if (disposed) return;
        if (data.status === 'setup_required') {
          setSetupRequired(true);
          setLoadError(null);
        } else {
          setSetupRequired(false);
          setLoadError(null);
          setState((previous) => ({...data, metrics:previous?.metrics ?? null}));
          setLastStateAt(Date.now());
        }
      } catch (err) {
        if (!disposed) {
          const message = err instanceof Error && err.name === 'AbortError'
            ? 'Dashboard state request timed out after 10 seconds.'
            : err instanceof Error ? err.message : 'Dashboard state request failed.';
          setLoadError(message);
          console.error('Failed to fetch state', err);
        }
      } finally {
        window.clearTimeout(timeout);
        controller.signal.removeEventListener('abort', abortOnDispose);
        inFlight = false;
      }
    };

    const fetchMetrics = async () => {
      if (metricsInFlight || disposed) return;
      metricsInFlight = true;
      try {
        const response = await fetch('/api/metrics', {signal:controller.signal});
        if (!response.ok) return;
        const metrics = await response.json() as NonNullable<StateResponse['metrics']>;
        if (!disposed) setState((previous) => previous ? {...previous, metrics} : previous);
      } catch { /* Metrics are secondary and must never block the dashboard. */ }
      finally { metricsInFlight = false; }
    };
    
    fetchState();
    fetchMetrics();
    const interval = activeTab === 'dashboard' ? setInterval(() => { fetchState(); fetchMetrics(); }, 2000) : undefined;
    return () => { disposed = true; controller.abort(); clearInterval(interval); };
  }, [activeTab, retryKey]);

  if (setupRequired) {
    return <SetupWizard />;
  }

  if (!state) {
    return (
      <div className="loading-container">
        <div className="spinner"></div>
        <div style={{ color: 'var(--text-secondary)' }}>{loadError || 'Connecting to Polycopier Daemon...'}</div>
        {loadError && <button className="btn btn-secondary" onClick={() => { setLoadError(null); setRetryKey((key) => key + 1); }}>Retry connection</button>}
      </div>
    );
  }

  const pendingOrders: Order[] = state.mode === 'simulation' ? Object.values(state.pending_orders || {}) : (state.active_orders || []);

  // The API already sends scanner-priority order. Sorting a growing 60k list
  // in the browser on every 2s state refresh made the dashboard sluggish.
  const targets = [...(state.target_positions || []), ...additionalTargets];
  const totalTargets = Number(state.target_positions_total ?? targets.length);

  const loadMoreTargets = async () => {
    if (targetLimit < targets.length) {
      setTargetLimit((limit) => Math.min(limit + 50, totalTargets));
      return;
    }
    if (loadingMoreTargets || targets.length >= totalTargets) return;
    setLoadingMoreTargets(true);
    try {
      const offset = (state.target_positions || []).length + additionalTargets.length;
      const response = await fetch(`/api/targets?offset=${offset}&limit=50`);
      if (!response.ok) throw new Error(`Targets request failed (${response.status})`);
      const page = await response.json();
      setAdditionalTargets((previous) => [...previous, ...(page.items || [])]);
      setTargetLimit((limit) => Math.min(limit + 50, totalTargets));
    } catch (error) {
      setLoadError(error instanceof Error ? error.message : 'Could not load more target positions.');
    } finally {
      setLoadingMoreTargets(false);
    }
  };

  const positions = Object.values(state.positions || {}) as Position[];
  const openPositions = positions.filter((p) => state.position_details?.[p.token_id]?.closed !== true);
  const closedMarketPositions = positions.filter((p) => state.position_details?.[p.token_id]?.closed === true);
  const submitPaperSale = async (scope: 'position' | 'all' | 'signals', tokenId?: string, sourceWallet?: string) => {
    if (state.mode !== 'simulation' || saleBusy) return;
    if (saleType === 'limit' && (!saleLimit || Number(saleLimit) <= 0 || Number(saleLimit) > 0.99)) {
      setSaleMessage('Enter a limit price between $0.01 and $0.99.'); return;
    }
    const label = scope === 'position' ? 'this leader’s position' : scope === 'signals' ? 'all copied positions' : 'all open positions';
    if (!window.confirm(`Submit a paper ${saleType} sell for ${label}? This will immediately fill against current bids; any unfilled quantity is cancelled.`)) return;
    setSaleBusy(true); setSaleMessage('Submitting paper sell…');
    try {
      const response = await fetch('/api/paper/sell', {method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({scope,token_id:tokenId,source_wallet:sourceWallet,order_type:saleType,limit_price:saleType==='limit'?Number(saleLimit):null})});
      const raw = await response.text();
      let result: Record<string, unknown>;
      try { result = JSON.parse(raw) as Record<string, unknown>; } catch { result = {message:raw}; }
      if (!response.ok) throw new Error(String(result.message || result.error || 'Paper sale failed'));
      const saleResults = Array.isArray(result.results) ? result.results as {success?:boolean}[] : [];
      const succeeded = saleResults.filter((item) => item.success);
      const failed = saleResults.filter((item) => !item.success);
      setSaleMessage(`Filled ${succeeded.length} position(s)${failed.length ? `; ${failed.length} had no fill or need review` : ''}. Cash balance and source PnL updated.`);
      setRetryKey((key) => key + 1);
    } catch (error) { setSaleMessage(error instanceof Error ? error.message : 'Paper sale failed'); }
    finally { setSaleBusy(false); }
  };
  const currentBalance = Number(state.total_balance || 0);
  const realizedPnl = Number(state.realized_pnl || 0);
  const unrealizedPnl = Number(state.unrealized_pnl || 0);
  const startingBalance = Number(state.starting_balance ?? 10000);
  const unpricedPositions = positions.filter((position) => state.position_prices?.[position.token_id] === undefined);
  const pricedMarketValue = positions.reduce((sum, position) => {
    const price = state.position_prices?.[position.token_id];
    return price === undefined ? sum : sum + Number(position.size) * Number(price);
  }, 0);
  const unpricedShares = unpricedPositions.reduce((sum, position) => sum + Number(position.size), 0);
  const staleOpenQuotes = positions.filter((position) =>
    state.position_details?.[position.token_id]?.closed !== true &&
    state.position_prices?.[position.token_id] !== undefined &&
    Date.now() / 1000 - Number(state.price_updated_at?.[position.token_id] || 0) > 60
  ).length;
  const equityFloor = currentBalance + pricedMarketValue;
  const equityCeiling = equityFloor + unpricedShares;
  const pnlFloor = equityFloor - startingBalance;
  const pnlCeiling = equityCeiling - startingBalance;
  const targetPerformance: TargetPerformance[] = state.target_performance || [];
  const targetNames = new Map<string, string>(targetPerformance.map((target): [string, string] => [target.address.toLowerCase(), target.name]));
  const skipReasonCounts = (state.live_feed || []).reduce((counts: Record<string, number>, trade: FeedTrade) => {
    if (!trade.validated && trade.reason) {
      const reason = String(trade.reason).replace(/\s+/g, ' ').trim();
      const category = /no asks available at or below/.test(reason)
        ? 'No executable ask within budget / 5-share minimum'
        : /source notional.*order minimum/.test(reason)
          ? 'Source stake below $1 order minimum'
          : /SELL book unavailable.*404/.test(reason)
            ? state.position_details?.[trade.original_event.token_id]?.closed === true
              ? 'SELL on confirmed closed market; settlement path'
              : 'SELL book 404; market status unconfirmed here'
            : /closing long we never entered/.test(reason)
              ? 'Leader sold shares we never bought'
              : reason.split(':')[0].slice(0, 90);
      counts[category] = (counts[category] || 0) + 1;
    }
    return counts;
  }, {} as Record<string, number>);
  const recentSkipReasons = (Object.entries(skipReasonCounts) as [string, number][])
    .sort((a, b) => b[1] - a[1])
    .slice(0, 5);
  const recentSkippedSignals = (state.live_feed || []).filter((trade) => !trade.validated && trade.reason);
  const expectedSaleSkips = recentSkippedSignals.filter((trade) =>
    /SELL skipped: target (closing long we never entered|opening short)/.test(String(trade.reason))).length;
  const otherRecentSkips = recentSkippedSignals.length - expectedSaleSkips;

  const getStatusLabel = (status: TargetPosition['status']) => {
      if (typeof status === 'string') { return status; }
      if (status && typeof status === 'object') { return Object.keys(status)[0]; }
      return 'UNKNOWN';
  };

  const CopyWalletButton = ({ wallet }: { wallet: string }) => {
    const [copied, setCopied] = useState(false);
    return (
      <button 
        onClick={() => {
          navigator.clipboard.writeText(wallet);
          setCopied(true);
          setTimeout(() => setCopied(false), 2000);
        }}
        title="Copy full address"
        style={{ 
          background: 'none', 
          border: 'none', 
          cursor: 'pointer', 
          padding: '2px', 
          display: 'flex', 
          alignItems: 'center', 
          color: copied ? 'var(--accent-positive)' : 'var(--text-secondary)', 
          opacity: copied ? 1 : 0.7,
          transition: 'all 0.2s ease'
        }}
        onMouseEnter={(e) => !copied && (e.currentTarget.style.opacity = '1')}
        onMouseLeave={(e) => !copied && (e.currentTarget.style.opacity = '0.7')}
      >
        {copied ? (
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
            <polyline points="20 6 9 17 4 12"></polyline>
          </svg>
        ) : (
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
            <rect x="9" y="9" width="13" height="13" rx="2" ry="2"></rect>
            <path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1"></path>
          </svg>
        )}
      </button>
    );
  };

  return (
    <div className="dashboard-container">
      {loadError && <div className="glass-panel" role="status" style={{marginBottom:'1rem',color:'var(--val-negative)'}}>
        Dashboard update delayed: {loadError} <button className="btn btn-secondary" onClick={() => setRetryKey((key) => key + 1)}>Reconnect</button>
      </div>}
      <header>
        <div className="header-title" style={{ display: 'flex', justifyContent: 'space-between', width: '100%' }}>
          <div style={{ display: 'flex', alignItems: 'center', gap: '1rem' }}>
            <h1>Polycopier</h1>
            <div className="live-badge">{state.mode === 'simulation' ? 'DRY-RUN / SIMULATION' : state.mode.toUpperCase()}</div>
            <div className="mode-switch" aria-label="Execution mode">
              <button className={state.mode === 'simulation' ? 'active' : ''} disabled={state.mode === 'simulation'}>PAPER</button>
              <button className={state.mode === 'live' ? 'active live' : ''} disabled={!state.live_ready || state.mode === 'live'} title={state.live_blockers.join(' · ')}>LIVE{!state.live_ready ? ' · locked' : ''}</button>
            </div>
            <button className="btn btn-secondary" disabled={state.paused && state.paper_integrity_fault} onClick={async () => { const response = await fetch('/api/control', {method:'POST', headers:{'Content-Type':'application/json'}, body:JSON.stringify({action:state.paused ? 'resume' : 'pause'})}); if (response.ok) setRetryKey((key) => key + 1); else setLoadError(await response.text()); }}>{state.paused ? 'Resume' : 'Pause'}</button>
            <span>{state.paused ? 'Paused' : 'Running'}</span>
            <span className="data-freshness" title="A fresh API response confirms the dashboard connection; prices may only change when the market moves">API {lastStateAt ? `${Math.max(0, Math.floor((Date.now()-lastStateAt)/1000))}s` : 'waiting'} · prices {state.prices_updated_at ? `${Math.max(0, Math.floor(Date.now()/1000-state.prices_updated_at))}s` : 'waiting'}</span>
          </div>
          <div className="nav-tabs">
            <button className={`nav-tab ${activeTab === 'dashboard' ? 'active' : ''}`} onClick={() => setActiveTab('dashboard')}>Dashboard</button>
            <button className={`nav-tab ${activeTab === 'settings' ? 'active' : ''}`} onClick={() => setActiveTab('settings')}>Targets & Settings</button>
          </div>
        </div>

        {state.paper_integrity_fault && <div className="glass-panel" role="alert" style={{marginTop:'1rem',padding:'0.9rem 1rem',borderColor:'var(--val-negative)',color:'var(--val-negative)'}}>Paper account persistence failed. Trading is paused until the saved account and source ledger are reconciled and the service is restarted.</div>}

        {state.mode === 'simulation' && <div className="glass-panel" role="status" style={{marginTop:'1rem',padding:'0.9rem 1rem',borderColor:'rgba(245,191,66,.35)'}}>
          <strong style={{color:'#f5bf42'}}>Live is not armed</strong>
          <span style={{color:'var(--text-secondary)',marginLeft:'.5rem'}}>This dashboard is connected to the paper process. Switching modes requires a separately configured live service and a fresh startup reconciliation; this button cannot turn paper orders into real orders.</span>
          {state.live_blockers.length > 0 && <details style={{marginTop:'.55rem',color:'var(--text-secondary)'}}>
            <summary style={{cursor:'pointer'}}>Live preflight ({state.live_blockers.length} blockers)</summary>
            <ul style={{margin:'.45rem 0 0',paddingLeft:'1.25rem'}}>{state.live_blockers.map((blocker) => <li key={blocker}>{blocker}</li>)}</ul>
          </details>}
        </div>}
        
        <div className="stats-grid">
          <div className="stat-card">
            <span className="stat-label">{state.mode === 'simulation' ? 'Paper Balance' : 'Total Balance'}</span>
            <span className="stat-value">${parseFloat(state.total_balance).toFixed(2)}</span>
          </div>
          <div className="stat-card">
            <span className="stat-label">Open PnL (unrealized)</span>
            <span className={`stat-value ${unpricedPositions.length > 0 ? 'val-neutral' : unrealizedPnl < 0 ? 'val-negative' : unrealizedPnl > 0 ? 'val-positive' : 'val-neutral'}`}>
              {unpricedPositions.length === 0 ? `$${unrealizedPnl.toFixed(2)}` : `Partial $${unrealizedPnl.toFixed(2)}`}
            </span>
            {unpricedPositions.length > 0 && <span className="stat-note">{unpricedPositions.length} position(s) have no quote</span>}
          </div>
          <div className="stat-card">
            <span className="stat-label">Signals validated</span>
            <span className="stat-value">{state.copies_executed}</span>
          </div>
          <div className="stat-card" title="Paper: filled buy lots in the copy ledger. Live: distinct buy orders with confirmed fills. Counted since performance tracking began.">
            <span className="stat-label">Filled buys (since reset)</span>
            <span className="stat-value">{state.filled_buy_orders == null ? 'Unavailable' : state.filled_buy_orders.toLocaleString()}</span>
          </div>
          <div className="stat-card" title="All skipped signals since the experiment began, including sells of positions we never held and unsupported short openings. See recent reasons below.">
            <span className="stat-label">Signals skipped (all reasons)</span>
            <span className="stat-value">{state.trades_skipped}</span>
            <span className="stat-note" title="Attempt categories are retained from this update onward; a later successful retry does not erase its earlier diagnostic attempt.">
              {Object.entries(state.skip_attempts || {}).sort((a, b) => b[1] - a[1]).slice(0, 2)
                .map(([key, count]) => `${skipLabels[key] || key}: ${count}`).join(' · ') || 'Reason history starts with this update'}
            </span>
          </div>
          <div className="stat-card">
            <span className="stat-label">Positions refresh</span>
            <span className="stat-value">{state.next_scan_secs}s</span>
          </div>
          <div className="stat-card">
            <span className="stat-label">Realized PnL (sales + confirmed settlement)</span>
            <span className={`stat-value ${realizedPnl < 0 ? 'val-negative' : realizedPnl > 0 ? 'val-positive' : 'val-neutral'}`}>${realizedPnl.toFixed(2)}</span>
            <span style={{color:'var(--text-secondary)',fontSize:'12px',lineHeight:1.4}} title="Realized PnL changes only after a copied sale or a confirmed redemption.">
              {Math.abs(realizedPnl) < 0.005
                ? `${closedMarketPositions.length} closed market(s) await redemption; ${openPositions.length} position(s) remain open.`
                : 'Updated by copied sales and confirmed redemptions.'}
            </span>
          </div>
          {state.mode === 'simulation' && <div className="stat-card">
            <span className="stat-label">Portfolio equity (cash + open market value)</span>
            <span className="stat-value">{state.equity == null ? `≥ $${equityFloor.toFixed(2)}` : `$${Number(state.equity).toFixed(2)}`}</span>
            <span style={{color:'var(--text-secondary)',fontSize:'12px'}}>{unpricedPositions.length > 0 ? `Upper bound $${equityCeiling.toFixed(2)} · ` : ''}Quotes: {state.priced_positions}/{positions.length}{staleOpenQuotes > 0 ? ` · ${staleOpenQuotes} open quote(s) older than 60s; indicative value` : ''}</span>
          </div>}
          {state.mode === 'simulation' && <div className="stat-card">
            <span className="stat-label">Total PnL vs starting bank</span>
            <span className={`stat-value ${state.total_pnl == null ? '' : Number(state.total_pnl) < 0 ? 'val-negative' : Number(state.total_pnl) > 0 ? 'val-positive' : ''}`}>{state.total_pnl == null ? `≥ ${pnlFloor < 0 ? '-' : '+'}$${Math.abs(pnlFloor).toFixed(2)}` : `${Number(state.total_pnl) >= 0 ? '+' : ''}$${Number(state.total_pnl).toFixed(2)}`}</span>
            <span style={{color:'var(--text-secondary)',fontSize:'12px'}}>{state.total_pnl == null ? `Upper bound $${pnlCeiling.toFixed(2)} · ` : ''}Open cost ${Number(state.open_cost_basis).toFixed(2)}{staleOpenQuotes > 0 ? ' · Indicative: stale open quotes' : ''}</span>
          </div>}
          <div className="stat-card"><span className="stat-label">Daily Volume</span><span className="stat-value">${parseFloat(state.daily_volume_usd).toFixed(2)}</span></div>
        </div>
        {state.mode === 'simulation' && <div className="glass-panel" role="status" style={{marginTop:'1rem',padding:'0.7rem 1rem',color:'var(--text-secondary)'}}>
          <strong>Paper settlement</strong> · {closedMarketPositions.length} closed-market positions awaiting confirmed payout
          {' · '}{state.paper_settlement_last_attempt == null ? 'lookup not yet attempted' : `last checked ${new Date(state.paper_settlement_last_attempt * 1000).toLocaleTimeString()}`}
          {state.paper_settlement_last_error && <span style={{color:'#f5bf42'}}> · {state.paper_settlement_last_error}</span>}
          {state.paper_settlement_market_failures > 0 && <span style={{color:'#f5bf42'}}> · {state.paper_settlement_market_failures} market detail lookups failed</span>}
          {closedMarketPositions.length > 0 && !state.paper_settlement_last_error && state.paper_settlement_market_failures === 0 && <span> · no confirmed payout returned yet</span>}
        </div>}
      </header>

      {activeTab === 'dashboard' ? (
        <div style={{ display: 'flex', flexDirection: 'column', gap: '1.5rem' }}>
          <div className="glass-panel">
            <div className="panel-header">Wallet copy performance <span className="panel-subtitle">since {state.performance_since ? new Date(state.performance_since).toLocaleString() : 'tracking began'} · automatic score</span></div>
            <div className="table-container"><table>
              <thead><tr><th>Source wallet</th><th>System rating</th><th>Copy ratio</th><th>Signals / skips</th><th>Copies / closed</th><th>Open positions</th><th>Open cost</th><th>Open value</th><th>Open PnL</th><th>Realized PnL</th><th>Total PnL</th><th>ROI</th></tr></thead>
              <tbody>{targetPerformance.map((target) => <tr key={target.address}>
                <td title={target.address}><strong>{target.name}</strong><div style={{fontFamily:'monospace',fontSize:'11px',color:'var(--text-secondary)'}}>{target.address}</div></td>
                <td title={target.roi_pct == null ? 'No complete tracked copy data yet' : `Return on tracked copied cost: ${Number(target.roi_pct).toFixed(2)}%`} style={{color:'#f5bf42',whiteSpace:'nowrap'}}>{target.roi_pct == null ? 'No data' : <>{'★'.repeat(Number(target.rating) || 0)}<span style={{color:'var(--text-secondary)'}}>{'☆'.repeat(5 - (Number(target.rating) || 0))}</span></>}</td>
                <td>{Number(target.copy_pct).toLocaleString(undefined, {maximumFractionDigits: 3})}%</td>
                <td>{target.recent_signals} / {target.recent_skips}</td><td>{target.copied_entries} / {target.closed_entries}</td><td>{target.open_positions}</td>
                <td>${Number(target.open_cost).toFixed(2)}</td><td>${Number(target.open_value).toFixed(2)}</td>
                <td className={Number(target.open_pnl) > 0 ? 'val-positive' : Number(target.open_pnl) < 0 ? 'val-negative' : ''}>{Number(target.open_pnl) >= 0 ? '+' : ''}${Number(target.open_pnl).toFixed(2)}</td>
                <td className={Number(target.realized_pnl) > 0 ? 'val-positive' : Number(target.realized_pnl) < 0 ? 'val-negative' : ''}>{Number(target.realized_pnl) >= 0 ? '+' : ''}${Number(target.realized_pnl).toFixed(2)}</td>
                <td className={Number(target.total_pnl) > 0 ? 'val-positive' : Number(target.total_pnl) < 0 ? 'val-negative' : ''}>{Number(target.total_pnl) >= 0 ? '+' : ''}${Number(target.total_pnl).toFixed(2)}</td>
                <td>{target.roi_pct == null ? '—' : `${Number(target.roi_pct).toFixed(1)}%`}</td>
              </tr>)}
              {!targetPerformance.length && <tr><td colSpan={12} style={{textAlign:'center',color:'var(--text-secondary)'}}>Add named wallets under Targets &amp; Settings to see per-source results.</td></tr>}
              </tbody>
            </table></div>
            <p style={{color:'var(--text-secondary)',marginBottom:0}}>Stars are calculated automatically from realized plus open PnL divided by tracked copied cost: 1★ below −20%, 2★ below 0%, 3★ below 10%, 4★ below 25%, otherwise 5★. Legacy closed trades before this accounting baseline are excluded because their sale proceeds were not recorded per wallet.</p>
          </div>
          <div className="glass-panel">
            <div className="panel-header">Recent skip reasons <span className="panel-subtitle">from the latest 100 feed events</span></div>
            <p style={{color:'var(--text-secondary)',margin:'0 0 0.8rem'}}>Recent skips: {expectedSaleSkips} inventory or short-sale signals we could not mirror; {otherRecentSkips} other reasons, including price, liquidity, and order minimums. These are counts from the recent feed, not the all-time total above.</p>
            {recentSkipReasons.length
              ? <div style={{display:'flex',flexWrap:'wrap',gap:'0.75rem'}}>{recentSkipReasons.map(([reason, count]) => <div key={reason} style={{padding:'0.65rem 0.85rem',border:'1px solid var(--border-color)',borderRadius:'8px',color:'var(--text-secondary)'}}><strong style={{color:'var(--text-primary)'}}>{count}</strong> × {reason}</div>)}</div>
              : <p style={{color:'var(--text-secondary)',margin:0}}>No skipped signals in the recent feed.</p>}
            <p style={{color:'var(--text-secondary)',margin:'0.8rem 0 0'}}>The Live Feed below keeps the full reason for each recent signal. A repeated BUY on an already-held token is now copied in simulation as a separate source lot.</p>
          </div>
          {/* Our Open Positions - FULL WIDTH */}
          <div className="glass-panel">
            <div className="panel-header">
              {state.mode === 'simulation' ? 'Open positions' : 'Our Positions'}
              <span className="panel-subtitle">Open: {openPositions.length} · Market closed, awaiting settlement: {closedMarketPositions.length}</span>
            </div>
            {state.mode === 'simulation' && <p style={{ color: 'var(--text-secondary)', margin: '0 0 1rem' }}>
              Starting paper bank ${startingBalance.toFixed(2)} · Balance now ${currentBalance.toFixed(2)} · Settled PnL ${realizedPnl.toFixed(2)} · Open mark-to-market PnL ${unrealizedPnl.toFixed(2)}. Closed markets remain in inventory until Force Redeem (paper) confirms the outcome; their proceeds then return to balance.
            </p>}
            <div className="table-container">
              <div className="position-sell-controls">
                <label>Exit order <select value={saleType} onChange={(event) => setSaleType(event.target.value as 'market' | 'limit')}><option value="market">Market · current bids</option><option value="limit">Limit floor · fill now or cancel</option></select></label>
                {saleType === 'limit' && <label>Minimum price <input type="number" min="0.01" max="0.99" step="0.01" value={saleLimit} onChange={(event) => setSaleLimit(event.target.value)} placeholder="0.65" /></label>}
                <button disabled={saleBusy || state.mode !== 'simulation' || openPositions.length === 0} onClick={() => void submitPaperSale('all')}>Sell all open</button>
                <button disabled={saleBusy || state.mode !== 'simulation' || openPositions.length === 0} onClick={() => void submitPaperSale('signals')}>Sell copied positions</button>
                {state.mode !== 'simulation' && <span>Manual dashboard exits are currently available in Paper mode only.</span>}
                {saleMessage && <span role="status">{saleMessage}</span>}
              </div>
              <table>
                <thead>
                  <tr>
                    <th>Market</th>
                    <th>Source</th>
                    <th>Size</th>
                    <th>Our Avg</th>
                    <th>Current Price</th>
                    <th>Target Avg</th>
                    <th>Diff %</th>
                    <th>Live PnL</th>
                    <th>Status</th>
                    <th>Exit</th>
                  </tr>
                </thead>
                <tbody>
                  {openPositions.map((p, i) => {
                    const t = targets.find((x) => x.token_id === p.token_id);
                    const metadata = state.position_details?.[p.token_id];
                    const title = metadata?.title || (t ? t.title : `${p.token_id.substring(0, 15)}...`);
                    const sourceWallet = state.position_sources ? (state.position_sources[p.token_id] || 'Unknown') : 'Unknown';
                    const shortWallet = sourceWallet === 'Unknown' ? 'Unknown' : `${sourceWallet.substring(0, 6)}...${sourceWallet.substring(sourceWallet.length - 4)}`;
                    const ourAvg = parseFloat(p.average_entry_price);
                    const targetAvg = t ? parseFloat(t.avg_price) : 0;
                    const hasPrice = state.position_prices?.[p.token_id] !== undefined;
                    const curPrice = hasPrice ? parseFloat(state.position_prices[p.token_id]) : Number.NaN;
                    const diffPct = targetAvg > 0 ? ((ourAvg - targetAvg) / targetAvg) * 100 : 0;
                    const pnlUsd = (curPrice - ourAvg) * parseFloat(p.size);
                    const pnlPct = ourAvg > 0 ? ((curPrice - ourAvg) / ourAvg) * 100 : 0;
                    
                    return (
                      <tr key={i}>
                        <td className="td-truncate" title={`${title}\nToken: ${p.token_id}`} style={{ maxWidth: '300px' }}>{title}<div style={{color:'var(--text-secondary)',fontSize:'12px'}}>{metadata?.outcome || t?.outcome || 'Outcome unavailable'}</div></td>
                        <td>
                          {state.position_source_allocations?.[p.token_id]?.length ? (
                            <div style={{display:'flex',flexDirection:'column',gap:'3px'}}>
                              {state.position_source_allocations[p.token_id].map((source: PositionSourceAllocation) => <span key={source.address} title={`${source.address} · ${Number(source.shares).toFixed(4)} shares · basis $${Number(source.cost_basis).toFixed(2)}`}>
                                {source.name}: {Number(source.shares).toFixed(2)} shares
                              </span>)}
                            </div>
                          ) : sourceWallet !== 'Unknown' ? (
                            <div style={{ display: 'flex', alignItems: 'center', gap: '6px' }}>
                              <span style={{ color: 'var(--text-secondary)' }} title={sourceWallet}>{targetNames.get(sourceWallet.toLowerCase()) || shortWallet}</span>
                              <CopyWalletButton wallet={sourceWallet} />
                            </div>
                          ) : (
                            <span style={{ fontFamily: 'monospace', color: 'var(--text-secondary)' }} title={sourceWallet}>{shortWallet}</span>
                          )}
                        </td>
                        <td>{parseFloat(p.size).toFixed(2)}</td>
                        <td>${ourAvg.toFixed(3)}</td>
                        <td>{hasPrice ? `$${curPrice.toFixed(3)}` : 'Unavailable'}<div style={{color:'var(--text-secondary)',fontSize:'11px'}}>{hasPrice ? `${metadata?.price_source || ''}${state.price_updated_at?.[p.token_id] ? ` · ${Math.max(0, Math.floor(Date.now()/1000 - Number(state.price_updated_at[p.token_id])))}s ago` : ''}` : 'No current quote'}</div></td>
                        <td>{targetAvg > 0 ? `$${targetAvg.toFixed(3)}` : '-'}</td>
                        <td className={targetAvg > 0 ? (diffPct > 0 ? 'val-negative' : 'val-positive') : ''}>
                          {targetAvg > 0 ? `${diffPct > 0 ? '+' : ''}${diffPct.toFixed(1)}%` : '-'}
                        </td>
                        <td className={pnlUsd !== 0 ? (pnlUsd > 0 ? 'val-positive' : 'val-negative') : ''}>
                          {hasPrice ? `${pnlUsd > 0 ? '+' : ''}$${pnlUsd.toFixed(2)} (${pnlPct > 0 ? '+' : ''}${pnlPct.toFixed(1)}%)` : 'Unavailable'}
                        </td>
                        <td><span className="status status-HELD">OPEN / HELD</span>{metadata?.end_date && <div style={{fontSize:'11px',color:'var(--text-secondary)'}}>{new Date(metadata.end_date).toLocaleString()}</div>}</td>
                        <td>{state.position_source_allocations?.[p.token_id]?.length ? state.position_source_allocations[p.token_id].map((source) => <button key={source.address} disabled={saleBusy || state.mode !== 'simulation'} onClick={() => void submitPaperSale('position',p.token_id,source.address)} title={`Sell ${source.name}'s shares only`}>Sell {source.name}</button>) : <button disabled>Unattributed</button>}</td>
                      </tr>
                    );
                  })}
                  {openPositions.length === 0 && (
                    <tr><td colSpan={10} style={{ textAlign: 'center', color: 'var(--text-secondary)' }}>No open positions</td></tr>
                  )}
                </tbody>
              </table>
            </div>
          </div>

          {closedMarketPositions.length > 0 && <div className="glass-panel">
            <div className="panel-header">Closed markets · checking confirmed payout <span className="panel-subtitle">{closedMarketPositions.length} positions</span></div>
            <p style={{ color: 'var(--text-secondary)', margin: '0 0 1rem' }}>The bot checks the authoritative market result automatically. Once confirmed by Gamma and CLOB, it credits payout proceeds to the paper balance and records realized PnL using each source’s own lots. The reference price below is indicative only; unresolved positions stay here and can be retried with Force Redeem (paper).</p>
            <div className="table-container"><table>
              <thead><tr><th>Market</th><th>Outcome</th><th>Shares</th><th>Entry average</th><th>Last reference</th><th>Indicative PnL*</th><th>Status</th></tr></thead>
              <tbody>{closedMarketPositions.map((p) => {
                const details = state.position_details?.[p.token_id];
                const title = details?.title || `${p.token_id.substring(0, 15)}...`;
                const avg = Number(p.average_entry_price || 0);
                const size = Number(p.size || 0);
                const price = state.position_prices?.[p.token_id] !== undefined ? Number(state.position_prices[p.token_id]) : null;
                const estimate = price === null ? null : (price - avg) * size;
                return <tr key={p.token_id}>
                  <td className="td-truncate" title={title} style={{ maxWidth: '320px' }}>{title}</td>
                  <td>{details?.outcome || '—'}</td><td>{size.toFixed(2)}</td><td>${avg.toFixed(3)}</td>
                  <td>{price === null ? 'Unavailable' : `$${price.toFixed(3)}`}</td>
                  <td className={estimate === null ? '' : estimate >= 0 ? 'val-positive' : 'val-negative'}>{estimate === null ? 'Pending result' : `${estimate >= 0 ? '+' : ''}$${estimate.toFixed(2)} (estimate)`}</td>
                  <td>Awaiting confirmed payout</td>
                </tr>;
              })}</tbody>
            </table></div>
            <small style={{ color: 'var(--text-secondary)' }}>* Reference estimate only; settled PnL and cash proceeds update after a confirmed winner/payout.</small>
          </div>}

          <div className="glass-panel" style={{ marginBottom: '1.5rem' }}>
            <div className="panel-header">Signal monitoring</div>
            <p>WebSocket primary · Data API recovery targets 5s with per-wallet backoff · maker and taker fills. Recovery searches 5 minutes of fills; BUY and SELL age limits apply separately.</p>
            {state.metrics && <p>Unique signals: {state.metrics.unique_signals_delivered} · Duplicates filtered: {state.metrics.duplicate_signals} · Poll errors: {state.metrics.polling_errors} · Page-limit warnings: {state.metrics.polling_page_limit_hits} · Last poll: {state.metrics.last_poll_duration_ms ?? '—'} ms</p>}
            {state.metrics && <p>Receive → order dispatch, last 5 minutes: p50 {state.metrics.receive_to_dispatch_p50_ms ?? '—'} ms · p95 {state.metrics.receive_to_dispatch_p95_ms ?? '—'} ms · {state.metrics.dispatch_latency_samples} measured orders</p>}
            <p style={{ color: 'var(--text-secondary)' }}>A received signal can still be skipped; the Live Feed shows the reason. Signals validated is a pre-execution count.</p>
          </div>
          <Operations mode={state.mode} />
          <div className="grid-cols-2">
            <div style={{ display: 'flex', flexDirection: 'column', gap: '1.5rem' }}>

            {/* Active API Limit Orders */}
            <div className="glass-panel">
              <div className="panel-header">
                {state.mode === 'simulation' ? 'Paper orders processing' : 'Active API Limits'}
                <div style={{ display: 'flex', gap: '8px', alignItems: 'center' }}>
                  <span className="live-badge" style={{ fontSize: '0.6rem', padding: '2px 4px' }}>{state.mode === 'simulation' ? 'SIMULATION' : 'LIVE BOOK'}</span>
                  <span className="panel-subtitle">Pending: {pendingOrders.length}</span>
                </div>
              </div>
              <div className="table-container">
                <table>
                  <thead>
                    <tr>
                      <th>Market</th>
                      <th>Source</th>
                      <th>Size</th>
                      <th>Limit Price</th>
                      <th>Status</th>
                    </tr>
                  </thead>
                  <tbody>
                    {pendingOrders.map((order, i) => {
                      const tokenId = order.token_id;
                      const t = targets.find((x) => x.token_id === tokenId);
                      const title = t ? t.title : `${tokenId.substring(0, 15)}...`;
                      const sourceWallet = state.position_sources ? (state.position_sources[tokenId] || 'Unknown') : 'Unknown';
                      const shortWallet = sourceWallet === 'Unknown' ? 'Unknown' : `${sourceWallet.substring(0, 6)}...${sourceWallet.substring(sourceWallet.length - 4)}`;
                      
                      return (
                        <tr key={i}>
                          <td className="td-truncate" title={title}>{title}</td>
                          <td>
                            {sourceWallet !== 'Unknown' ? (
                              <div style={{ display: 'flex', alignItems: 'center', gap: '6px' }}>
                                <span style={{ fontFamily: 'monospace', color: 'var(--text-secondary)' }} title={sourceWallet}>{shortWallet}</span>
                                <CopyWalletButton wallet={sourceWallet} />
                              </div>
                            ) : (
                              <span style={{ fontFamily: 'monospace', color: 'var(--text-secondary)' }} title={sourceWallet}>{shortWallet}</span>
                            )}
                          </td>
                          <td><span className={`side-${order.side}`}>{order.side}</span> {parseFloat(order.size).toFixed(2)}</td>
                          <td>${parseFloat(order.price).toFixed(3)}</td>
                          <td><span className={`status status-${order.side === 'BUY' ? 'QUEUED' : 'HELD'}`}>PENDING {order.side}</span></td>
                        </tr>
                      );
                    })}
                    {pendingOrders.length === 0 && (
                      <tr><td colSpan={5} style={{ textAlign: 'center', color: 'var(--text-secondary)' }}>{state.mode === 'simulation' ? 'No orders processing. Paper orders fill immediately; see Paper Positions and trade history.' : 'No resting limit orders'}</td></tr>
                    )}
                  </tbody>
                </table>
              </div>
            </div>

            {/* Target Positions (Scanning/Watching) */}
            <div className="glass-panel">
              <div className="panel-header">
                <span>Target positions</span>
                <span className="panel-subtitle">{totalTargets.toLocaleString()} monitored · next scan in {state.next_scan_secs}s</span>
              </div>
              <p className="target-table-intro">Each row is one leader’s position. “Our share” is matched by token and source wallet, so a position copied from another leader is not credited to this one.</p>
              <div className="table-container" style={{ maxHeight: '400px', overflowY: 'auto' }}>
                <table>
                  <thead>
                    <tr>
                      <th>Leader</th>
                      <th>Market</th>
                      <th>Side</th>
                      <th>Target shares</th>
                      <th>Target Avg</th>
                      <th>Cur Price</th>
                      <th>PnL</th>
                      <th>Our share / decision</th>
                    </tr>
                  </thead>
                  <tbody>
                    {targets.slice(0, targetLimit).map((t) => {
                      const statusKey = getStatusLabel(t.status);
                      const pnl = parseFloat(t.percent_pnl) * 100;
                      const sourceAddress = String(t.source_wallet || '').toLowerCase();
                      const allocations = state.position_source_allocations?.[t.token_id] || [];
                      const sourceAllocations = allocations.filter((allocation) => String(allocation.address || '').toLowerCase() === sourceAddress);
                      const ourShares = sourceAllocations.reduce((sum, allocation) => sum + Number(allocation.shares || 0), 0);
                      const sourceLabel = targetNames.get(sourceAddress) || (sourceAddress ? `${sourceAddress.slice(0, 6)}…${sourceAddress.slice(-4)}` : 'Unknown leader');
                      const otherSources = allocations.filter((allocation) => String(allocation.address || '').toLowerCase() !== sourceAddress);
                      const sourceFailure = (state.live_feed || []).find((trade) =>
                        !trade.validated && trade.reason &&
                        String(trade.original_event.token_id) === String(t.token_id) &&
                        String(trade.original_event.taker_address || '').toLowerCase() === sourceAddress
                      );
                      const statusReason: Record<string, string> = {
                        Monitoring: sourceFailure?.reason || t.engine_reason || 'No fill attributed to this leader yet; checking whether a catch-up entry is still eligible',
                        Entered: 'Copy order submitted; fill is being tracked',
                        SkippedOwned: otherSources.length ? `Token already held via ${otherSources.map((item) => item.name).join(', ')}` : 'Token already held in our portfolio',
                        SkippedLoss: 'Leader position is beyond the configured loss threshold',
                        SkippedGain: 'Leader position is beyond the configured gain threshold',
                        SkippedPrice: 'Current price is outside the configured entry range',
                        SkippedExpired: 'Market has resolved or passed its end date',
                        SkippedSize: 'Leader position is outside the configured size limits',
                      };
                      return (
                        <tr key={`${t.token_id}:${sourceAddress}`}>
                          <td><strong>{sourceLabel}</strong></td>
                          <td className="td-truncate" title={t.title}>{t.title}</td>
                          <td>{t.outcome}</td>
                          <td>{parseFloat(t.size).toLocaleString(undefined, {maximumFractionDigits: 2})}</td>
                          <td>${parseFloat(t.avg_price).toFixed(3)}</td>
                          <td>${parseFloat(t.cur_price).toFixed(3)}</td>
                          <td className={pnl < 0 ? 'val-negative' : pnl > 0 ? 'val-positive' : ''}>
                            {pnl > 0 ? '+' : ''}{pnl.toFixed(1)}%
                          </td>
                          <td className="copy-decision-cell">
                            <span className={`copy-share ${ourShares > 0 ? 'has-copy' : 'no-copy'}`}>
                              {ourShares > 0 ? `${ourShares.toLocaleString(undefined, {maximumFractionDigits: 2})} shares copied` : 'No copied shares'}
                            </span>
                            <span className="copy-decision-reason">{statusReason[statusKey] || statusKey}</span>
                          </td>
                        </tr>
                      );
                    })}
                  </tbody>
                </table>
              </div>
              <p style={{color:'var(--text-secondary)',margin:'0.75rem 0 0'}}>Showing {Math.min(targetLimit, targets.length)} of {totalTargets.toLocaleString()} leader positions.</p>
              {targetLimit < totalTargets && <button className="btn btn-secondary" disabled={loadingMoreTargets} onClick={loadMoreTargets}>{loadingMoreTargets ? 'Loading…' : 'Load 50 more positions'}</button>}
            </div>

          </div>

          <div className="glass-panel" style={{ alignSelf: 'flex-start', position: 'sticky', top: '2rem' }}>
            <div className="panel-header">Live Feed</div>
            <div className="feed-list" style={{ maxHeight: '800px' }}>
              {(state.live_feed || []).slice(0, 20).map((feed, i) => {
                const ev = feed.original_event;
                return (
                  <div key={i} className="feed-item">
                    <div className="feed-item-header">
                      <span className={`side-${ev.side}`}>{ev.side}</span>
                      <span className="feed-token">{ev.token_id.substring(0, 8)}...</span>
                      <span>${parseFloat(ev.price).toFixed(3)}</span>
                    </div>
                    <div style={{ fontSize: '0.875rem' }}>Size: {parseFloat(ev.size).toFixed(2)}</div>
                    {!feed.validated && feed.reason && (
                      <div className="feed-reason">Skipped: {feed.reason}</div>
                    )}
                  </div>
                );
              })}
            </div>
          </div>
        </div>
      </div>
      ) : (
        <SettingsManager />
      )}
    </div>
  );
}

export default App;
