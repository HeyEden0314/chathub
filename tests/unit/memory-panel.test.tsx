import { act, cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { vi } from 'vitest';
import { MemoryPanel } from '@/components/MemoryPanel';
import { describeRuntime } from '@/lib/memory';
import { MEMORY_CONTRACT } from '@/types/memory-contract.generated';
import {
  DEFAULT_MEMORY_SETTINGS,
  type MemoryListResult,
  type MemorySettings,
  type MemoryStatus,
} from '@/types/memory';

const { mockGetMemoryStatus, mockIngestMemories, mockListMemories } = vi.hoisted(() => ({
  mockGetMemoryStatus: vi.fn(),
  mockIngestMemories: vi.fn(),
  mockListMemories: vi.fn(),
}));

vi.mock('@/services/memory-host', () => ({
  getMemoryStatus: mockGetMemoryStatus,
  ingestMemories: mockIngestMemories,
  listMemories: mockListMemories,
}));

type StoreState = {
  memory: MemorySettings;
  services: Array<{ id: string; name: string; url: string; enabled: boolean; order: number }>;
  updateMemorySettings: ReturnType<typeof vi.fn>;
  setMemoryCapture: ReturnType<typeof vi.fn>;
};

let storeState: StoreState;

vi.mock('@/stores/app-store', () => ({
  useAppStore: (selector: (state: StoreState) => unknown) => selector(storeState),
}));

const statuses = MEMORY_CONTRACT.statuses as MemoryStatus[];
const statusWith = (phase: MemoryStatus['runtime']['phase']) =>
  statuses.find((s) => s.runtime.phase === phase)!;
const emptyPage: MemoryListResult = {
  kind: 'page',
  items: [],
  total: 0,
  nextOffset: null,
  dropped: 0,
};

async function renderPanel() {
  await act(async () => {
    render(<MemoryPanel />);
  });
}

describe('MemoryPanel', () => {
  beforeEach(() => {
    vi.clearAllMocks();
    storeState = {
      memory: { ...DEFAULT_MEMORY_SETTINGS, enabled: true, captureServiceIds: ['chatgpt'] },
      services: [
        { id: 'chatgpt', name: 'ChatGPT', url: 'https://chatgpt.com', enabled: true, order: 0 },
        { id: 'grok', name: 'Grok', url: 'https://grok.com', enabled: true, order: 1 },
        { id: 'poe', name: 'Poe', url: 'https://poe.com', enabled: false, order: 2 },
      ],
      updateMemorySettings: vi.fn(),
      setMemoryCapture: vi.fn(),
    };
    mockGetMemoryStatus.mockResolvedValue(statusWith('idle'));
    mockIngestMemories.mockResolvedValue({ kind: 'coalesced' });
    mockListMemories.mockResolvedValue(emptyPage);
  });

  it('renders a label for every runtime phase', async () => {
    for (const status of statuses) {
      mockGetMemoryStatus.mockResolvedValue(status);
      await renderPanel();
      const { label } = describeRuntime(status.runtime);
      expect(
        await screen.findByText(
          (_, el) => el?.tagName === 'P' && !!el.textContent?.startsWith(label)
        )
      ).toBeInTheDocument();
      cleanup();
    }
  });

  it('ingests once on open and lists the first page', async () => {
    await renderPanel();
    expect(mockIngestMemories).toHaveBeenCalledTimes(1);
    expect(mockListMemories).toHaveBeenCalledWith({ serviceId: undefined, offset: 0, limit: 50 });
  });

  it('writes the store once on save, with the model fields only', async () => {
    await renderPanel();
    const save = screen.getByRole('button', { name: '保存并应用' });
    expect(save).toBeDisabled();

    fireEvent.change(screen.getByLabelText('模型'), { target: { value: 'gpt-4.1-mini' } });
    fireEvent.change(screen.getByLabelText('API Key'), { target: { value: 'sk-test' } });
    fireEvent.click(save);

    expect(storeState.updateMemorySettings).toHaveBeenCalledTimes(1);
    expect(storeState.updateMemorySettings).toHaveBeenCalledWith({
      generation: { baseUrl: '', apiKey: 'sk-test', model: 'gpt-4.1-mini' },
      embedding: DEFAULT_MEMORY_SETTINGS.embedding,
      pythonCommand: '',
    });
  });

  it('tells the user the key is stored in plaintext', async () => {
    await renderPanel();
    expect(screen.getByText('API Key 以明文保存在本机应用数据中。')).toBeInTheDocument();
  });

  it('shows config problems next to their fields', async () => {
    mockGetMemoryStatus.mockResolvedValue(statusWith('needs-config'));
    await renderPanel();
    expect(await screen.findByText('请填写生成模型名称')).toBeInTheDocument();
    expect(screen.getByLabelText('模型')).toHaveAttribute('aria-invalid', 'true');
  });

  it('shows the runtime notice when the list is unavailable', async () => {
    const failed = statuses.find((s) => s.runtime.phase === 'failed')!;
    mockListMemories.mockResolvedValue({ kind: 'unavailable', runtime: failed.runtime });
    await renderPanel();
    expect(
      await screen.findByText(`记忆服务暂不可用：${describeRuntime(failed.runtime).label}`)
    ).toBeInTheDocument();
  });

  it('renders memories and appends the next page', async () => {
    const page = MEMORY_CONTRACT.listResults.find((r) => r.kind === 'page') as Extract<
      MemoryListResult,
      { kind: 'page' }
    >;
    mockListMemories.mockResolvedValue({ ...page, nextOffset: 1 });
    await renderPanel();
    expect(await screen.findByText(page.items[0].text)).toBeInTheDocument();
    expect(screen.getByText('事实')).toBeInTheDocument();

    mockListMemories.mockResolvedValueOnce({
      kind: 'page',
      items: [{ ...page.items[0], id: 'next', text: '用户在学吉他。' }],
      total: 3,
      nextOffset: null,
      dropped: 0,
    });
    fireEvent.click(screen.getByRole('button', { name: '加载更多' }));
    expect(await screen.findByText('用户在学吉他。')).toBeInTheDocument();
    expect(screen.getByText(page.items[0].text)).toBeInTheDocument();
    expect(mockListMemories).toHaveBeenLastCalledWith({
      serviceId: undefined,
      offset: 1,
      limit: 50,
    });
    expect(screen.queryByRole('button', { name: '加载更多' })).not.toBeInTheDocument();
  });

  it('toggles capture for supported services only', async () => {
    await renderPanel();
    await waitFor(() =>
      expect(screen.getByRole('button', { name: '停止采集 ChatGPT' })).toBeEnabled()
    );
    fireEvent.click(screen.getByRole('button', { name: '停止采集 ChatGPT' }));
    expect(storeState.setMemoryCapture).toHaveBeenCalledWith('chatgpt', false);

    expect(screen.getByRole('button', { name: '采集 Grok' })).toBeDisabled();
    expect(screen.queryByText('Poe')).not.toBeInTheDocument();
  });
});
