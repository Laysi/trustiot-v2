// Cleanup that must also happen when a command is interrupted (Ctrl-C, SIGTERM): removing the
// temporary secret files, build directories and the instance lock. The handlers are synchronous
// file operations, so they finish before the process exits.

const handlers = new Set<() => void>();

/** Registers `cleanup` for an interrupt; call the returned function once the cleanup is done. */
export function registerCleanup(cleanup: () => void): () => void {
  handlers.add(cleanup);
  return () => {
    handlers.delete(cleanup);
  };
}

/** Runs and forgets every registered cleanup; a failing one does not stop the others. */
export function runCleanups(): void {
  const pending = [...handlers];
  handlers.clear();
  for (const cleanup of pending) {
    try {
      cleanup();
    } catch {
      // best effort: the process is leaving anyway
    }
  }
}
