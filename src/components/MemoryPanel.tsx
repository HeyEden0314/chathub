import { useCallback, useEffect, useId, useRef, useState, type ReactNode } from 'react';
import { CircleDot, Eye, EyeOff, Loader2, RefreshCw } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { useAppStore } from '@/stores/app-store';
import { assertNever, cn } from '@/lib/utils';
import {
  MODEL_PRESETS,
  applyPreset,
  captureSupport,
  captureTargets,
  describeIngest,
  describeMemoryKind,
  describeProbe,
  describeRuntime,
  formatRelativeTime,
  type StatusTone,
} from '@/lib/memory';
import { getMemoryStatus, ingestMemories, listMemories } from '@/services/memory-host';
import type { ChatService } from '@/types';
import type {
  ConfigField,
  ConfigProblem,
  MemoryItem,
  MemoryListResult,
  MemorySettings,
  MemoryStatus,
  RuntimePhase,
} from '@/types/memory';

const STATUS_POLL_MS = 2000;
const PAGE_SIZE = 50;
const ROW = 'flex items-center gap-3 rounded-lg border p-3 border-border bg-card';

const TONE_CLASS: Record<StatusTone, string> = {
  muted: 'text-muted-foreground',
  busy: 'text-foreground',
  ok: 'text-primary',
  error: 'text-destructive',
};

type ListState =
  | { kind: 'loading' }
  | { kind: 'error'; message: string }
  | { kind: 'loaded'; result: MemoryListResult };

/** The part of the settings that 保存并应用 writes; enabling and capture apply at once. */
function modelFields(settings: MemorySettings): Partial<MemorySettings> {
  return {
    generation: settings.generation,
    embedding: settings.embedding,
    pythonCommand: settings.pythonCommand,
  };
}

function nonNegativeInteger(value: string): number {
  return Math.max(0, Math.trunc(Number(value) || 0));
}

export function MemoryPanel() {
  const saved = useAppStore((state) => state.memory);
  const services = useAppStore((state) => state.services);
  const updateMemorySettings = useAppStore((state) => state.updateMemorySettings);
  const setMemoryCapture = useAppStore((state) => state.setMemoryCapture);

  const [draft, setDraft] = useState<MemorySettings>(saved);
  const [status, setStatus] = useState<MemoryStatus | null>(null);
  const [syncing, setSyncing] = useState(false);
  const [filter, setFilter] = useState<string | undefined>();
  const [list, setList] = useState<ListState>({ kind: 'loading' });
  const filterRef = useRef(filter);
  const requestRef = useRef(0);

  const enabledServices = services.filter((s) => s.enabled).sort((a, b) => a.order - b.order);
  const targets = captureTargets(saved, services);
  const problems = status?.runtime.phase === 'needs-config' ? status.runtime.problems : [];
  const dirty = JSON.stringify(modelFields(draft)) !== JSON.stringify(modelFields(saved));

  const refreshStatus = useCallback(async () => {
    try {
      setStatus(await getMemoryStatus());
    } catch (error) {
      console.error(error);
    }
  }, []);

  const load = useCallback(async (serviceId: string | undefined, offset: number) => {
    const request = ++requestRef.current;
    try {
      const result = await listMemories({ serviceId, offset, limit: PAGE_SIZE });
      if (request !== requestRef.current) return;
      setList((previous) =>
        offset > 0 &&
        previous.kind === 'loaded' &&
        previous.result.kind === 'page' &&
        result.kind === 'page'
          ? {
              kind: 'loaded',
              result: { ...result, items: [...previous.result.items, ...result.items] },
            }
          : { kind: 'loaded', result }
      );
    } catch (error) {
      if (request !== requestRef.current) return;
      setList({ kind: 'error', message: String(error) });
    }
  }, []);

  const syncNow = useCallback(async () => {
    setSyncing(true);
    try {
      await ingestMemories();
    } catch (error) {
      console.error(error);
    } finally {
      setSyncing(false);
    }
    void refreshStatus();
    void load(filterRef.current, 0);
  }, [load, refreshStatus]);

  useEffect(() => {
    void refreshStatus();
    const timer = setInterval(() => void refreshStatus(), STATUS_POLL_MS);
    return () => clearInterval(timer);
  }, [refreshStatus]);

  useEffect(() => {
    void syncNow();
  }, [syncNow]);

  useEffect(() => {
    filterRef.current = filter;
    setList({ kind: 'loading' });
    void load(filter, 0);
  }, [filter, load]);

  return (
    <div className="flex-1 overflow-y-auto p-4 sm:p-8 max-w-3xl [&::-webkit-scrollbar]:hidden [-ms-overflow-style:'none'] [scrollbar-width:'none']">
      <div className="mb-6 flex flex-wrap items-start justify-between gap-4">
        <div>
          <h1 className="text-2xl font-bold">记忆</h1>
          <p className="mt-1 text-sm text-muted-foreground">
            把开启采集的服务中浏览过的对话，交给本机的 Hy-Memory 提炼成长期记忆。
          </p>
        </div>
        <Button
          variant={saved.enabled ? 'outline' : 'default'}
          onClick={() => updateMemorySettings({ enabled: !saved.enabled })}
        >
          {saved.enabled ? '关闭记忆' : '开启记忆'}
        </Button>
      </div>

      <StatusRow
        status={status}
        enabled={saved.enabled}
        syncing={syncing}
        onSync={() => void syncNow()}
      />

      <section className="mt-8 space-y-4">
        <h2 className="text-base font-semibold">生成模型</h2>
        <div className="flex flex-wrap gap-2">
          {MODEL_PRESETS.map((preset) => (
            <Button
              key={preset.id}
              variant="outline"
              size="sm"
              onClick={() => setDraft((current) => applyPreset(current, preset))}
            >
              {preset.label}
            </Button>
          ))}
        </div>
        <div className="grid gap-4 sm:grid-cols-2">
          <Field label="接口地址" field="generation.baseUrl" problems={problems}>
            {(props) => (
              <Input
                {...props}
                placeholder="https://api.openai.com/v1"
                value={draft.generation.baseUrl}
                onChange={(e) =>
                  setDraft({
                    ...draft,
                    generation: { ...draft.generation, baseUrl: e.target.value },
                  })
                }
              />
            )}
          </Field>
          <Field label="模型" field="generation.model" problems={problems}>
            {(props) => (
              <Input
                {...props}
                placeholder="gpt-4.1-mini"
                value={draft.generation.model}
                onChange={(e) =>
                  setDraft({ ...draft, generation: { ...draft.generation, model: e.target.value } })
                }
              />
            )}
          </Field>
          <Field
            label="API Key"
            field="generation.apiKey"
            problems={problems}
            className="sm:col-span-2"
          >
            {(props) => (
              <Input
                {...props}
                type="password"
                autoComplete="off"
                value={draft.generation.apiKey}
                onChange={(e) =>
                  setDraft({
                    ...draft,
                    generation: { ...draft.generation, apiKey: e.target.value },
                  })
                }
              />
            )}
          </Field>
        </div>
        <p className="text-xs text-muted-foreground">API Key 以明文保存在本机应用数据中。</p>

        <h3 className="pt-2 text-sm font-semibold">嵌入模型</h3>
        <div className="flex flex-wrap gap-2">
          <Button
            size="sm"
            variant={draft.embedding.source.kind === 'reuse-generation' ? 'secondary' : 'outline'}
            onClick={() =>
              setDraft({
                ...draft,
                embedding: { ...draft.embedding, source: { kind: 'reuse-generation' } },
              })
            }
          >
            复用生成接口
          </Button>
          <Button
            size="sm"
            variant={draft.embedding.source.kind === 'separate' ? 'secondary' : 'outline'}
            onClick={() =>
              draft.embedding.source.kind !== 'separate' &&
              setDraft({
                ...draft,
                embedding: {
                  ...draft.embedding,
                  source: { kind: 'separate', baseUrl: '', apiKey: '' },
                },
              })
            }
          >
            单独配置
          </Button>
        </div>
        <div className="grid gap-4 sm:grid-cols-2">
          {draft.embedding.source.kind === 'separate' && (
            <>
              <Field label="嵌入接口地址" field="embedding.baseUrl" problems={problems}>
                {(props) => (
                  <Input
                    {...props}
                    placeholder="https://api.openai.com/v1"
                    value={
                      draft.embedding.source.kind === 'separate'
                        ? draft.embedding.source.baseUrl
                        : ''
                    }
                    onChange={(e) =>
                      draft.embedding.source.kind === 'separate' &&
                      setDraft({
                        ...draft,
                        embedding: {
                          ...draft.embedding,
                          source: { ...draft.embedding.source, baseUrl: e.target.value },
                        },
                      })
                    }
                  />
                )}
              </Field>
              <Field label="嵌入 API Key" field="embedding.apiKey" problems={problems}>
                {(props) => (
                  <Input
                    {...props}
                    type="password"
                    autoComplete="off"
                    value={
                      draft.embedding.source.kind === 'separate'
                        ? draft.embedding.source.apiKey
                        : ''
                    }
                    onChange={(e) =>
                      draft.embedding.source.kind === 'separate' &&
                      setDraft({
                        ...draft,
                        embedding: {
                          ...draft.embedding,
                          source: { ...draft.embedding.source, apiKey: e.target.value },
                        },
                      })
                    }
                  />
                )}
              </Field>
            </>
          )}
          <Field label="嵌入模型" field="embedding.model" problems={problems}>
            {(props) => (
              <Input
                {...props}
                value={draft.embedding.model}
                onChange={(e) =>
                  setDraft({ ...draft, embedding: { ...draft.embedding, model: e.target.value } })
                }
              />
            )}
          </Field>
          <Field label="向量维度" field="embedding.dims" problems={problems}>
            {(props) => (
              <Input
                {...props}
                type="number"
                min={1}
                value={draft.embedding.dims}
                onChange={(e) =>
                  setDraft({
                    ...draft,
                    embedding: { ...draft.embedding, dims: nonNegativeInteger(e.target.value) },
                  })
                }
              />
            )}
          </Field>
          <Field
            label="Python 命令"
            field="pythonCommand"
            problems={problems}
            className="sm:col-span-2"
          >
            {(props) => (
              <Input
                {...props}
                placeholder="留空自动检测（python3 / python）"
                value={draft.pythonCommand}
                onChange={(e) => setDraft({ ...draft, pythonCommand: e.target.value })}
              />
            )}
          </Field>
        </div>
        <p className="text-xs text-muted-foreground">
          更换嵌入模型或维度会使用另一份记忆库；换回后原来的记忆仍在。
        </p>
        <div className="flex justify-end">
          <Button disabled={!dirty} onClick={() => updateMemorySettings(modelFields(draft))}>
            保存并应用
          </Button>
        </div>
      </section>

      <section className="mt-8 space-y-2">
        <h2 className="text-base font-semibold">采集服务</h2>
        <p className="text-sm text-muted-foreground">
          只读取已开启采集的服务当前打开的对话页，不保存聊天原文。
        </p>
        {enabledServices.map((service) => (
          <CaptureRow
            key={service.id}
            service={service}
            status={status}
            enabled={saved.enabled}
            capturing={saved.captureServiceIds.includes(service.id)}
            onToggle={(on) => setMemoryCapture(service.id, on)}
          />
        ))}
      </section>

      <section className="mt-8 space-y-3">
        <h2 className="text-base font-semibold">记忆列表</h2>
        <div className="flex flex-wrap gap-2">
          <Button
            size="sm"
            variant={filter === undefined ? 'secondary' : 'outline'}
            onClick={() => setFilter(undefined)}
          >
            全部
          </Button>
          {targets.map((id) => (
            <Button
              key={id}
              size="sm"
              variant={filter === id ? 'secondary' : 'outline'}
              onClick={() => setFilter(id)}
            >
              {services.find((s) => s.id === id)?.name ?? id}
            </Button>
          ))}
        </div>
        <MemoryList
          state={list}
          services={services}
          onMore={(offset) => void load(filter, offset)}
        />
      </section>
    </div>
  );
}

function StatusRow({
  status,
  enabled,
  syncing,
  onSync,
}: {
  status: MemoryStatus | null;
  enabled: boolean;
  syncing: boolean;
  onSync: () => void;
}) {
  const runtime = status
    ? describeRuntime(status.runtime)
    : { label: '正在读取状态…', tone: 'busy' as const };
  const running = status?.ingest.state === 'running';
  const lastRun = status?.ingest.lastRun;
  const abandoned = status?.ingest.abandonedTurns ?? 0;

  return (
    <div className={ROW}>
      {runtime.tone === 'busy' || running ? (
        <Loader2 className="h-4 w-4 flex-none animate-spin text-muted-foreground" />
      ) : (
        <CircleDot className={cn('h-4 w-4 flex-none', TONE_CLASS[runtime.tone])} />
      )}
      <div className="min-w-0 flex-1">
        <p className={cn('text-sm font-medium', TONE_CLASS[runtime.tone])}>
          {runtime.label}
          {running && ' · 生成中'}
        </p>
        {lastRun && <p className="text-xs text-muted-foreground">{describeIngest(lastRun)}</p>}
        {abandoned > 0 && (
          <p className="text-xs text-muted-foreground">{abandoned} 条消息多次记入失败，已跳过。</p>
        )}
      </div>
      <Button variant="outline" size="sm" disabled={!enabled || syncing} onClick={onSync}>
        <RefreshCw className={cn(syncing && 'animate-spin')} />
        立即同步
      </Button>
    </div>
  );
}

interface FieldInputProps {
  id: string;
  'aria-invalid': boolean;
  className?: string;
}

function Field({
  label,
  field,
  problems,
  className,
  children,
}: {
  label: string;
  field: ConfigField;
  problems: ConfigProblem[];
  className?: string;
  children: (props: FieldInputProps) => ReactNode;
}) {
  const id = useId();
  const messages = problems.filter((problem) => problem.field === field);
  const invalid = messages.length > 0;
  return (
    <div className={cn('space-y-1.5', className)}>
      <label htmlFor={id} className="text-sm font-medium">
        {label}
      </label>
      {children({
        id,
        'aria-invalid': invalid,
        className: invalid ? 'border-destructive' : undefined,
      })}
      {messages.map((problem) => (
        <p key={problem.message} className="text-xs text-destructive">
          {problem.message}
        </p>
      ))}
    </div>
  );
}

function CaptureRow({
  service,
  status,
  enabled,
  capturing,
  onToggle,
}: {
  service: ChatService;
  status: MemoryStatus | null;
  enabled: boolean;
  capturing: boolean;
  onToggle: (on: boolean) => void;
}) {
  const supported =
    status !== null && captureSupport(service, status.supportedHosts) === 'supported';
  const probe = status?.probes[service.id];
  let detail: string;
  if (!status) detail = '正在读取状态…';
  else if (!supported) detail = '暂不支持该网站';
  else if (!capturing) detail = '未开启采集';
  else if (probe) detail = describeProbe(probe);
  else detail = enabled ? '等待下次同步' : '记忆未开启';

  return (
    <div className={ROW}>
      <div className="flex h-8 w-8 flex-none items-center justify-center rounded-md bg-muted">
        <span className="text-xs font-medium">{service.name.charAt(0).toUpperCase()}</span>
      </div>
      <div className="min-w-0 flex-1">
        <p className="text-sm font-medium">{service.name}</p>
        <p className="truncate text-xs text-muted-foreground">{detail}</p>
      </div>
      <Button
        variant="ghost"
        size="icon"
        className="h-8 w-8"
        aria-label={capturing ? `停止采集 ${service.name}` : `采集 ${service.name}`}
        aria-pressed={capturing}
        disabled={!capturing && !supported}
        onClick={() => onToggle(!capturing)}
      >
        {capturing ? <Eye className="h-4 w-4" /> : <EyeOff className="h-4 w-4" />}
      </Button>
    </div>
  );
}

function RuntimeNotice({ runtime }: { runtime: RuntimePhase }) {
  const text =
    runtime.phase === 'off'
      ? '记忆未开启。开启后，这里会显示提炼出的记忆。'
      : `记忆服务暂不可用：${describeRuntime(runtime).label}`;
  return (
    <p className="rounded-lg border border-border bg-muted/30 p-3 text-sm text-muted-foreground">
      {text}
    </p>
  );
}

function MemoryList({
  state,
  services,
  onMore,
}: {
  state: ListState;
  services: ChatService[];
  onMore: (offset: number) => void;
}) {
  switch (state.kind) {
    case 'loading':
      return <p className="text-sm text-muted-foreground">正在加载…</p>;
    case 'error':
      return <p className="text-sm text-destructive">加载失败：{state.message}</p>;
    case 'loaded':
      switch (state.result.kind) {
        case 'unavailable':
          return <RuntimeNotice runtime={state.result.runtime} />;
        case 'page':
          return <MemoryPage result={state.result} services={services} onMore={onMore} />;
        default:
          return assertNever(state.result);
      }
    default:
      return assertNever(state);
  }
}

function MemoryPage({
  result,
  services,
  onMore,
}: {
  result: Extract<MemoryListResult, { kind: 'page' }>;
  services: ChatService[];
  onMore: (offset: number) => void;
}) {
  if (result.items.length === 0) {
    return (
      <p className="text-sm text-muted-foreground">
        还没有记忆。开启采集并在服务中对话后，这里会出现提炼出的记忆。
      </p>
    );
  }
  const now = Date.now();
  return (
    <div className="space-y-2">
      <p className="text-xs text-muted-foreground">共 {result.total} 条</p>
      {result.items.map((item) => (
        <MemoryRow key={item.id} item={item} services={services} now={now} />
      ))}
      {result.dropped > 0 && (
        <p className="text-xs text-muted-foreground">
          有 {result.dropped} 条记录无法识别，未显示。
        </p>
      )}
      {result.nextOffset !== null && (
        <Button variant="outline" className="w-full" onClick={() => onMore(result.nextOffset ?? 0)}>
          加载更多
        </Button>
      )}
    </div>
  );
}

function MemoryRow({
  item,
  services,
  now,
}: {
  item: MemoryItem;
  services: ChatService[];
  now: number;
}) {
  const source = item.serviceId
    ? (services.find((s) => s.id === item.serviceId)?.name ?? item.serviceId)
    : '全部服务';
  return (
    <div className={cn(ROW, 'items-start')}>
      <span className="flex-none rounded-md bg-muted px-1.5 py-0.5 text-xs font-medium">
        {describeMemoryKind(item.kind)}
      </span>
      <div className="min-w-0 flex-1">
        <p className="text-sm leading-6">{item.text}</p>
        <p className="text-xs text-muted-foreground">
          {source}
          {item.createdAt !== null && ` · ${formatRelativeTime(item.createdAt, now)}`}
        </p>
      </div>
    </div>
  );
}
