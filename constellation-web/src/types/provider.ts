export type ProviderType = 'local' | 'cloud';
export type ProviderHealth = 'healthy' | 'degraded' | 'error' | 'unknown';
export type ProviderTier = 'free' | 'standard' | 'premium';

export interface ProviderModel {
  id: string;
  name: string;
  tier: ProviderTier;
  context_window: number;
}

export interface ProviderUsage {
  tokens_24h: number;
  requests_24h: number;
  rate_limit_pct: number;
  sparkline_24h: number[];
}

export interface ProviderCost {
  budget_usd: number;
  used_usd: number;
  pct_used: number;
}

export interface Provider {
  name: string;
  display_name: string;
  type: ProviderType;
  enabled: boolean;
  status: ProviderHealth;
  latency_ms: number | null;
  last_checked: string | null;
  active_tasks: number;
  models: ProviderModel[];
  usage: ProviderUsage;
  cost?: ProviderCost;
}
