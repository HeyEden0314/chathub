import { describe, expect, it } from 'vitest';
import { MEMORY_CONTRACT } from '@/types/memory-contract.generated';
import { describeFailure, describeIngest, describeProbe, describeRuntime } from '@/lib/memory';
import type {
  ConfigField,
  IngestSkipReason,
  IngestSummary,
  MemoryListResult,
  ProbeHealth,
  RuntimePhase,
  SidecarFailure,
} from '@/types/memory';

// `Record` over each union forces this list to name every variant.
const PHASES: Record<RuntimePhase['phase'], true> = {
  off: true,
  'needs-config': true,
  idle: true,
  starting: true,
  ready: true,
  backoff: true,
  failed: true,
};
const FAILURES: Record<SidecarFailure['kind'], true> = {
  'python-not-found': true,
  'module-missing': true,
  'exited-on-start': true,
  'health-timeout': true,
  'model-check-failed': true,
  'crash-loop': true,
};
const PROBES: Record<ProbeHealth['kind'], true> = {
  'not-open': true,
  'unsupported-site': true,
  'no-conversation': true,
  'unrecognized-page': true,
  ok: true,
  error: true,
};
const INGESTS: Record<IngestSummary['kind'], true> = { skipped: true, coalesced: true, ran: true };
const SKIPS: Record<IngestSkipReason, true> = {
  'memory-off': true,
  'needs-config': true,
  'runtime-failed': true,
  'no-targets': true,
};
const LISTS: Record<MemoryListResult['kind'], true> = { page: true, unavailable: true };
const FIELDS: Record<ConfigField, true> = {
  'generation.baseUrl': true,
  'generation.apiKey': true,
  'generation.model': true,
  'embedding.baseUrl': true,
  'embedding.apiKey': true,
  'embedding.model': true,
  'embedding.dims': true,
  pythonCommand: true,
  captureServiceIds: true,
};

const sorted = (values: Iterable<string>) => [...new Set(values)].sort();

describe('Rust-generated memory contract', () => {
  const runtimes = MEMORY_CONTRACT.statuses.map((s) => s.runtime as RuntimePhase);

  it('covers every runtime phase and failure kind', () => {
    expect(sorted(runtimes.map((r) => r.phase))).toEqual(sorted(Object.keys(PHASES)));
    const failures = runtimes.flatMap((r) => (r.phase === 'failed' ? [r.failure] : []));
    expect(sorted(failures.map((f) => f.kind))).toEqual(sorted(Object.keys(FAILURES)));
    for (const runtime of runtimes) expect(describeRuntime(runtime).label).not.toBe('');
    for (const failure of failures) expect(describeFailure(failure)).not.toBe('');
  });

  it('covers every probe health kind', () => {
    const probes = MEMORY_CONTRACT.statuses.flatMap(
      (s) => Object.values(s.probes) as ProbeHealth[]
    );
    expect(sorted(probes.map((p) => p.kind))).toEqual(sorted(Object.keys(PROBES)));
    for (const probe of probes) expect(describeProbe(probe)).not.toBe('');
  });

  it('covers every ingest summary and skip reason', () => {
    const summaries = MEMORY_CONTRACT.ingestSummaries as IngestSummary[];
    expect(sorted(summaries.map((s) => s.kind))).toEqual(sorted(Object.keys(INGESTS)));
    const reasons = summaries.flatMap((s) => (s.kind === 'skipped' ? [s.reason] : []));
    expect(sorted(reasons)).toEqual(sorted(Object.keys(SKIPS)));
    for (const summary of summaries) expect(describeIngest(summary)).not.toBe('');
  });

  it('covers both list results and every config field', () => {
    const lists = MEMORY_CONTRACT.listResults as MemoryListResult[];
    expect(sorted(lists.map((l) => l.kind))).toEqual(sorted(Object.keys(LISTS)));
    expect(sorted(MEMORY_CONTRACT.configFields)).toEqual(sorted(Object.keys(FIELDS)));
  });

  it('never carries an API key', () => {
    expect(JSON.stringify(MEMORY_CONTRACT)).not.toMatch(/"apiKey"\s*:/);
  });
});
