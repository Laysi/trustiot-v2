import type { I18n } from "../../i18n/useI18n.ts";
import { useI18n } from "../../i18n/useI18n.ts";
import { useNow } from "./queries.ts";

/** How long, as "2 min 14 s": minutes only from the first minute. */
export function formatWait(milliseconds: number, t: I18n["t"]): string {
  const total = Math.max(0, Math.floor(milliseconds / 1000));
  const minutes = Math.floor(total / 60);
  const seconds = total % 60;
  return minutes === 0 ? t("platform.wait.seconds", { seconds }) : t("platform.wait.minutes", { minutes, seconds });
}

/** After this long without certs' confirmation the page says something may be stuck (decision Q4). */
export const STUCK_AFTER_MS = 10 * 60_000;

/**
 * The time a suspension has been waiting for certs, counted from when the tenant entered the state
 * (`state_changed_at`). The figure counts up on screen every second; what a screen reader hears
 * changes once a minute (`aria-live="polite"`), so it is not read out second by second.
 */
export function WaitedFor({ since }: { since: string }) {
  const { t } = useI18n();
  const now = useNow(1000);
  const minuteNow = useNow(60_000);
  const started = Date.parse(since);
  return (
    <>
      <span aria-hidden="true">{t("platform.wait.waited", { time: formatWait(now - started, t) })}</span>
      <span role="status" className="sr-only">
        {t("platform.wait.waited", { time: formatWait(Math.floor((minuteNow - started) / 60_000) * 60_000, t) })}
      </span>
    </>
  );
}

/** Whether a suspension has waited past `STUCK_AFTER_MS`. */
export function useStuck(since: string, status: string): boolean {
  const now = useNow(30_000);
  return status === "processing" && now - Date.parse(since) > STUCK_AFTER_MS;
}
