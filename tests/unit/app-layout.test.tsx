import { act, render, screen, waitFor } from '@testing-library/react';
import { vi } from 'vitest';
import { AppLayout } from '@/components/AppLayout';
import { DEFAULT_MEMORY_SETTINGS, type MemorySettings, type SettingsTab } from '@/types/memory';

const {
  mockActivateServiceContent,
  mockHideAllServiceContent,
  mockSyncServiceHostState,
  mockUsesDockedWindowContentHost,
  mockSyncMemoryRuntime,
} = vi.hoisted(() => ({
  mockSyncMemoryRuntime: vi.fn(() => Promise.resolve()),
  mockActivateServiceContent: vi.fn(() => Promise.resolve()),
  mockHideAllServiceContent: vi.fn(() => Promise.resolve()),
  mockSyncServiceHostState: vi.fn(() => Promise.resolve()),
  mockUsesDockedWindowContentHost: vi.fn(() => Promise.resolve(false)),
}));

vi.mock('@/hooks/useKeyboardShortcuts', () => ({
  useKeyboardShortcuts: () => undefined,
}));

vi.mock('@tauri-apps/api/window', () => ({
  getCurrentWindow: () => ({
    onResized: vi.fn(() => Promise.resolve(() => undefined)),
    onMoved: vi.fn(() => Promise.resolve(() => undefined)),
    onScaleChanged: vi.fn(() => Promise.resolve(() => undefined)),
    onFocusChanged: vi.fn(() => Promise.resolve(() => undefined)),
  }),
}));

vi.mock('@/services/content-host', () => ({
  activateServiceContent: mockActivateServiceContent,
  hideAllServiceContent: mockHideAllServiceContent,
  syncServiceHostState: mockSyncServiceHostState,
  syncDockedContentLayout: vi.fn(() => Promise.resolve()),
  usesDockedWindowContentHost: mockUsesDockedWindowContentHost,
}));

vi.mock('@/services/memory-host', () => ({
  syncMemoryRuntime: mockSyncMemoryRuntime,
}));

type StoreState = {
  activeServiceId: string | null;
  settingsPageOpen: boolean;
  settingsActiveTab: SettingsTab;
  addServiceDialogOpen: boolean;
  services: Array<{ id: string; name: string; url: string; enabled: boolean }>;
  memory: MemorySettings;
};

let storeState: StoreState = {
  activeServiceId: null,
  settingsPageOpen: false,
  settingsActiveTab: 'services',
  addServiceDialogOpen: false,
  services: [],
  memory: DEFAULT_MEMORY_SETTINGS,
};

vi.mock('@/stores/app-store', () => ({
  useAppStore: (selector?: (state: StoreState) => unknown) =>
    selector ? selector(storeState) : storeState,
}));

vi.mock('@/components/Sidebar', () => ({
  Sidebar: () => <div data-testid="sidebar" />,
}));

vi.mock('@/components/WebViewContainer', () => ({
  WebViewContainer: () => <div data-testid="webview" />,
}));

vi.mock('@/components/AddServiceDialog', () => ({
  AddServiceDialog: () => <div data-testid="add-service" />,
}));

vi.mock('@/components/SettingsPage', () => ({
  SettingsPage: () => <div data-testid="settings" />,
}));

describe('AppLayout', () => {
  beforeEach(() => {
    vi.clearAllMocks();
    mockUsesDockedWindowContentHost.mockResolvedValue(false);
    storeState = {
      activeServiceId: null,
      settingsPageOpen: false,
      settingsActiveTab: 'services',
      addServiceDialogOpen: false,
      services: [],
      memory: DEFAULT_MEMORY_SETTINGS,
    };
  });

  it('renders webview content when settings page is closed', async () => {
    await act(async () => {
      render(<AppLayout />);
    });

    expect(screen.getByTestId('webview')).toBeInTheDocument();
    expect(screen.queryByTestId('settings')).not.toBeInTheDocument();
  });

  it('renders settings page when settings page is open', async () => {
    storeState = {
      ...storeState,
      settingsPageOpen: true,
      settingsActiveTab: 'about',
    };

    await act(async () => {
      render(<AppLayout />);
    });

    expect(screen.getByTestId('settings')).toBeInTheDocument();
    expect(screen.queryByTestId('webview')).not.toBeInTheDocument();
  });

  it('switches to the active service when no dialog is open', async () => {
    storeState = {
      ...storeState,
      activeServiceId: 'chatgpt',
      services: [
        { id: 'chatgpt', name: 'ChatGPT', url: 'https://chatgpt.com', enabled: true },
      ],
    };

    render(<AppLayout />);

    await waitFor(() => {
      expect(mockActivateServiceContent).toHaveBeenCalledWith(
        { id: 'chatgpt', name: 'ChatGPT', url: 'https://chatgpt.com', enabled: true },
        [{ id: 'chatgpt', name: 'ChatGPT', url: 'https://chatgpt.com', enabled: true }]
      );
    });
  });

  it('hides all webviews while dialogs are open', async () => {
    storeState = {
      ...storeState,
      settingsPageOpen: true,
      activeServiceId: 'chatgpt',
      services: [
        { id: 'chatgpt', name: 'ChatGPT', url: 'https://chatgpt.com', enabled: true },
      ],
    };

    render(<AppLayout />);

    await waitFor(() => {
      expect(mockHideAllServiceContent).toHaveBeenCalled();
    });
  });

  it('syncs Rust host state when the Windows docked host is active and no service remains selected', async () => {
    mockUsesDockedWindowContentHost.mockResolvedValue(true);
    storeState = {
      ...storeState,
      services: [
        { id: 'chatgpt', name: 'ChatGPT', url: 'https://chatgpt.com', enabled: false },
        { id: 'gemini', name: 'Gemini', url: 'https://gemini.google.com', enabled: false },
      ],
    };

    render(<AppLayout />);

    await waitFor(() => {
      expect(mockSyncServiceHostState).toHaveBeenCalledWith(
        [
          { id: 'chatgpt', name: 'ChatGPT', url: 'https://chatgpt.com', enabled: false },
          { id: 'gemini', name: 'Gemini', url: 'https://gemini.google.com', enabled: false },
        ],
        null
      );
    });
  });

  it('suppresses the Windows docked host while the settings page is open', async () => {
    mockUsesDockedWindowContentHost.mockResolvedValue(true);
    storeState = {
      ...storeState,
      settingsPageOpen: true,
      activeServiceId: 'chatgpt',
      services: [
        { id: 'chatgpt', name: 'ChatGPT', url: 'https://chatgpt.com', enabled: true },
        { id: 'gemini', name: 'Gemini', url: 'https://gemini.google.com', enabled: true },
      ],
    };

    render(<AppLayout />);

    await waitFor(() => {
      expect(mockSyncServiceHostState).toHaveBeenCalledWith(
        [
          { id: 'chatgpt', name: 'ChatGPT', url: 'https://chatgpt.com', enabled: true },
          { id: 'gemini', name: 'Gemini', url: 'https://gemini.google.com', enabled: true },
        ],
        null
      );
    });
  });

  it('pushes memory settings to Rust without touching content hosting', async () => {
    const services = [
      { id: 'chatgpt', name: 'ChatGPT', url: 'https://chatgpt.com', enabled: true },
      { id: 'claude', name: 'Claude', url: 'https://claude.ai', enabled: false },
    ];
    const memory = {
      ...DEFAULT_MEMORY_SETTINGS,
      enabled: true,
      captureServiceIds: ['chatgpt', 'claude'],
    };
    storeState = {
      ...storeState,
      settingsPageOpen: true,
      settingsActiveTab: 'memory',
      services,
      memory,
    };

    render(<AppLayout />);

    await waitFor(() => {
      expect(mockSyncMemoryRuntime).toHaveBeenCalledWith(memory, services);
      expect(mockHideAllServiceContent).toHaveBeenCalled();
    });
    expect(mockActivateServiceContent).not.toHaveBeenCalled();
    expect(mockSyncServiceHostState).not.toHaveBeenCalled();
  });
});
