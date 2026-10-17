import { useEffect } from 'react';
import { captureTargets } from '@/lib/memory';
import { syncMemoryRuntime } from '@/services/memory-host';
import { useAppStore } from '@/stores/app-store';

/** Pushes memory settings and effective capture targets to Rust whenever either changes. */
export function useMemoryRuntime(): void {
  const memory = useAppStore((state) => state.memory);
  const services = useAppStore((state) => state.services);
  const targetsSignature = captureTargets(memory, services).join('|');

  useEffect(() => {
    syncMemoryRuntime(memory, services).catch(console.error);
    // `services` changes on every icon or order update; only the targets matter here.
  }, [memory, targetsSignature]);
}
