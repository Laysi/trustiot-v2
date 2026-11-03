import { X } from "lucide-react";
import { type ReactNode, useEffect, useId, useRef } from "react";

import { Button } from "../components/ui/button.tsx";
import { useDashboardEnv } from "./context.tsx";
import { cellAt, type DashboardData, type Dataset, datasetOf, stateOf } from "./data.ts";
import { formulaDatasetNames, type SourceRef } from "./model.ts";
import { MAX_SERIES } from "./spec.ts";

/**
 * Rows the panel lists one by one. A point or a bar group has at most one row per series, so
 * every row of what was clicked is listed; only a longer selection would be summarized.
 */
const MAX_LISTED_ROWS = MAX_SERIES;

function Field(props: { label: string; children: ReactNode }) {
  return (
    <div className="grid gap-0.5 sm:grid-cols-[8rem_1fr] sm:gap-3">
      <dt className="text-muted-foreground">{props.label}</dt>
      <dd className="min-w-0 break-words">{props.children}</dd>
    </div>
  );
}

function Code(props: { children: string }) {
  return (
    <pre className="max-h-48 overflow-auto rounded-md bg-muted px-3 py-2 font-mono text-xs leading-relaxed whitespace-pre-wrap">
      {props.children}
    </pre>
  );
}

/** A dataset's title, description, query and when this result was fetched. */
function DatasetSource(props: { data: DashboardData; dataset: Dataset; rows: readonly number[] }) {
  const { t, format } = useDashboardEnv();
  const { dataset, rows } = props;
  const state = stateOf(props.data, dataset.name);
  const source = dataset.source;
  return (
    <div className="grid gap-2">
      <dl className="grid gap-2 text-sm">
        <Field label={t("dashboard.source.dataset")}>
          <span className="font-medium">{dataset.title}</span>
          {dataset.description ? <p className="text-muted-foreground">{dataset.description}</p> : null}
        </Field>
        {source.kind === "sql" ? (
          <Field label={t("dashboard.source.sql")}>
            <Code>{source.sql}</Code>
          </Field>
        ) : (
          <Field
            label={t(source.kind === "telemetry_usage" ? "dashboard.source.usage" : "dashboard.source.statistics")}
          >
            <Code>{JSON.stringify(source.request, null, 2)}</Code>
          </Field>
        )}
        <Field label={t("dashboard.source.queried_at")}>
          {state.status === "ready" ? (
            <span className="tabular-nums">{format.instant(state.result.meta.queried_at)}</span>
          ) : (
            <span className="text-muted-foreground">{t("dashboard.source.not_run")}</span>
          )}
        </Field>
      </dl>
      {state.status === "ready" ? (
        rows.length === 0 ? (
          <p className="text-sm text-muted-foreground">
            {t("dashboard.source.whole", { count: state.result.rows.length })}
          </p>
        ) : (
          <div className="grid gap-2">
            {rows.slice(0, MAX_LISTED_ROWS).map((row) => (
              <section key={row} aria-label={t("dashboard.source.row", { row: row + 1 })} className="grid gap-1">
                <h5 className="text-xs font-medium text-muted-foreground">
                  {t("dashboard.source.row", { row: row + 1 })}
                </h5>
                {/* Capped so a name and its value stay close on a wide card. */}
                <dl className="grid max-w-2xl gap-x-4 gap-y-1 text-sm sm:grid-cols-2">
                  {state.result.columns.map((column, index) => (
                    <div key={column.name} className="flex min-w-0 justify-between gap-3 border-b border-dashed py-0.5">
                      <dt className="truncate font-mono text-xs text-muted-foreground">{column.name}</dt>
                      <dd className="tabular-nums" data-testid="source-cell">
                        {format.cell(column.type, cellAt(state.result, row, index))}
                      </dd>
                    </div>
                  ))}
                </dl>
              </section>
            ))}
            {rows.length > MAX_LISTED_ROWS ? (
              <p className="text-xs text-muted-foreground">
                {t("dashboard.source.more_rows", { count: rows.length - MAX_LISTED_ROWS })}
              </p>
            ) : null}
          </div>
        )
      ) : null}
    </div>
  );
}

function datasetsOfFormula(formula: string, data: DashboardData): Dataset[] {
  return formulaDatasetNames(formula).flatMap((name) => datasetOf(data, name) ?? []);
}

/**
 * Where a value comes from (docs/architecture/ai-assistant.md, 「儀表板」 point 6): the dataset
 * with its query, the rows and when it ran; a computation with its formula and the datasets it
 * reads; a constant with its note. Everything is text: SQL and parameters in a `<pre>`.
 */
export function SourcePanel(props: { source: SourceRef; data: DashboardData; onClose(): void }) {
  const { t, format } = useDashboardEnv();
  const heading = useRef<HTMLHeadingElement>(null);
  const headingId = useId();
  const { source, data } = props;

  // biome-ignore lint/correctness/useExhaustiveDependencies: focus moves to the panel each time it shows a new source
  useEffect(() => {
    heading.current?.focus();
  }, [source]);

  let body: ReactNode = null;
  if (source.kind === "rows") {
    const dataset = datasetOf(data, source.dataset);
    body = dataset ? <DatasetSource data={data} dataset={dataset} rows={source.rows} /> : null;
  } else if (source.kind === "computation") {
    const computation = data.computations.find((candidate) => candidate.name === source.name);
    body = computation ? (
      <div className="grid gap-3">
        <dl className="grid gap-2 text-sm">
          <Field label={t("dashboard.source.computation")}>
            <span className="font-medium">{computation.title}</span>
          </Field>
          <Field label={t("dashboard.source.formula")}>
            <Code>{computation.formula}</Code>
          </Field>
        </dl>
        {datasetsOfFormula(computation.formula, data).map((dataset) => (
          <DatasetSource key={dataset.name} data={data} dataset={dataset} rows={[]} />
        ))}
      </div>
    ) : null;
  } else {
    const constant = data.constants.find((candidate) => candidate.name === source.name);
    body = constant ? (
      <dl className="grid gap-2 text-sm">
        <Field label={t("dashboard.source.constant")}>
          <span className="font-medium">{constant.title}</span>
        </Field>
        <Field label={t("dashboard.source.value")}>
          <span className="tabular-nums">
            {format.withUnit(
              typeof constant.value === "number" ? format.number(constant.value) : constant.value,
              constant.unit ?? undefined,
            )}
          </span>
        </Field>
        {constant.note ? <Field label={t("dashboard.source.note")}>{constant.note}</Field> : null}
      </dl>
    ) : null;
  }

  return (
    <section aria-labelledby={headingId} className="grid gap-3 rounded-lg border bg-muted/40 p-4">
      <div className="flex items-center justify-between gap-2">
        <h4 id={headingId} ref={heading} tabIndex={-1} className="text-sm font-semibold outline-none">
          {t("dashboard.source.title")}
        </h4>
        <Button variant="ghost" size="sm" onClick={props.onClose} aria-label={t("dashboard.source.close")}>
          <X aria-hidden="true" />
        </Button>
      </div>
      {body}
    </section>
  );
}
