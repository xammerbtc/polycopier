import { useState, useEffect } from "react";
import type { BotConfig, EnvData } from "./types";

export default function SettingsManager() {
  const [config, setConfig] = useState<BotConfig | null>(null);
  const [envData, setEnvData] = useState<EnvData | null>(null);
  const [loadError, setLoadError] = useState("");
  const [retryKey, setRetryKey] = useState(0);
  const [loading, setLoading] = useState(true);
  const [isSaving, setIsSaving] = useState(false);
  const [message, setMessage] = useState("");

  useEffect(() => {
    const controller = new AbortController();
    const timeout = window.setTimeout(() => controller.abort(), 8000);
    setLoading(true);
    setLoadError("");

    // Configuration is required to edit settings. Credential metadata is
    // optional and must never hold the whole control center behind a spinner.
    fetch("/api/config", { signal: controller.signal })
      .then(async (res) => {
        const contentType = res.headers.get("content-type") || "";
        const body = contentType.includes("application/json")
          ? await res.json()
          : { error: await res.text() };
        if (!res.ok) throw new Error(body.error || `Configuration request failed (${res.status})`);
        if (!body || typeof body !== "object") throw new Error("Configuration response was empty.");
        const loaded = body as BotConfig;
        loaded.targets.wallets = loaded.targets.wallets.map((wallet) => wallet.trim()).filter(Boolean);
        setConfig(loaded);
      })
      .catch((error) => {
        if (error instanceof Error && error.name === "AbortError") {
          setLoadError("Configuration request timed out. Check the local dashboard connection and retry.");
        } else {
          setLoadError(error instanceof Error ? error.message : "Could not load configuration.");
        }
      })
      .finally(() => {
        window.clearTimeout(timeout);
        setLoading(false);
      });

    const envController = new AbortController();
    const envTimeout = window.setTimeout(() => envController.abort(), 4000);
    fetch("/api/env", { signal: envController.signal })
      .then((res) => res.ok ? res.json() : null)
      .then((data) => { if (data) setEnvData(data); })
      .catch(() => { /* optional status only */ })
      .finally(() => window.clearTimeout(envTimeout));

    return () => {
      controller.abort();
      envController.abort();
      window.clearTimeout(timeout);
      window.clearTimeout(envTimeout);
    };
  }, [retryKey]);

  const handleSave = async () => {
    if (!config) return;
    setIsSaving(true);
    setMessage("");

    try {
      // Clean up target wallets array if string
      const payloadConfig: BotConfig = {
        ...config,
        targets: {
          ...config.targets,
          wallets: config.targets.wallets.map((wallet) => wallet.trim()).filter(Boolean),
        },
      };

      const response = await fetch("/api/config", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(payloadConfig),
      });

      const contentType = response.headers.get("content-type") || "";
      const result = contentType.includes("application/json")
        ? await response.json()
        : { error: await response.text() };
      if (!response.ok) throw new Error(result.error || "Save failed");
      setMessage("Saved. Targets and risk settings apply automatically without restarting.");
    } catch (e) {
      setMessage(e instanceof Error ? e.message : "Failed to save.");
    }

    setIsSaving(false);
  };



  if (loading && !config) return <div className="loading-container settings-loading"><div className="spinner"></div><div>Loading settings…</div></div>;
  if (!config) return <div className="glass-panel settings-load-error" role="alert"><div className="panel-header">Settings could not be loaded</div><p>{loadError || "The configuration service did not return a valid response."}</p><button className="btn btn-secondary" onClick={() => setRetryKey((key) => key + 1)}>Retry</button></div>;

  const isSelfPct = config.sizing.mode === "self_pct";
  const isBankScaled = config.sizing.mode === "bank_scaled";
  const hasLossGuard = Number(config.risk.max_consecutive_losses || 0) > 0;

  return (
    <div className="settings-container">
      <div className="glass-panel" style={{ marginBottom: "1.5rem" }}>
        <div className="panel-header">Credentials managed on VPS</div>
        {envData ? <>
          <div className="form-group">
            <label>Private keys are never returned by the dashboard API</label>
            <input type="password" disabled value={envData.private_key || ""} placeholder="Stored securely on VPS" />
          </div>
          <div className="form-group">
            <label>Funder Wallet Address (Polymarket Proxy)</label>
            <input type="text" disabled value={envData.funder_address || ""} placeholder="Not configured" />
          </div>
        </> : <p className="field-hint">Credential status is temporarily unavailable. This does not block configuration editing.</p>}
      </div>

      <div className="grid-cols-2">
        <div style={{ display: 'flex', flexDirection: 'column', gap: '1.5rem' }}>
          <div className="glass-panel">
            <div className="panel-header">Targets Network</div>
            <div className="form-group">
              <label>Wallets to Copy</label>
              <p className="field-hint" style={{ marginBottom: '1rem' }}>
                Add proxy wallet addresses to copy trade. When using the <strong>Target Scalar</strong> sizing mode, you can select exactly what percentage of their order size you want to copy.
                <br/><span style={{ opacity: 0.8, marginTop: '2px', display: 'block' }}><em>Example: If a whale bets $500 with a $100k portfolio and you have $1k (1%), setting this to <strong>1%</strong> will perfectly scale that same bet down to $5 for you.</em></span>
              </p>
              {config.targets.wallets.map((w, idx) => {
                const parts = w.split(':');
                const cleanWallet = parts[0] || "";
                const cleanScalar = parts.length > 1 ? parts[1] : "1.0";
                const cleanName = parts.length > 2 ? parts.slice(2).join(':') : "";
                
                return (
                <div key={idx} style={{ display: 'flex', gap: '0.5rem', marginBottom: '0.5rem' }}>
                  <button className="btn btn-secondary" onClick={() => {
                    const next = [...config.targets.wallets];
                    next[idx] = `${cleanWallet}:${parseFloat(cleanScalar) === 0 ? '1' : '0'}:${cleanName}`;
                    setConfig({...config, targets:{...config.targets, wallets:next}});
                  }}>{parseFloat(cleanScalar) === 0 ? 'Enable' : 'Disable'}</button>
                  <input
                    type="text"
                    value={cleanWallet}
                    onChange={(e) => {
                      const newWallets = [...config.targets.wallets];
                      newWallets[idx] = `${e.target.value}:${cleanScalar}:${cleanName}`;
                      setConfig({ ...config, targets: { ...config.targets, wallets: newWallets }});
                    }}
                    placeholder="0x..."
                    style={{ flex: 1 }}
                  />
                  <input
                    type="text"
                    value={cleanName}
                    aria-label={`Wallet ${idx + 1} name`}
                    onChange={(e) => {
                      const newWallets = [...config.targets.wallets];
                      newWallets[idx] = `${cleanWallet}:${cleanScalar}:${e.target.value.replaceAll(':', ' ')}`;
                      setConfig({ ...config, targets: { ...config.targets, wallets: newWallets }});
                    }}
                    placeholder="Wallet name"
                    style={{ width: '150px' }}
                  />
                  {config.sizing?.mode === 'target_scalar' && (
                    <div style={{ display: 'flex', alignItems: 'center', gap: '0.5rem', background: 'var(--panel-bg)', padding: '0 0.5rem', borderRadius: '4px', border: '1px solid var(--border-color)', minWidth: '130px' }}>
                      <span style={{color: 'var(--text-secondary)', fontSize: '0.9rem'}}>Copy</span>
                      <input
                        type="number"
                        step="0.1"
                        min="0.1"
                        value={parseFloat(cleanScalar) * 100}
                        onChange={(e) => {
                          const val = Math.max(0, parseFloat(e.target.value) || 0);
                          const scaled = (val / 100).toFixed(4).replace(/\.?0+$/, '');
                          const newWallets = [...config.targets.wallets];
                          newWallets[idx] = `${cleanWallet}:${scaled}:${cleanName}`;
                          setConfig({ ...config, targets: { ...config.targets, wallets: newWallets }});
                        }}
                        style={{ width: '40px', background: 'transparent', border: 'none', padding: '0', textAlign: 'right', fontWeight: 'bold' }}
                      />
                      <span style={{color: 'var(--text-secondary)', fontSize: '0.9rem', paddingRight: '0.25rem'}}>%</span>
                    </div>
                  )}
                  <button
                    className="btn btn-secondary"
                    onClick={() => {
                      const newWallets = config.targets.wallets.filter((_, i) => i !== idx);
                      setConfig({ ...config, targets: { ...config.targets, wallets: newWallets }});
                    }}
                  >
                    X
                  </button>
                </div>
              )})}
              <button
                className="btn btn-primary"
                style={{ marginTop: '0.5rem', alignSelf: 'flex-start' }}
                onClick={() => {
                   const curr = Array.isArray(config.targets.wallets) ? config.targets.wallets : [];
                   setConfig({ ...config, targets: { ...config.targets, wallets: [...curr, ""] }});
                }}
              >
                + Add Wallet
              </button>
              {config.sizing?.mode === 'target_scalar' && (
                <div className="panel-subtitle" style={{ marginTop: '0.5rem' }}>
                  Copy is a percentage of each leader stake. Orders below the venue minimum are skipped, not rounded up.
                </div>
              )}
              <span className="field-hint" style={{ marginTop: '0.5rem', display: 'block' }}>The proxy wallets tracked by the websocket listener natively.</span>
            </div>
            <div className="form-group" style={{ marginTop: '1rem' }}>
              <label>Maximum signal age for SELL exits</label>
              <div style={{ display: 'flex', gap: '0.5rem', alignItems: 'center' }}>
                <input
                  type="number"
                  style={{ width: '100px' }}
                  value={config.execution.max_delay_seconds}
                  onChange={(e) =>
                    setConfig({ ...config, execution: { ...config.execution, max_delay_seconds: parseInt(e.target.value) } })
                  }
                />
                <span className="field-hint">seconds</span>
              </div>
            </div>

            <div className="form-group" style={{ marginTop: '1rem' }}>
              <label>Maximum signal age for BUY (optional)</label>
              <div style={{ display: 'flex', gap: '0.5rem', alignItems: 'center' }}>
                <input
                  type="number"
                  min="1"
                  style={{ width: '100px' }}
                  value={config.execution.max_buy_delay_seconds ?? ''}
                  placeholder="Same as SELL"
                  onChange={(e) => setConfig({
                    ...config,
                    execution: {
                      ...config.execution,
                      max_buy_delay_seconds: e.target.value ? parseInt(e.target.value) : null,
                    },
                  })}
                />
                <span className="field-hint">seconds</span>
              </div>
            </div>
            
            <div className="form-group" style={{ marginTop: '1rem' }}>
              <label>Ignore Markets Closing Within</label>
              <div style={{ display: 'flex', gap: '0.5rem', alignItems: 'center' }}>
                <input
                  type="number"
                  style={{ width: '100px' }}
                  value={(config.execution.ignore_closing_in_mins === 0 || config.execution.ignore_closing_in_mins === null) ? '' : config.execution.ignore_closing_in_mins}
                  placeholder="Disabled"
                  onChange={(e) => {
                    const val = e.target.value;
                    const parsed = val ? parseInt(val) : 0;
                    setConfig({ ...config, execution: { ...config.execution, ignore_closing_in_mins: parsed } })
                  }}
                />
                <span className="field-hint">minutes (Leave blank to disable)</span>
              </div>
            </div>
          </div>

          <div className="glass-panel">
            <div className="panel-header">Scanner Tuning</div>
            <label><input type="checkbox" checked={config.scanner.max_entries_per_cycle > 0} onChange={e => setConfig({...config, scanner:{...config.scanner, max_entries_per_cycle:e.target.checked ? 1 : 0}})} /> Catch-up entries enabled</label>
            <div className="form-group">
              <label>Max Entries Per Scan Cycle</label>
              <input
                type="number"
                style={{ width: '100px' }}
                value={config.scanner.max_entries_per_cycle}
                onChange={(e) =>
                  setConfig({ ...config, scanner: { ...config.scanner, max_entries_per_cycle: parseInt(e.target.value) } })
                }
              />
              <span className="field-hint">How many concurrent Catch-up buys the scanner drops onto the CLOB simultaneously.</span>
            </div>
            <div className="form-group" style={{ marginTop: '1rem' }}>
              <label>Catch-up Loss Tolerance (Downside Threshold)</label>
              <div style={{ display: 'flex', alignItems: 'center', gap: '1rem' }}>
                <input
                  type="range"
                  min="0" max="1.0" step="0.01"
                  value={Number(config.scanner.max_copy_loss_pct) || 0}
                  onChange={(e) =>
                    setConfig({ ...config, scanner: { ...config.scanner, max_copy_loss_pct: e.target.value } })
                  }
                  style={{ flex: 1 }}
                />
                <span className="val-negative" style={{ fontSize: '1.2rem', fontWeight: 600, minWidth: '60px' }}>
                  -{((Number(config.scanner.max_copy_loss_pct) || 0) * 100).toFixed(0)}%
                </span>
              </div>
              <span className="field-hint" style={{ display: 'block', marginTop: '0.25rem' }}>
                If you just turned on the bot and the proxy wallet holds a token that is <strong>deep in the red</strong> (past this percentage), the system will intelligently <em>skip</em> copying it to protect you from catching a falling knife.
              </span>
            </div>

            <div className="form-group" style={{ marginTop: '1.5rem' }}>
              <label>Catch-up FOMO Guard (Upside Threshold)</label>
              <div style={{ display: 'flex', alignItems: 'center', gap: '1rem' }}>
                <input
                  type="range"
                  min="0" max="1.0" step="0.01"
                  value={Number(config.scanner.max_copy_gain_pct) || 0}
                  onChange={(e) =>
                    setConfig({ ...config, scanner: { ...config.scanner, max_copy_gain_pct: e.target.value } })
                  }
                  style={{ flex: 1 }}
                />
                <span className="val-positive" style={{ fontSize: '1.2rem', fontWeight: 600, minWidth: '60px' }}>
                  +{((Number(config.scanner.max_copy_gain_pct) || 0) * 100).toFixed(0)}%
                </span>
              </div>
              <span className="field-hint" style={{ display: 'block', marginTop: '0.25rem' }}>
                If the proxy wallet holds a token that has already <strong>pumped heavily</strong> (past this percentage), the system will <em>skip</em> copying it to prevent you from buying at the absolute top of a missed rally.
              </span>
            </div>
            <div className="form-group" style={{ marginTop: '1rem' }}>
              <label>Entry Price Dust Filter</label>
              <div style={{ display: 'flex', gap: '0.5rem', alignItems: 'center' }}>
                <span style={{ color: 'var(--text-secondary)' }}>$</span>
                <input
                  type="text"
                  placeholder="Min"
                  style={{ width: '80px' }}
                  value={config.scanner.min_entry_price}
                  onChange={(e) => setConfig({ ...config, scanner: { ...config.scanner, min_entry_price: e.target.value } })}
                />
                <span>to</span>
                <span style={{ color: 'var(--text-secondary)' }}>$</span>
                <input
                  type="text"
                  placeholder="Max"
                  style={{ width: '80px' }}
                  value={config.scanner.max_entry_price}
                  onChange={(e) => setConfig({ ...config, scanner: { ...config.scanner, max_entry_price: e.target.value } })}
                />
              </div>
              <span className="field-hint" style={{ display: 'block', marginTop: '0.5rem' }}>
                <strong>Min Filter:</strong> Ignores dead markets (e.g. $0.01) so you don't accumulate abandoned losing tickets from the target's wallet.
              </span>
              <span className="field-hint" style={{ display: 'block', marginTop: '0.25rem' }}>
                <strong>Max Filter:</strong> Ignores fully-realized winning markets (e.g. $0.99) to prevent risking capital for pennies.
              </span>
            </div>

            <div className="form-group" style={{ marginTop: '1rem' }}>
              <label>Target Position Size Filter (USD Value)</label>
              <div style={{ display: 'flex', gap: '0.5rem', alignItems: 'center' }}>
                <span style={{ color: 'var(--text-secondary)' }}>$</span>
                <input
                  type="number"
                  placeholder="Min USD"
                  style={{ width: '100px' }}
                  value={config.scanner.min_amount || ""}
                  onChange={(e) => setConfig({ ...config, scanner: { ...config.scanner, min_amount: e.target.value !== "" ? e.target.value : null } })}
                />
                <span>to</span>
                <span style={{ color: 'var(--text-secondary)' }}>$</span>
                <input
                  type="number"
                  placeholder="Max USD"
                  style={{ width: '100px' }}
                  value={config.scanner.max_amount || ""}
                  onChange={(e) => setConfig({ ...config, scanner: { ...config.scanner, max_amount: e.target.value !== "" ? e.target.value : null } })}
                />
              </div>
              <span className="field-hint" style={{ display: 'block', marginTop: '0.5rem' }}>
                Universal bounds check. Polycopier will skip scanning or copying live trades if the Target's resulting position USD value falls outside this window. Leave blank to disable.
              </span>
            </div>
          </div>
        </div>

        <div style={{ display: 'flex', flexDirection: 'column', gap: '1.5rem' }}>
          <div className="glass-panel">
            <div className="panel-header">Sizing Engine</div>
            <div className="form-group">
              <label>Sizing Strategy Mode</label>
              <select
                value={config.sizing.mode}
                onChange={(e) => setConfig({ ...config, sizing: { ...config.sizing, mode: e.target.value } })}
              >
                <option value="fixed">Fixed (Enforce Max Trade Size)</option>
                <option value="self_pct">Self Percentage (Fraction of our balance)</option>
                <option value="target_usd">Mirror Target USD (Proportional dollar size)</option>
                <option value="target_scalar">Target Scalar (Scale target dollar amount)</option>
                <option value="bank_scaled">Bank-scaled (up to $10 exact; larger: $10 or %)</option>
              </select>
            </div>
            
            {(isSelfPct || isBankScaled) && (
              <div className="form-group dynamic-field" style={{ animation: 'fadeIn 0.3s', marginTop: '1rem' }}>
                <label style={{ color: 'var(--accent-primary)' }}>{isBankScaled ? 'Percent of full source stake above $10' : 'Our Balance Allocation (Copy Size Pct)'}</label>
                <div style={{ display: 'flex', alignItems: 'center', gap: '1rem' }}>
                  <input
                    type="range"
                    min="0" max="1.0" step="0.01"
                    value={Number(config.sizing.copy_size_pct || "0.10")}
                    onChange={(e) =>
                      setConfig({ ...config, sizing: { ...config.sizing, copy_size_pct: e.target.value } })
                    }
                    style={{ flex: 1 }}
                  />
                  <span style={{ fontSize: '1.2rem', fontWeight: 600, minWidth: '60px', color: 'var(--accent-primary)' }}>
                    {((Number(config.sizing.copy_size_pct || "0.10")) * 100).toFixed(1)}%
                  </span>
                </div>
                <span className="field-hint" style={{ display: 'block', marginTop: '0.25rem' }}>
                  {isBankScaled ? 'Source stakes up to $10 copy 1:1. Above $10, spend at least $10 or this fraction of the full source stake, whichever is larger, up to the trade cap. Fee is inside the budget.' : 'Fraction of our total available balance to deploy on each copied trade.'}
                </span>
              </div>
            )}

            <div className="form-group" style={{ marginTop: (isSelfPct || isBankScaled) ? '1.5rem' : '1rem' }}>
              <label>Max Trade Ceiling (Safety Stop)</label>
              <div style={{ display: 'flex', alignItems: 'center', gap: '0.5rem' }}>
                <span style={{ fontSize: '1.2rem', fontWeight: 600, color: 'var(--text-secondary)' }}>$</span>
                <input
                  type="number"
                  min="5"
                  style={{ width: '120px' }}
                  value={config.execution.max_trade_size_usd}
                  onChange={(e) =>
                    setConfig({ ...config, execution: { ...config.execution, max_trade_size_usd: e.target.value } })
                  }
                />
                <span>USD</span>
              </div>
              <span className="field-hint" style={{ display: 'block', marginTop: '0.25rem' }}>
                Regardless of the algorithm above, a single entry will <strong>never</strong> exceed this absolute USD value cap. This strictly safeguards your balance.
              </span>
            </div>
          </div>

          <div className="glass-panel">
            <div className="panel-header">Risk Guards</div>
            <div className="form-group">
              <label>Slippage Aggression</label>
              <div style={{ display: 'flex', alignItems: 'center', gap: '1rem' }}>
                <input
                  type="range"
                  min="0" max="0.10" step="0.001"
                  value={Number(config.execution.max_slippage_pct) || 0}
                  onChange={(e) =>
                    setConfig({ ...config, execution: { ...config.execution, max_slippage_pct: e.target.value } })
                  }
                  style={{ flex: 1 }}
                />
                <span style={{ fontSize: '1.2rem', fontWeight: 600, minWidth: '60px', color: 'var(--accent-warning)' }}>
                  {((Number(config.execution.max_slippage_pct) || 0) * 100).toFixed(1)}%
                </span>
              </div>
              <span className="field-hint" style={{ display: 'block', marginTop: '0.25rem' }}>
                Price buffer strictly added to target's entry price to guarantee prompt execution if the orderbook is violently shifting.
              </span>
            </div>

            <div className="form-group" style={{ marginTop: '1.5rem' }}>
              <label style={{ display: 'flex', alignItems: 'center', gap: '0.5rem', cursor: 'pointer' }}>
                <input
                  type="checkbox"
                  checked={config.risk.max_consecutive_losses > 0}
                  onChange={(e) => {
                    const enabled = e.target.checked;
                    setConfig({ ...config, risk: { ...config.risk, max_consecutive_losses: enabled ? 3 : 0 } });
                  }}
                  style={{ width: 'auto', margin: 0 }}
                />
                <span>Enable Systemic Cooldown Guard</span>
              </label>
              <span className="field-hint" style={{ display: 'block', marginTop: '0.25rem' }}>
                Automatically suspends copying entirely if the target proxy wallet starts bleeding heavily.
              </span>
            </div>

            {hasLossGuard && (
              <div className="form-group dynamic-field" style={{ animation: 'fadeIn 0.3s', paddingLeft: '1.5rem', borderLeft: '2px solid rgba(255,255,255,0.1)' }}>
                <label>Consecutive Realized Losses Limit</label>
                <div style={{ display: 'flex', alignItems: 'center', gap: '0.5rem' }}>
                  <input
                    type="number"
                    min="1"
                    style={{ width: '100px' }}
                    value={config.risk.max_consecutive_losses}
                    onChange={(e) =>
                      setConfig({ ...config, risk: { ...config.risk, max_consecutive_losses: Math.max(1, parseInt(e.target.value) || 1) } })
                    }
                  />
                  <span className="field-hint">losses</span>
                </div>
              </div>
            )}

            {hasLossGuard && (
              <div className="form-group dynamic-field" style={{ animation: 'fadeIn 0.3s' }}>
                <label style={{ color: 'var(--accent-danger)' }}>Loss Trigger Cooldown Window</label>
                <div style={{ display: 'flex', gap: '0.5rem', alignItems: 'center' }}>
                  <input
                    type="number"
                    style={{ width: '100px' }}
                    value={config.risk.loss_cooldown_secs}
                    onChange={(e) =>
                      setConfig({ ...config, risk: { ...config.risk, loss_cooldown_secs: parseInt(e.target.value) } })
                    }
                  />
                  <span className="field-hint">seconds timeout until strategy engine resumes parsing WS events.</span>
                </div>
              </div>
            )}
            <div className="form-group" style={{ marginTop: '1.5rem' }}>
              <label>Paper profit guard: account gain (%)</label>
              <input type="number" min="0" max="100" step="0.1"
                value={Number(config.risk.profit_guard_trigger_pct) * 100}
                onChange={(e) => setConfig({ ...config, risk: { ...config.risk,
                  profit_guard_trigger_pct: (Number(e.target.value) / 100).toString() } })} />
              <span className="field-hint">At 7% account gain, review losing paper positions. Set 0 to disable.</span>
            </div>
            <div className="form-group">
              <label>Paper profit guard: position loss (%)</label>
              <input type="number" min="0" max="100" step="0.1"
                value={Number(config.risk.profit_guard_loss_pct) * 100}
                onChange={(e) => setConfig({ ...config, risk: { ...config.risk,
                  profit_guard_loss_pct: (Number(e.target.value) / 100).toString() } })} />
              <span className="field-hint">At 10% loss, sell only into bids within the configured slippage limit.</span>
            </div>
          </div>
        </div>
      </div>

      <div className="settings-actions" style={{ position: 'sticky', bottom: '-1px', background: 'var(--bg-secondary)', padding: '1rem', borderTop: '1px solid var(--border)', zIndex: 10 }}>
        {message && <span className="message">{message}</span>}
        <button className="btn btn-primary" onClick={handleSave} disabled={isSaving}>
          {isSaving ? "Writing configs..." : "Save Configuration System"}
        </button>
      </div>
    </div>
  );
}
