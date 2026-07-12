export interface ChordEngineEndpoint {
  name: string;
  endpoint_env_var: string;
  status: 'online' | 'degraded' | 'offline';
  models: ChordLoadedModel[];
  response_time_ms: number;
}
export interface ChordLoadedModel {
  name: string;
  size_vram_mb: number;
  active_requests: number;
  tokens_per_sec?: number;
}
export interface ChordVRAMState {
  total_mb: number;
  used_mb: number;
  free_mb: number;
  allocations: ChordVRAMAllocation[];
}
export interface ChordVRAMAllocation {
  model_name: string;
  engine: string;
  size_mb: number;
  loaded_at: string;
}
export interface ChordModelRecord {
  name: string;
  file_path: string;
  size_bytes: number;
  quant_level?: string;
  engine_compat: string[];
  storage_tier: 'hot' | 'warm';
  last_used?: string;
  loaded: boolean;
}
export interface ChordStorageDisk {
  path: string;
  free_bytes: number;
  used_bytes: number;
  total_bytes: number;
  model_bytes: number;
}
export interface ChordStorageLocation {
  name: string;
  path: string;
  tier: 'hot' | 'warm';
  model_count: number;
  model_bytes: number;
  disk: ChordStorageDisk;
}
export interface ChordInferenceState {
  engines: ChordEngineEndpoint[];
  vram: ChordVRAMState;
  timestamp: string;
}
