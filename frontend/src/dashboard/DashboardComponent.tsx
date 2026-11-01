import { CircleAlert } from "lucide-react";
import { useId, useMemo, useState } from "react";

import { Alert, AlertDescription, AlertTitle } from "../components/ui/alert.tsx";
import { Button } from "../components/ui/button.tsx";
import { useI18n } from "../i18n/useI18n.ts";
import { usePreferences } from "../preferences/PreferencesProvider.tsx";
import { BarBody, TimeSeriesBody } from "./charts.tsx";
import { DashboardEnvProvider, useDashboardEnv } from "./context.tsx";
import type { DashboardData } from "./data.ts";
import { type ComponentModel, type ComponentView, componentModel, type SourceRef } from "./model.ts";
import { datasetTitle, problemMessage } from "./problems.ts";
import { SourcePanel } from "./SourcePanel.tsx";
import type { ComponentSpec } from "./spec.ts";
import { KpiBody, StatusBody, TableBody } from "./values.tsx";

export interface DashboardComponentProps {
  spec: ComponentSpec;
  data: DashboardData;
}

function sourceLabel(source: SourceRef, data: DashboardData): string {
  switch (source.kind) {
    case "rows":
      return datasetTitle(data, source.dataset);
    case "computation":
      return data.computations.find((computation) => computation.name === source.name)?.title ?? source.name;
    case "constant":
      return data.constants.find((constant) => constant.name === source.name)?.title ?? source.name;
  }
}

function Body(props: { view: ComponentView; onSource(source: SourceRef): void }) {
  const { view, onSource } = props;
  const { t } = useDashboardEnv();
  const empty =
    (view.kind === "time_series" && view.xs.length === 0) || (view.kind === "bar" && view.categories.length === 0);
  if (empty) {
    return (
      <div
        role="status"
        className="grid place-items-center gap-2 rounded-md border border-dashed px-4 py-10 text-center"
      >
        <p className="text-sm text-muted-foreground">{t("dashboard.empty")}</p>
        <Button size="sm" variant="outline" onClick={() => onSource({ kind: "rows", dataset: view.dataset, rows: [] })}>
          {t("dashboard.source.open")}
        </Button>
      </div>
    );
  }
  switch (view.kind) {
    case "time_series":
      return <TimeSeriesBody view={view} onSource={onSource} />;
    case "bar":
      return <BarBody view={view} onSource={onSource} />;
    case "kpi":
      return <KpiBody view={view} onSource={onSource} />;
    case "table":
      return <TableBody view={view} onSource={onSource} />;
    case "status":
      return <StatusBody view={view} onSource={onSource} />;
  }
}

function Loading(props: { compact: boolean }) {
  const { t } = useDashboardEnv();
  return (
    <div role="status" className="grid gap-2">
      <span className="sr-only">{t("dashboard.loading")}</span>
      <div
        aria-hidden="true"
        className={props.compact ? "h-24 animate-pulse rounded-md bg-muted" : "h-72 animate-pulse rounded-md bg-muted"}
      />
    </div>
  );
}

function ProblemState(props: {
  model: Extract<ComponentModel, { state: "error" }>;
  data: DashboardData;
  onSource(source: SourceRef): void;
}) {
  const env = useDashboardEnv();
  const { model, data } = props;
  return (
    <Alert variant="destructive">
      <CircleAlert aria-hidden="true" />
      <AlertTitle>{env.t("dashboard.error.title")}</AlertTitle>
      <AlertDescription>
        <p data-testid="problem">{problemMessage(model.problem, data, env)}</p>
        {model.sources.length > 0 ? (
          <div className="flex flex-wrap gap-2 pt-1">
            {model.sources.map((source) => (
              <Button
                key={`${source.kind}:${sourceLabel(source, data)}`}
                size="sm"
                variant="outline"
                className="text-foreground"
                onClick={() => props.onSource(source)}
              >
                {env.t("dashboard.source.open_named", { name: sourceLabel(source, data) })}
              </Button>
            ))}
          </div>
        ) : null}
      </AlertDescription>
    </Alert>
  );
}

/**
 * One component of a dashboard (docs/architecture/ai-assistant.md, 「元件模式」) in a card: its
 * title, then the data, a loading placeholder, or why the data cannot be shown; under it, the
 * source of the value last clicked. For the dashboard page and the assistant's answers.
 */
export function DashboardComponentView(props: DashboardComponentProps) {
  const { spec, data } = props;
  const { t } = useDashboardEnv();
  const titleId = useId();
  const model = useMemo(() => componentModel(spec, data), [spec, data]);
  const [source, setSource] = useState<SourceRef | null>(null);
  const chartUnit = (spec.kind === "time_series" || spec.kind === "bar") && spec.unit !== undefined ? spec.unit : null;
  return (
    <section
      aria-labelledby={titleId}
      data-component={spec.kind}
      className="flex min-w-0 flex-col gap-3 rounded-xl border bg-card p-4 text-card-foreground shadow-sm sm:p-5"
    >
      <header className="flex flex-wrap items-baseline justify-between gap-x-3 gap-y-1">
        <h3 id={titleId} className="min-w-0 text-base leading-snug font-semibold break-words">
          {spec.title}
        </h3>
        {chartUnit !== null ? (
          <span className="text-xs text-muted-foreground">{t("dashboard.unit", { unit: chartUnit })}</span>
        ) : null}
      </header>
      {model.state === "loading" ? (
        <Loading compact={spec.kind === "kpi" || spec.kind === "status"} />
      ) : model.state === "error" ? (
        <ProblemState model={model} data={data} onSource={setSource} />
      ) : (
        <Body view={model.view} onSource={setSource} />
      )}
      {source !== null ? <SourcePanel source={source} data={data} onClose={() => setSource(null)} /> : null}
    </section>
  );
}

/** `DashboardComponentView` in the user's language and time zone. */
export function DashboardComponent(props: DashboardComponentProps) {
  const { locale } = useI18n();
  const { timeZone } = usePreferences();
  return (
    <DashboardEnvProvider locale={locale} timeZone={timeZone}>
      <DashboardComponentView {...props} />
    </DashboardEnvProvider>
  );
}
