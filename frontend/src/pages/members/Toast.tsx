import { CircleCheckIcon, XIcon } from "lucide-react";
import { useEffect, useRef, useState } from "react";

import { Button } from "../../components/ui/button.tsx";

/** How long a notice stays: long enough to read, and paused while the pointer or focus is on it. */
export const TOAST_MS = 5000;

/**
 * A success notice (design.md 「2. 重用與新增的元件」, Toast): a `role="status"` region, gone after
 * five seconds unless the pointer is over it or focus is in it. The region is always in the page,
 * so a screen reader announces the text when it appears.
 */
export function Toast({
  notice,
  onDismiss,
  dismissLabel,
  durationMs = TOAST_MS,
}: {
  notice: { id: number; title: string; body: string } | null;
  onDismiss(): void;
  dismissLabel: string;
  /** Only a test shortens it. */
  durationMs?: number;
}) {
  const [paused, setPaused] = useState(false);
  const dismiss = useRef(onDismiss);
  dismiss.current = onDismiss;
  const id = notice?.id;
  const element = useRef<HTMLElement>(null);
  // The pointer over the notice, or focus inside it, pauses the timer. Plain listeners: the
  // notice is not a control, so it gets no handler props (and no role to excuse them).
  useEffect(() => {
    const target = element.current;
    if (id === undefined || target === null) return;
    const pause = () => setPaused(true);
    const resume = () => setPaused(false);
    target.addEventListener("pointerenter", pause);
    target.addEventListener("pointerleave", resume);
    target.addEventListener("focusin", pause);
    target.addEventListener("focusout", resume);
    return () => {
      target.removeEventListener("pointerenter", pause);
      target.removeEventListener("pointerleave", resume);
      target.removeEventListener("focusin", pause);
      target.removeEventListener("focusout", resume);
      setPaused(false);
    };
  }, [id]);
  useEffect(() => {
    if (id === undefined || paused) return;
    const timer = setTimeout(() => dismiss.current(), durationMs);
    return () => clearTimeout(timer);
  }, [id, paused, durationMs]);
  return (
    <div className="pointer-events-none fixed inset-x-4 bottom-4 z-40 flex justify-end">
      <div role="status">
        {notice ? (
          <section
            ref={element}
            className="pointer-events-auto flex max-w-sm items-start gap-3 rounded-lg border bg-card p-4 text-sm text-card-foreground shadow-lg"
          >
            <CircleCheckIcon aria-hidden="true" className="mt-0.5 size-4 shrink-0 text-success" />
            <div className="grid min-w-0 gap-0.5">
              <p className="font-medium wrap-anywhere">{notice.title}</p>
              <p className="text-muted-foreground">{notice.body}</p>
            </div>
            <Button
              variant="ghost"
              size="icon"
              className="-my-2 -mr-2 size-8"
              aria-label={dismissLabel}
              onClick={onDismiss}
            >
              <XIcon />
            </Button>
          </section>
        ) : null}
      </div>
    </div>
  );
}
