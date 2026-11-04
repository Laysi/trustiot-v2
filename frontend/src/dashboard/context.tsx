import { createContext, type ReactNode, useContext, useEffect, useMemo, useState } from "react";

import type { Locale, MessageId } from "../i18n/messages.ts";
import { type MessageValues, translate } from "../i18n/translate.ts";
import { type DashboardFormat, dashboardFormat } from "./format.ts";

/**
 * The language, time zone and formatting the components use. `DashboardComponent` provides it from
 * the user's preferences; tests provide it directly, without a session.
 */
export interface DashboardEnv {
  locale: Locale;
  timeZone: string;
  t(id: MessageId, values?: MessageValues): string;
  format: DashboardFormat;
}

const DashboardEnvContext = createContext<DashboardEnv | null>(null);

export function DashboardEnvProvider(props: { locale: Locale; timeZone: string; children: ReactNode }) {
  const { locale, timeZone } = props;
  const value = useMemo<DashboardEnv>(
    () => ({
      locale,
      timeZone,
      t: (id, values) => translate(locale, id, values),
      format: dashboardFormat(locale, timeZone),
    }),
    [locale, timeZone],
  );
  return <DashboardEnvContext.Provider value={value}>{props.children}</DashboardEnvContext.Provider>;
}

export function useDashboardEnv(): DashboardEnv {
  const value = useContext(DashboardEnvContext);
  if (value === null) throw new Error("a dashboard component needs a DashboardEnvProvider");
  return value;
}

/** Changes whenever `<html data-theme>` does, so charts read the theme's colors again. */
export function useThemeVersion(): number {
  const [version, setVersion] = useState(0);
  useEffect(() => {
    if (typeof MutationObserver === "undefined") return;
    const observer = new MutationObserver(() => setVersion((current) => current + 1));
    observer.observe(document.documentElement, { attributes: true, attributeFilter: ["data-theme"] });
    return () => observer.disconnect();
  }, []);
  return version;
}
