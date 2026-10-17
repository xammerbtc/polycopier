export type Decimal = string;

export interface Position {
  token_id: string;
  size: Decimal;
  average_entry_price: Decimal;
}

export interface MarketDetails {
  title: string;
  outcome: string;
  end_date: string | null;
  closed: boolean;
  reference_price: Decimal | null;
  price_source: string;
}

export type ScanStatus =
  | 'Monitoring'
  | 'Entered'
  | 'SkippedOwned'
  | 'SkippedLoss'
  | 'SkippedGain'
  | 'SkippedPrice'
  | 'SkippedExpired'
  | 'SkippedSize';

export interface TargetPosition {
  title: string;
  outcome: string;
  token_id: string;
  cur_price: Decimal;
  avg_price: Decimal;
  percent_pnl: Decimal;
  size: Decimal;
  status: ScanStatus | Record<string, unknown>;
  source_wallet: string;
  engine_reason: string | null;
}

export interface TargetPerformance {
  address: string;
  name: string;
  copy_pct: Decimal;
  rating: number;
  copied_entries: number;
  closed_entries: number;
  recent_signals: number;
  recent_skips: number;
  open_positions: number;
  open_cost: Decimal;
  open_value: Decimal;
  open_pnl: Decimal;
  realized_pnl: Decimal;
  total_pnl: Decimal;
  roi_pct: Decimal | null;
}

export interface PositionSourceAllocation {
  address: string;
  name: string;
  shares: Decimal;
  cost_basis: Decimal;
  open_pnl: Decimal | null;
}

export interface Order {
  token_id: string;
  price: Decimal;
  size: Decimal;
  side: 'BUY' | 'SELL';
}

export interface FeedTrade {
  original_event: {
    transaction_hash: string;
    maker_address: string;
    taker_address: string;
    token_id: string;
    price: Decimal;
    size: Decimal;
    side: 'BUY' | 'SELL';
    timestamp: number;
  };
  validated: boolean;
  reason: string | null;
}

export interface Metrics {
  execution_mode: string;
  unique_signals_delivered: number;
  duplicate_signals: number;
  polling_errors: number;
  polling_page_limit_hits: number;
  last_poll_duration_ms: number | null;
  dispatch_latency_samples: number;
  receive_to_dispatch_p50_ms: number | null;
  receive_to_dispatch_p95_ms: number | null;
}

export interface StateResponse {
  position_details: Record<string, MarketDetails>;
  position_prices: Record<string, Decimal>;
  prices_updated_at: number;
  price_updated_at: Record<string, number>;
  paused: boolean;
  paper_integrity_fault: boolean;
  paper_settlement_last_attempt: number | null;
  paper_settlement_last_lookup: number | null;
  paper_settlement_last_error: string | null;
  paper_settlement_market_failures: number;
  mode: string;
  live_ready: boolean;
  live_blockers: string[];
  daily_volume_usd: Decimal;
  positions: Record<string, Position>;
  live_feed: FeedTrade[];
  total_balance: Decimal;
  starting_balance: Decimal;
  unrealized_pnl: Decimal;
  realized_pnl: Decimal;
  open_cost_basis: Decimal;
  open_market_value: Decimal | null;
  equity: Decimal | null;
  total_pnl: Decimal | null;
  priced_positions: number;
  target_positions: TargetPosition[];
  target_positions_total: number;
  copies_executed: number;
  filled_buy_orders: number | null;
  trades_skipped: number;
  skip_attempts: Record<string, number>;
  copied_count: number;
  next_scan_secs: number;
  pending_orders: Record<string, Order>;
  active_orders: Order[];
  position_sources: Record<string, string>;
  position_source_allocations: Record<string, PositionSourceAllocation[]>;
  target_performance: TargetPerformance[];
  performance_since: string;
  metrics?: Metrics | null;
}

export interface BotConfig {
  targets: { wallets: string[]; ratings: string[] };
  sizing: { mode: string; copy_size_pct: string | number };
  execution: {
    max_delay_seconds: number;
    max_buy_delay_seconds?: number | null;
    ignore_closing_in_mins: number | null;
    max_trade_size_usd: string | number;
    max_slippage_pct: string | number;
  };
  scanner: {
    max_entries_per_cycle: number;
    max_copy_loss_pct: string | number;
    max_copy_gain_pct: string | number;
    min_entry_price: string | number;
    max_entry_price: string | number;
    min_amount?: string | number | null;
    max_amount?: string | number | null;
  };
  risk: {
    max_consecutive_losses: number;
    loss_cooldown_secs: number;
    profit_guard_trigger_pct: string | number;
    profit_guard_loss_pct: string | number;
  };
  [key: string]: unknown;
}

export interface EnvData {
  private_key: string;
  funder_address: string;
}

export interface LedgerEntry {
  copied_at: string;
  source_wallet: string;
  token_id: string;
  entry_price: Decimal;
  size: Decimal;
  closed: boolean;
}

export interface RedemptionRecord {
  status: string;
  proof: unknown;
}

export interface RedemptionStatus {
  live_configured: boolean;
  automatic_enabled: boolean;
  records: Record<string, RedemptionRecord>;
}
