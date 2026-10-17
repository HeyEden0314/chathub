import { assertNever } from '@/lib/utils';
import type { ChatService } from '@/types';
import {
  DEFAULT_MEMORY_SETTINGS,
  type EmbeddingSettings,
  type EmbeddingSource,
  type IngestSkipReason,
  type IngestSummary,
  type MemoryKind,
  type MemorySettings,
  type ModelEndpoint,
  type ProbeHealth,
  type RuntimePhase,
  type SidecarFailure,
} from '@/types/memory';

type UnknownRecord = Record<string, unknown>;

function asRecord(value: unknown): UnknownRecord | null {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
    ? (value as UnknownRecord)
    : null;
}

function stringOr(value: unknown, fallback: string): string {
  return typeof value === 'string' ? value : fallback;
}

function parseEndpoint(raw: unknown): ModelEndpoint {
  const record = asRecord(raw);
  const fallback = DEFAULT_MEMORY_SETTINGS.generation;
  return {
    baseUrl: stringOr(record?.baseUrl, fallback.baseUrl),
    apiKey: stringOr(record?.apiKey, fallback.apiKey),
    model: stringOr(record?.model, fallback.model),
  };
}

function parseEmbedding(raw: unknown): EmbeddingSettings {
  const record = asRecord(raw);
  const fallback = DEFAULT_MEMORY_SETTINGS.embedding;
  const source = asRecord(record?.source);
  const dims = record?.dims;
  return {
    source:
      source?.kind === 'separate'
        ? {
            kind: 'separate',
            baseUrl: stringOr(source.baseUrl, ''),
            apiKey: stringOr(source.apiKey, ''),
          }
        : { kind: 'reuse-generation' },
    model: stringOr(record?.model, fallback.model),
    dims: typeof dims === 'number' && Number.isInteger(dims) && dims > 0 ? dims : fallback.dims,
  };
}

/** localStorage is a boundary: fills defaults per field, drops garbage, never throws. */
export function parseMemorySettings(raw: unknown): MemorySettings {
  const record = asRecord(raw);
  const ids = Array.isArray(record?.captureServiceIds) ? record.captureServiceIds : [];
  return {
    enabled: record?.enabled === true,
    captureServiceIds: [
      ...new Set(ids.filter((id): id is string => typeof id === 'string' && id !== '')),
    ],
    generation: parseEndpoint(record?.generation),
    embedding: parseEmbedding(record?.embedding),
    pythonCommand: stringOr(record?.pythonCommand, DEFAULT_MEMORY_SETTINGS.pythonCommand),
  };
}

/** Opted in ∩ currently enabled, sorted. Empty when memory is off. */
export function captureTargets(settings: MemorySettings, services: ChatService[]): string[] {
  if (!settings.enabled) return [];
  const enabled = new Set(services.filter((s) => s.enabled).map((s) => s.id));
  return settings.captureServiceIds.filter((id) => enabled.has(id)).sort();
}

export type CaptureSupport = 'supported' | 'unsupported';

export function captureSupport(
  service: ChatService,
  supportedHosts: readonly string[]
): CaptureSupport {
  let host: string;
  try {
    host = new URL(service.url).hostname.toLowerCase();
  } catch {
    return 'unsupported';
  }
  return supportedHosts.some((h) => host === h || host.endsWith(`.${h}`))
    ? 'supported'
    : 'unsupported';
}

export type StatusTone = 'muted' | 'busy' | 'ok' | 'error';

export function describeRuntime(runtime: RuntimePhase): { label: string; tone: StatusTone } {
  switch (runtime.phase) {
    case 'off':
      return { label: '未开启', tone: 'muted' };
    case 'needs-config':
      return { label: '需要完成配置', tone: 'error' };
    case 'idle':
      return { label: '待命：有新对话时自动启动', tone: 'muted' };
    case 'starting':
      return { label: '启动中', tone: 'busy' };
    case 'ready':
      return { label: `运行中 · ${runtime.applied.model}`, tone: 'ok' };
    case 'backoff':
      return { label: `已退出，第 ${runtime.attempt} 次重启等待中`, tone: 'busy' };
    case 'failed':
      return { label: describeFailure(runtime.failure), tone: 'error' };
    default:
      return assertNever(runtime);
  }
}

export function describeFailure(failure: SidecarFailure): string {
  switch (failure.kind) {
    case 'python-not-found':
      return '未找到 Python';
    case 'module-missing':
      return `未安装 Hy-Memory（${failure.installHint}）`;
    case 'exited-on-start':
      return '记忆服务启动后立即退出';
    case 'health-timeout':
      return '记忆服务启动超时';
    case 'model-check-failed':
      return `模型检查失败：${failure.message}`;
    case 'crash-loop':
      return '记忆服务反复崩溃，已停止重启';
    default:
      return assertNever(failure);
  }
}

export function describeProbe(health: ProbeHealth): string {
  switch (health.kind) {
    case 'not-open':
      return '未打开（打开该服务后开始采集）';
    case 'unsupported-site':
      return '暂不支持该网站';
    case 'no-conversation':
      return '当前不在对话页';
    case 'unrecognized-page':
      return '未识别到对话内容（页面可能已改版）';
    case 'ok':
      return `已读取 ${health.turns} 条`;
    case 'error':
      return `读取失败：${health.message}`;
    default:
      return assertNever(health);
  }
}

function describeSkip(reason: IngestSkipReason): string {
  switch (reason) {
    case 'memory-off':
      return '记忆未开启，这次没有记入。';
    case 'needs-config':
      return '还没有完成模型配置，填写接口、模型和 API Key 后才会记入对话。';
    case 'runtime-failed':
      return '记忆服务没有启动，这次没有记入。';
    case 'no-targets':
      return '没有开启采集的服务，这次没有记入。';
    default:
      return assertNever(reason);
  }
}

/** One sentence per outcome. A `ran` summary reports its most important part. */
export function describeIngest(summary: IngestSummary): string {
  switch (summary.kind) {
    case 'skipped':
      return describeSkip(summary.reason);
    case 'coalesced':
      return '上一轮同步还在进行，这次请求已并入其中。';
    case 'ran':
      if (summary.batchesUnknown > 0) {
        return `有 ${summary.batchesUnknown} 段对话没有确认是否记入，下次同步会重试；如果其实已经写入，重试可能多出一条相似的记忆。`;
      }
      if (summary.batchesRejected > 0) {
        return `有 ${summary.batchesRejected} 段对话被记忆服务拒绝，会再试，连续 3 次失败后放弃。`;
      }
      if (summary.batchesStored > 0) {
        return `已记入 ${summary.batchesStored} 段对话，生成 ${summary.memoriesProduced} 条记忆。`;
      }
      if (summary.turnsDeferred > 0) {
        return `有 ${summary.turnsDeferred} 条消息还没结束，稍后再记入。`;
      }
      return '没有新的对话需要记入。';
    default:
      return assertNever(summary);
  }
}

export function describeMemoryKind(kind: MemoryKind): string {
  switch (kind) {
    case 'profile':
      return '画像';
    case 'fact':
      return '事实';
    case 'intention':
      return '计划';
    case 'schema':
      return '规律';
    case 'summary':
      return '摘要';
    case 'other':
      return '其他';
    default:
      return assertNever(kind);
  }
}

export function formatRelativeTime(at: number, now: number): string {
  const minutes = Math.floor((now - at) / 60_000);
  if (minutes < 1) return '刚刚';
  if (minutes < 60) return `${minutes} 分钟前`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours} 小时前`;
  const days = Math.floor(hours / 24);
  if (days < 30) return `${days} 天前`;
  return new Date(at).toLocaleDateString('zh-CN');
}

/** UI convenience only. Model names are examples; verify them against each provider. */
export interface ModelPreset {
  id: 'openai' | 'deepseek' | 'hunyuan' | 'ollama' | 'custom';
  label: string;
  generation: Pick<ModelEndpoint, 'baseUrl' | 'model'>;
  embedding: { source: EmbeddingSource['kind']; model: string; dims: number; baseUrl?: string };
}

export const MODEL_PRESETS: readonly ModelPreset[] = [
  {
    id: 'openai',
    label: 'OpenAI',
    generation: { baseUrl: 'https://api.openai.com/v1', model: 'gpt-4.1-mini' },
    embedding: { source: 'reuse-generation', model: 'text-embedding-3-small', dims: 1536 },
  },
  {
    id: 'deepseek',
    label: 'DeepSeek',
    generation: { baseUrl: 'https://api.deepseek.com/v1', model: 'deepseek-chat' },
    embedding: {
      source: 'separate',
      baseUrl: 'https://api.openai.com/v1',
      model: 'text-embedding-3-small',
      dims: 1536,
    },
  },
  {
    id: 'hunyuan',
    label: '腾讯混元',
    generation: {
      baseUrl: 'https://api.hunyuan.cloud.tencent.com/v1',
      model: 'hunyuan-turbos-latest',
    },
    embedding: { source: 'reuse-generation', model: 'hunyuan-embedding', dims: 1024 },
  },
  {
    id: 'ollama',
    label: 'Ollama（本地）',
    generation: { baseUrl: 'http://127.0.0.1:11434/v1', model: 'qwen2.5:7b' },
    embedding: { source: 'reuse-generation', model: 'nomic-embed-text', dims: 768 },
  },
  {
    id: 'custom',
    label: '自定义',
    generation: { baseUrl: '', model: '' },
    embedding: {
      source: 'reuse-generation',
      model: DEFAULT_MEMORY_SETTINGS.embedding.model,
      dims: DEFAULT_MEMORY_SETTINGS.embedding.dims,
    },
  },
];

/** Keeps any API key already typed; replaces endpoints, models, and embedding identity. */
export function applyPreset(draft: MemorySettings, preset: ModelPreset): MemorySettings {
  const separateKey =
    draft.embedding.source.kind === 'separate' ? draft.embedding.source.apiKey : '';
  return {
    ...draft,
    generation: { ...draft.generation, ...preset.generation },
    embedding: {
      source:
        preset.embedding.source === 'separate'
          ? { kind: 'separate', baseUrl: preset.embedding.baseUrl ?? '', apiKey: separateKey }
          : { kind: 'reuse-generation' },
      model: preset.embedding.model,
      dims: preset.embedding.dims,
    },
  };
}
