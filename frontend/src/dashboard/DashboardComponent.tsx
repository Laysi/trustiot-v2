import { CircleAlert, Database, RefreshCw } from "lucide-react";
import { Component, Fragment, type ReactNode, type Ref, useId, useMemo, useRef } from "react";

import { Alert, AlertDescription, AlertTitle } from "../components/ui/alert.tsx";
import { Button } from "../components/ui/button.tsx";
import { useI18n } from "../i18n/useI18n.ts";
import { usePreferences } from "../preferences/PreferencesProvider.tsx";
import { BarBody, TimeSeriesBody } from "./charts.tsx";
import { DashboardEnvProvider, useDashboardEnv } from "./context.tsx";
import type { DashboardData } from "./data.ts";
import { useComponentExtras } from "./extensions.tsx";
import { type ComponentModel, type ComponentView, componentModel, type SourceRef } from "./model.ts";
import { datasetTitle, problemMessage } from "./problems.ts";
import { SourcePanel } from "./SourcePanel.tsx";
import { SourceStateProvider, useSourceExpanded, useSourcePanel } from "./source-state.tsx";
import type { PlatformComponent } from "./spec.ts";
import { componentQueriedAt, componentSource } from "./uses.ts";
import { KpiBody, StatusBody, TableBody } from "./values.tsx";

export interface DashboardComponentProps {
  /**
   * An item of a version's component list but the custom page, which is not drawn here: the page
   * gives it to the sandbox (`isPlatformComponent` tells them apart).
   */
  spec: PlatformComponent;
  data: DashboardData;
  /** Runs a failed dataset again; without it a failed run only says why. */
  onRetry?(dataset: string): void;
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
  const expanded = useSourceExpanded();
  const empty =
    (view.kind === "time_series" && view.xs.length === 0) || (view.kind === "bar" && view.categories.length === 0);
  if (empty) {
    return (
      <div
        role="status"
        className="grid place-items-center gap-2 rounded-md border border-dashed px-4 py-10 text-center"
      >
        <p className="text-sm text-muted-foreground">{t("dashboard.empty")}</p>
        <Button
          size="sm"
          variant="outline"
          aria-expanded={expanded({ kind: "rows", dataset: view.dataset, rows: [] })}
          onClick={() => onSource({ kind: "rows", dataset: view.dataset, rows: [] })}
        >
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
  onRetry?(dataset: string): void;
}) {
  const env = useDashboardEnv();
  const expanded = useSourceExpanded();
  const { model, data, onRetry } = props;
  const problem = model.problem;
  // A failed run can be run again; every other problem is in the spec or the result itself.
  const retry = problem.code === "dataset_failed" && onRetry !== undefined ? () => onRetry(problem.dataset) : null;
  return (
    <Alert variant="destructive">
      <CircleAlert aria-hidden="true" />
      {/* min-w-0: in a narrow card (a KPI in a quarter of the row) the text and the buttons wrap
          inside the alert instead of widening its grid column past the card. */}
      <AlertTitle className="min-w-0">{env.t("dashboard.error.title")}</AlertTitle>
      <AlertDescription className="min-w-0">
        <p data-testid="problem">{problemMessage(model.problem, data, env)}</p>
        {model.sources.length > 0 || retry !== null ? (
          <div className="flex flex-wrap gap-2 pt-1">
            {retry !== null ? (
              <Button size="sm" variant="outline" className="text-foreground" onClick={retry}>
                <RefreshCw aria-hidden="true" />
                {env.t("dashboard.retry")}
              </Button>
            ) : null}
            {model.sources.map((source) => (
              <Button
                key={`${source.kind}:${sourceLabel(source, data)}`}
                size="sm"
                variant="outline"
                className="h-auto min-h-8 max-w-full py-1.5 text-left whitespace-normal text-foreground"
                aria-expanded={expanded(source)}
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
 * The card every component sits in. Its header follows docs/design/chat-dashboards/dashboard.html:
 * the title with a line under it (a chart's unit, when its data was queried), and the button that
 * opens the source of the whole component at the end. Then `children`.
 */
function Card(props: {
  spec: PlatformComponent;
  data?: DashboardData;
  onSource?(source: SourceRef): void;
  /** The card's element, for what has to know whether focus is in it. */
  cardRef?: Ref<HTMLElement>;
  children: ReactNode;
}) {
  const { spec, data, onSource } = props;
  const { t, format } = useDashboardEnv();
  const { footer } = useComponentExtras();
  const expanded = useSourceExpanded();
  const titleId = useId();
  const chartUnit = spec.kind === "time_series" || spec.kind === "bar" ? (spec.unit ?? null) : null;
  const queriedAt = data === undefined ? null : componentQueriedAt(spec, data);
  const facts: { key: string; node: ReactNode }[] = [];
  if (chartUnit !== null) facts.push({ key: "unit", node: <span>{t("dashboard.unit", { unit: chartUnit })}</span> });
  if (queriedAt !== null) {
    facts.push({
      key: "queried",
      node: <span className="tabular-nums">{t("dashboard.card.queried_at", { time: format.instant(queriedAt) })}</span>,
    });
  }
  return (
    <section
      ref={props.cardRef}
      aria-labelledby={titleId}
      data-component={spec.kind}
      className="flex min-w-0 flex-col gap-3 rounded-xl border bg-card p-4 text-card-foreground shadow-sm sm:p-5"
    >
      <header className="flex min-w-0 items-start justify-between gap-2">
        <div className="grid min-w-0 gap-0.5">
          <h3 id={titleId} className="min-w-0 text-base leading-snug font-semibold break-words">
            {spec.title}
          </h3>
          {facts.length > 0 ? (
            <p className="flex flex-wrap items-center gap-x-1.5 text-xs text-muted-foreground" data-testid="card-facts">
              {facts.map((fact, index) => (
                <Fragment key={fact.key}>
                  {index > 0 ? <span aria-hidden="true">·</span> : null}
                  {fact.node}
                </Fragment>
              ))}
            </p>
          ) : null}
        </div>
        {onSource !== undefined ? (
          <Button
            variant="ghost"
            size="sm"
            className="-my-1 -mr-2 shrink-0"
            aria-label={t("dashboard.card.source_of", { title: spec.title })}
            aria-expanded={expanded(componentSource(spec))}
            onClick={() => onSource(componentSource(spec))}
          >
            <Database aria-hidden="true" />
            {t("dashboard.source.title")}
          </Button>
        ) : null}
      </header>
      {props.children}
      {footer?.(spec)}
    </section>
  );
}

function ComponentCard(props: DashboardComponentProps) {
  const { spec, data, onRetry } = props;
  const model = useMemo(() => componentModel(spec, data), [spec, data]);
  const card = useRef<HTMLElement>(null);
  const { source, open, close } = useSourcePanel(card);
  return (
    <SourceStateProvider source={source}>
      <Card spec={spec} data={data} onSource={open} cardRef={card}>
        {model.state === "loading" ? (
          <Loading compact={spec.kind === "kpi" || spec.kind === "status"} />
        ) : model.state === "error" ? (
          <ProblemState model={model} data={data} onSource={open} {...(onRetry ? { onRetry } : {})} />
        ) : (
          <Body view={model.view} onSource={open} />
        )}
        {source !== null ? <SourcePanel source={source} data={data} onClose={close} /> : null}
      </Card>
    </SourceStateProvider>
  );
}

/** What a card shows when the component itself failed: its title, and that it can't be shown. */
function CrashedCard(props: { spec: PlatformComponent }) {
  const { t } = useDashboardEnv();
  return (
    <Card spec={props.spec}>
      <Alert variant="destructive">
        <CircleAlert aria-hidden="true" />
        <AlertTitle>{t("dashboard.error.title")}</AlertTitle>
        <AlertDescription>
          <p data-testid="crashed">{t("dashboard.error.crashed")}</p>
        </AlertDescription>
      </Alert>
    </Card>
  );
}

interface BoundaryProps extends DashboardComponentProps {
  children: ReactNode;
}

interface BoundaryState {
  failed: boolean;
  /** The props the state is for: new ones try again. */
  spec: PlatformComponent;
  data: DashboardData;
}

/**
 * Keeps an error inside one component to its own card: a bug in a component, or ECharts throwing
 * on an option, shows that card's title and that it can't be shown, and the rest of the page goes
 * on. Without it React hands the error to the route's error page. A new spec or new data (the
 * next viewing) tries again.
 */
class ComponentBoundary extends Component<BoundaryProps, BoundaryState> {
  override state: BoundaryState = { failed: false, spec: this.props.spec, data: this.props.data };

  static getDerivedStateFromError(): Partial<BoundaryState> {
    return { failed: true };
  }

  static getDerivedStateFromProps(props: BoundaryProps, state: BoundaryState): Partial<BoundaryState> | null {
    if (props.spec === state.spec && props.data === state.data) return null;
    return { failed: false, spec: props.spec, data: props.data };
  }

  override render(): ReactNode {
    return this.state.failed ? <CrashedCard spec={this.props.spec} /> : this.props.children;
  }
}

/**
 * One component of a dashboard (docs/architecture/ai-assistant.md, 「元件模式」) in a card: its
 * title, then the data, a loading placeholder, or why the data cannot be shown; under it, the
 * source of the value last clicked. For the dashboard page and the assistant's answers. An error
 * inside it stays in its card.
 */
export function DashboardComponentView(props: DashboardComponentProps) {
  return (
    <ComponentBoundary spec={props.spec} data={props.data}>
      <ComponentCard {...props} />
    </ComponentBoundary>
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
