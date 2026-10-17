import { describe, expect, it } from 'vitest';
import {
  MODEL_PRESETS,
  applyPreset,
  captureSupport,
  captureTargets,
  describeIngest,
  formatRelativeTime,
  parseMemorySettings,
} from '@/lib/memory';
import { DEFAULT_MEMORY_SETTINGS, type IngestSummary, type MemorySettings } from '@/types/memory';
import type { ChatService } from '@/types';

const service = (id: string, url: string, enabled = true): ChatService => ({
  id,
  name: id,
  url,
  enabled,
  order: 0,
});

const SUPPORTED = ['chatgpt.com', 'chat.openai.com', 'claude.ai', 'gemini.google.com'];

const ran = (overrides: Partial<Extract<IngestSummary, { kind: 'ran' }>>): IngestSummary => ({
  kind: 'ran',
  at: 0,
  conversationsProbed: 1,
  batchesStored: 0,
  batchesRejected: 0,
  batchesUnknown: 0,
  turnsDeferred: 0,
  memoriesProduced: 0,
  ...overrides,
});

describe('parseMemorySettings', () => {
  it('returns defaults for anything that is not an object', () => {
    for (const raw of [undefined, null, 'x', 42, [], true]) {
      expect(parseMemorySettings(raw)).toEqual(DEFAULT_MEMORY_SETTINGS);
    }
  });

  it('keeps valid fields and replaces garbage per field', () => {
    const parsed = parseMemorySettings({
      enabled: 'yes',
      captureServiceIds: ['chatgpt', 7, 'chatgpt', '', 'claude'],
      generation: { baseUrl: 'https://api.example.com/v1', apiKey: 12, model: 'gpt' },
      embedding: { source: { kind: 'remote' }, model: 'emb', dims: -3 },
      pythonCommand: null,
    });
    expect(parsed).toEqual({
      enabled: false,
      captureServiceIds: ['chatgpt', 'claude'],
      generation: { baseUrl: 'https://api.example.com/v1', apiKey: '', model: 'gpt' },
      embedding: { source: { kind: 'reuse-generation' }, model: 'emb', dims: 1536 },
      pythonCommand: '',
    });
  });

  it('keeps a separate embedding endpoint', () => {
    const parsed = parseMemorySettings({
      embedding: {
        source: { kind: 'separate', baseUrl: 'https://e.example.com', apiKey: 'k' },
        dims: 768,
      },
    });
    expect(parsed.embedding).toEqual({
      source: { kind: 'separate', baseUrl: 'https://e.example.com', apiKey: 'k' },
      model: DEFAULT_MEMORY_SETTINGS.embedding.model,
      dims: 768,
    });
  });
});

describe('captureTargets', () => {
  const services = [
    service('chatgpt', 'https://chatgpt.com'),
    service('claude', 'https://claude.ai'),
    service('grok', 'https://grok.com', false),
  ];
  const settings: MemorySettings = {
    ...DEFAULT_MEMORY_SETTINGS,
    enabled: true,
    captureServiceIds: ['grok', 'claude', 'chatgpt', 'gone'],
  };

  it('is opted in ∩ enabled, sorted', () => {
    expect(captureTargets(settings, services)).toEqual(['chatgpt', 'claude']);
  });

  it('is empty while memory is off', () => {
    expect(captureTargets({ ...settings, enabled: false }, services)).toEqual([]);
  });
});

describe('captureSupport', () => {
  it('matches host suffixes', () => {
    expect(captureSupport(service('a', 'https://chatgpt.com/c/1'), SUPPORTED)).toBe('supported');
    expect(captureSupport(service('b', 'https://www.claude.ai'), SUPPORTED)).toBe('supported');
    expect(captureSupport(service('c', 'https://notclaude.ai'), SUPPORTED)).toBe('unsupported');
    expect(captureSupport(service('d', 'https://grok.com'), SUPPORTED)).toBe('unsupported');
    expect(captureSupport(service('e', 'not a url'), SUPPORTED)).toBe('unsupported');
  });
});

describe('describeIngest', () => {
  const sentence = /^[^。]+。$/;

  it('says one sentence for every outcome', () => {
    const outcomes: IngestSummary[] = [
      { kind: 'skipped', reason: 'memory-off' },
      { kind: 'skipped', reason: 'needs-config' },
      { kind: 'skipped', reason: 'runtime-failed' },
      { kind: 'skipped', reason: 'no-targets' },
      { kind: 'coalesced' },
      ran({ batchesUnknown: 1 }),
      ran({ batchesRejected: 1 }),
      ran({ batchesStored: 2, memoriesProduced: 3 }),
      ran({ turnsDeferred: 1 }),
      ran({}),
    ];
    const sentences = outcomes.map(describeIngest);
    for (const text of sentences) expect(text).toMatch(sentence);
    expect(new Set(sentences).size).toBe(outcomes.length);
  });

  it('warns that retrying an unknown add may store a similar memory', () => {
    expect(describeIngest(ran({ batchesStored: 3, batchesUnknown: 2 }))).toMatch(
      /重试可能多出一条相似的记忆/
    );
  });

  it('reports what was produced', () => {
    expect(describeIngest(ran({ batchesStored: 2, memoriesProduced: 3 }))).toBe(
      '已记入 2 段对话，生成 3 条记忆。'
    );
  });
});

describe('applyPreset', () => {
  it('keeps typed keys and replaces endpoint identity', () => {
    const draft: MemorySettings = {
      ...DEFAULT_MEMORY_SETTINGS,
      generation: { baseUrl: 'https://old.example.com', apiKey: 'sk-mine', model: 'old' },
      embedding: {
        source: { kind: 'separate', baseUrl: 'https://e.example.com', apiKey: 'ek' },
        model: 'e',
        dims: 8,
      },
    };
    const deepseek = MODEL_PRESETS.find((p) => p.id === 'deepseek')!;
    const next = applyPreset(draft, deepseek);
    expect(next.generation).toEqual({ ...deepseek.generation, apiKey: 'sk-mine' });
    expect(next.embedding).toEqual({
      source: { kind: 'separate', baseUrl: deepseek.embedding.baseUrl, apiKey: 'ek' },
      model: deepseek.embedding.model,
      dims: deepseek.embedding.dims,
    });

    const openai = MODEL_PRESETS.find((p) => p.id === 'openai')!;
    expect(applyPreset(next, openai).embedding.source).toEqual({ kind: 'reuse-generation' });
  });
});

describe('formatRelativeTime', () => {
  it('uses minutes, hours, and days', () => {
    const now = 10 * 86_400_000;
    expect(formatRelativeTime(now - 10_000, now)).toBe('刚刚');
    expect(formatRelativeTime(now - 3 * 60_000, now)).toBe('3 分钟前');
    expect(formatRelativeTime(now - 2 * 3_600_000, now)).toBe('2 小时前');
    expect(formatRelativeTime(now - 4 * 86_400_000, now)).toBe('4 天前');
  });
});
