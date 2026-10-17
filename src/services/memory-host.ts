import { invoke } from '@tauri-apps/api/core';
import { captureTargets } from '@/lib/memory';
import type { ChatService } from '@/types';
import type {
  IngestSummary,
  MemoryListResult,
  MemoryQuery,
  MemorySettings,
  MemoryStatus,
} from '@/types/memory';

/** Idempotent. Rust restarts the sidecar only when generation, embedding, or python changed. */
export function syncMemoryRuntime(
  settings: MemorySettings,
  services: ChatService[]
): Promise<MemoryStatus> {
  return invoke<MemoryStatus>('memory_sync_runtime', {
    settings,
    captureTargets: captureTargets(settings, services),
  });
}

/** Probe, plan, add, and ledger all run inside Rust. */
export function ingestMemories(): Promise<IngestSummary> {
  return invoke<IngestSummary>('memory_ingest');
}

/** Starts the sidecar on demand; never cached on either side. */
export function listMemories(query: MemoryQuery): Promise<MemoryListResult> {
  return invoke<MemoryListResult>('memory_list', { query });
}

export function getMemoryStatus(): Promise<MemoryStatus> {
  return invoke<MemoryStatus>('memory_status');
}
