import { useCallback, useEffect, useRef, useState } from "react";

import type { DocumentApi, DocumentSummary, UploadRequest } from "../../api/pending/documents.ts";
import { isPdf } from "./format.ts";

/**
 * One file of the upload dialog. A file the library cannot take is `rejected` at once, with the
 * reason; the others wait `staged` until the user starts them, then are `uploading`, and end
 * `done` or `failed` (with the error and a retry).
 */
export interface UploadItem {
  id: number;
  file: File;
  state: "staged" | "rejected" | "uploading" | "done" | "failed";
  /** The share of the file sent, 0 to 1. */
  fraction: number;
  rejection?: "not_pdf" | "over_capacity";
  /** The capacity left when an over-capacity file was refused. */
  remainingBytes?: number;
  /** Where it goes, once started; kept for a retry. */
  request?: UploadRequest;
  /** What a failed upload threw. */
  error?: unknown;
}

export interface UploadQueue {
  items: readonly UploadItem[];
  /** Adds files: a non-PDF or what does not fit in `remainingBytes` is rejected, the rest staged. */
  stage(files: readonly File[], remainingBytes: number): void;
  start(id: number, request: UploadRequest): void;
  retry(id: number): void;
  /** Takes a file out of the queue; one that is uploading is cancelled first. */
  remove(id: number): void;
  /** Drops the items in these states (closing the dialog drops the ones that need no more attention). */
  clear(states: readonly UploadItem["state"][]): void;
  /** Bytes the files that are staged or uploading will take. */
  committedBytes: number;
}

/**
 * The files being uploaded on the library page. It lives on the page, not in the dialog, so
 * closing the dialog ("continue in the background") leaves the uploads running; leaving the page
 * cancels them, since nothing is left to carry them on. The page is mounted under `TenantGate`,
 * which remounts everything below it when the active tenant changes, so a queue never outlives
 * its tenant: the unmount cleanup aborts what is uploading and the new tenant starts empty.
 */
export function useUploadQueue(api: DocumentApi, onStored: (document: DocumentSummary) => void): UploadQueue {
  const [items, setItems] = useState<readonly UploadItem[]>([]);
  const controllers = useRef(new Map<number, AbortController>());
  const next = useRef(1);
  const latest = useRef({ api, onStored });
  latest.current = { api, onStored };

  const patch = useCallback((id: number, change: Partial<UploadItem>) => {
    setItems((current) => current.map((item) => (item.id === id ? { ...item, ...change } : item)));
  }, []);

  const run = useCallback(
    (item: UploadItem, request: UploadRequest) => {
      const controller = new AbortController();
      controllers.current.set(item.id, controller);
      patch(item.id, { state: "uploading", fraction: 0, request, error: undefined });
      latest.current.api
        .upload(request, item.file, {
          signal: controller.signal,
          onProgress: (fraction) => {
            if (!controller.signal.aborted) patch(item.id, { fraction });
          },
        })
        .then(
          (document) => {
            controllers.current.delete(item.id);
            if (controller.signal.aborted) return;
            patch(item.id, { state: "done", fraction: 1 });
            latest.current.onStored(document);
          },
          (error: unknown) => {
            controllers.current.delete(item.id);
            if (controller.signal.aborted) return;
            patch(item.id, { state: "failed", error });
          },
        );
    },
    [patch],
  );

  useEffect(() => {
    const running = controllers.current;
    return () => {
      for (const controller of running.values()) controller.abort();
      running.clear();
    };
  }, []);

  const stage = useCallback((files: readonly File[], remainingBytes: number) => {
    setItems((current) => {
      let remaining = remainingBytes - current.reduce((sum, item) => sum + (isCommitted(item) ? item.file.size : 0), 0);
      const added: UploadItem[] = files.map((file) => {
        const id = next.current++;
        if (!isPdf(file)) return { id, file, state: "rejected", fraction: 0, rejection: "not_pdf" };
        if (file.size > remaining) {
          return {
            id,
            file,
            state: "rejected",
            fraction: 0,
            rejection: "over_capacity",
            remainingBytes: Math.max(0, remaining),
          };
        }
        remaining -= file.size;
        return { id, file, state: "staged", fraction: 0 };
      });
      return [...current, ...added];
    });
  }, []);

  const start = useCallback(
    (id: number, request: UploadRequest) => {
      const item = items.find((candidate) => candidate.id === id);
      if (item === undefined || item.state !== "staged") return;
      run(item, request);
    },
    [items, run],
  );

  const retry = useCallback(
    (id: number) => {
      const item = items.find((candidate) => candidate.id === id);
      if (item === undefined || item.state !== "failed" || item.request === undefined) return;
      run(item, item.request);
    },
    [items, run],
  );

  const remove = useCallback((id: number) => {
    controllers.current.get(id)?.abort();
    controllers.current.delete(id);
    setItems((current) => current.filter((item) => item.id !== id));
  }, []);

  const clear = useCallback((states: readonly UploadItem["state"][]) => {
    setItems((current) => current.filter((item) => !states.includes(item.state)));
  }, []);

  return {
    items,
    stage,
    start,
    retry,
    remove,
    clear,
    committedBytes: items.reduce((sum, item) => sum + (isCommitted(item) ? item.file.size : 0), 0),
  };
}

function isCommitted(item: UploadItem): boolean {
  return item.state === "staged" || item.state === "uploading";
}
