import { create } from 'zustand';
import { persist } from 'zustand/middleware';
import { ChatService, DEFAULT_SERVICES } from '@/types';
import { DEFAULT_MEMORY_SETTINGS, type MemorySettings, type SettingsTab } from '@/types/memory';
import { parseMemorySettings } from '@/lib/memory';

interface AppState {
  services: ChatService[];
  activeServiceId: string | null;
  settingsPageOpen: boolean;
  settingsActiveTab: SettingsTab;
  addServiceDialogOpen: boolean;
  memory: MemorySettings;

  setActiveService: (id: string) => void;
  addService: (service: Omit<ChatService, 'id' | 'order'>) => void;
  removeService: (id: string) => void;
  updateService: (id: string, updates: Partial<ChatService>) => void;
  toggleServiceEnabled: (id: string) => void;
  reorderServices: (startIndex: number, endIndex: number) => void;
  setSettingsPageOpen: (open: boolean) => void;
  setSettingsActiveTab: (tab: SettingsTab) => void;
  setAddServiceDialogOpen: (open: boolean) => void;
  /** Top-level shallow merge; callers pass whole nested objects. */
  updateMemorySettings: (patch: Partial<MemorySettings>) => void;
  setMemoryCapture: (serviceId: string, on: boolean) => void;
}

export const useAppStore = create<AppState>()(
  persist(
    (set, get) => ({
      services: DEFAULT_SERVICES,
      activeServiceId: DEFAULT_SERVICES.find((s) => s.enabled)?.id ?? null,
      settingsPageOpen: false,
      settingsActiveTab: 'services',
      addServiceDialogOpen: false,
      memory: DEFAULT_MEMORY_SETTINGS,

      setActiveService: (id) => {
        const service = get().services.find((s) => s.id === id);
        if (service?.enabled) {
          set({ activeServiceId: id });
        }
      },

      addService: (serviceData) => {
        const services = get().services;
        const id = `custom-${Date.now()}`;
        const order = Math.max(...services.map((s) => s.order), -1) + 1;
        const newService: ChatService = {
          ...serviceData,
          id,
          order,
          enabled: true,
        };
        set({ services: [...services, newService] });
      },

      removeService: (id) => {
        const services = get().services.filter((s) => s.id !== id);
        const activeId = get().activeServiceId;
        const memory = get().memory;
        set({
          services,
          activeServiceId:
            activeId === id ? (services.find((s) => s.enabled)?.id ?? null) : activeId,
          memory: memory.captureServiceIds.includes(id)
            ? { ...memory, captureServiceIds: memory.captureServiceIds.filter((sid) => sid !== id) }
            : memory,
        });
      },

      updateService: (id, updates) => {
        set({
          services: get().services.map((s) => (s.id === id ? { ...s, ...updates } : s)),
        });
      },

      toggleServiceEnabled: (id) => {
        const services = get().services.map((s) =>
          s.id === id ? { ...s, enabled: !s.enabled } : s
        );
        const activeId = get().activeServiceId;
        const targetService = services.find((s) => s.id === id);

        let newActiveId = activeId;
        if (activeId === id && !targetService?.enabled) {
          newActiveId = services.find((s) => s.enabled)?.id ?? null;
        }

        set({ services, activeServiceId: newActiveId });
      },

      reorderServices: (startIndex, endIndex) => {
        const services = [...get().services];
        const [removed] = services.splice(startIndex, 1);
        services.splice(endIndex, 0, removed);
        set({
          services: services.map((s, i) => ({ ...s, order: i })),
        });
      },

      setSettingsPageOpen: (open) => set({ settingsPageOpen: open }),
      setSettingsActiveTab: (tab) => set({ settingsActiveTab: tab }),
      setAddServiceDialogOpen: (open) => set({ addServiceDialogOpen: open }),

      updateMemorySettings: (patch) => set({ memory: { ...get().memory, ...patch } }),

      setMemoryCapture: (serviceId, on) => {
        const memory = get().memory;
        const ids = memory.captureServiceIds.filter((id) => id !== serviceId);
        set({ memory: { ...memory, captureServiceIds: on ? [...ids, serviceId] : ids } });
      },
    }),
    {
      name: 'chat-box-app-storage',
      partialize: (state) => ({
        services: state.services,
        activeServiceId: state.activeServiceId,
        memory: state.memory,
      }),
      merge: (persistedState, currentState) => {
        const persisted = persistedState as Partial<AppState>;
        const persistedServices = persisted.services ?? [];

        const mergedServices = DEFAULT_SERVICES.map((defaultService) => {
          const saved = persistedServices.find((s) => s.id === defaultService.id);
          if (saved) {
            return {
              ...defaultService,
              iconUrl: saved.iconUrl ?? defaultService.iconUrl,
              enabled: saved.enabled,
              order: saved.order,
            };
          }
          return defaultService;
        });

        const customServices = persistedServices.filter((s) => s.id.startsWith('custom-'));
        const allServices = [...mergedServices, ...customServices];

        return {
          ...currentState,
          ...persisted,
          services: allServices,
          memory: parseMemorySettings(persisted.memory),
        };
      },
    }
  )
);
