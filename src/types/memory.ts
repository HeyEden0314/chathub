/**
 * SECURITY: `apiKey` fields are stored in plaintext in this webview profile's
 * localStorage (`chat-box-app-storage`) and are visible in the sidecar's process
 * environment to other processes of the same OS user. There is no keychain.
 */
export interface ModelEndpoint {
  /** '' means the OpenAI SDK default host. */
  baseUrl: string;
  apiKey: string;
  model: string;
}

export type EmbeddingSource =
  | { kind: 'reuse-generation' }
  | { kind: 'separate'; baseUrl: string; apiKey: string };

/**
 * Model, dims, and endpoint host select the on-disk memory store. Changing them
 * switches to another store directory instead of mixing incompatible vectors.
 */
export interface EmbeddingSettings {
  source: EmbeddingSource;
  model: string;
  dims: number;
}

export interface MemorySettings {
  enabled: boolean;
  /**
   * Opt-in capture list, intersected with enabled services. Lives here, not on
   * ChatService, because the store's `merge` drops unknown fields on builtin rows.
   */
  captureServiceIds: string[];
  generation: ModelEndpoint;
  embedding: EmbeddingSettings;
  /** '' = auto-detect (`python3`, `python`, Windows `py -3`). */
  pythonCommand: string;
}

export const DEFAULT_MEMORY_SETTINGS: MemorySettings = {
  enabled: false,
  captureServiceIds: [],
  generation: { baseUrl: '', apiKey: '', model: '' },
  embedding: { source: { kind: 'reuse-generation' }, model: 'text-embedding-3-small', dims: 1536 },
  pythonCommand: '',
};

export type SettingsTab = 'services' | 'memory' | 'about';

export interface SettingsTabEntry {
  id: SettingsTab;
  label: string;
  icon: 'Settings' | 'Brain' | 'Info';
}

export const SETTINGS_TABS: readonly SettingsTabEntry[] = [
  { id: 'services', label: '服务管理', icon: 'Settings' },
  { id: 'memory', label: '记忆', icon: 'Brain' },
  { id: 'about', label: '关于', icon: 'Info' },
];

// Read models below are serialized by `src-tauri/src/memory/dto.rs`.
// `memory-contract.generated.ts` holds one Rust-produced sample of every variant
// and is checked against these types by `tsc`.

export type ConfigField =
  | 'generation.baseUrl'
  | 'generation.apiKey'
  | 'generation.model'
  | 'embedding.baseUrl'
  | 'embedding.apiKey'
  | 'embedding.model'
  | 'embedding.dims'
  | 'pythonCommand'
  | 'captureServiceIds';

export interface ConfigProblem {
  field: ConfigField;
  message: string;
}

/** What the running (or next) sidecar was configured with. Never contains the key. */
export interface AppliedRuntime {
  model: string;
  endpointHost: string;
  embeddingModel: string;
  storeId: string;
}

export type SidecarFailure =
  | { kind: 'python-not-found'; tried: string[] }
  | { kind: 'module-missing'; python: string; installHint: string }
  | { kind: 'exited-on-start'; code: number | null; stderrTail: string }
  | { kind: 'health-timeout'; afterMs: number }
  | { kind: 'model-check-failed'; message: string }
  | { kind: 'crash-loop'; restarts: number; stderrTail: string };

/** `idle` = configured and allowed to run, process stopped until there is work. */
export type RuntimePhase =
  | { phase: 'off' }
  | { phase: 'needs-config'; problems: ConfigProblem[] }
  | { phase: 'idle'; applied: AppliedRuntime }
  | { phase: 'starting'; applied: AppliedRuntime }
  | { phase: 'ready'; applied: AppliedRuntime }
  | {
      phase: 'backoff';
      applied: AppliedRuntime;
      attempt: number;
      retryAt: number;
      lastExit: string;
    }
  | { phase: 'failed'; applied: AppliedRuntime | null; failure: SidecarFailure };

export type ProbeHealth =
  | { kind: 'not-open' }
  | { kind: 'unsupported-site'; host: string }
  | { kind: 'no-conversation' }
  | { kind: 'unrecognized-page'; conversationKey: string }
  | { kind: 'ok'; turns: number; at: number }
  | { kind: 'error'; message: string };

export type IngestSkipReason = 'memory-off' | 'needs-config' | 'runtime-failed' | 'no-targets';

export type IngestSummary =
  | { kind: 'skipped'; reason: IngestSkipReason }
  | { kind: 'coalesced' }
  | {
      kind: 'ran';
      at: number;
      conversationsProbed: number;
      batchesStored: number;
      /** Hy-Memory answered success:false and rolled back. Retried, then abandoned. */
      batchesRejected: number;
      /** Transport failure or the sidecar died mid-add. Outcome unknown; retried. */
      batchesUnknown: number;
      /** Turns seen but not settled yet (still streaming, or a user turn without reply). */
      turnsDeferred: number;
      memoriesProduced: number;
    };

export interface IngestActivity {
  state: 'idle' | 'running';
  lastRun: IngestSummary | null;
  /** Turns abandoned under the current generation model. */
  abandonedTurns: number;
}

export interface MemoryStatus {
  runtime: RuntimePhase;
  ingest: IngestActivity;
  /** Keyed by service id; only effective capture targets appear. */
  probes: Record<string, ProbeHealth>;
  /** Host suffixes the Rust extractor registry understands. */
  supportedHosts: string[];
}

export type MemoryKind = 'profile' | 'fact' | 'intention' | 'schema' | 'summary' | 'other';

export interface MemoryItem {
  id: string;
  text: string;
  kind: MemoryKind;
  /** AnyChat service id; null for user-level profile rows. */
  serviceId: string | null;
  conversationKey: string | null;
  /** epoch ms */
  createdAt: number | null;
}

export interface MemoryQuery {
  serviceId?: string;
  offset: number;
  limit: number;
}

export type MemoryListResult =
  | { kind: 'page'; items: MemoryItem[]; total: number; nextOffset: number | null; dropped: number }
  | { kind: 'unavailable'; runtime: RuntimePhase };

export interface MemoryContractFixture {
  statuses: MemoryStatus[];
  ingestSummaries: IngestSummary[];
  listResults: MemoryListResult[];
  configFields: ConfigField[];
}
