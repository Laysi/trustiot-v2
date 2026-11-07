import { Copy, X } from "lucide-react";
import { type ReactNode, useCallback, useEffect, useId, useRef, useState } from "react";

import { Button } from "../components/ui/button.tsx";
import type { MessageId } from "../i18n/messages.ts";
import { useDashboardEnv } from "./context.tsx";
import { cellAt, type DashboardData, type Dataset, datasetOf, stateOf } from "./data.ts";
import { useComponentExtras } from "./extensions.tsx";
import { formulaDatasetNames, type SourceRef } from "./model.ts";
import { RequestSummary } from "./RequestSummary.tsx";
import { resultTimeRange } from "./range.ts";
import { MAX_SERIES } from "./spec.ts";

/**
 * Rows the panel lists one by one. A point or a bar group has at most one row per series, so
 * every row of what was clicked is listed; only a longer selection would be summarized.
 */
const MAX_LISTED_ROWS = MAX_SERIES;

/**
 * A fact's name and value: side by side once the panel itself is 20rem wide (a container query on
 * the panel, not the window's width: the panel can sit in a narrow column of a wide window).
 */
function Field(props: { label: string; children: ReactNode }) {
  return (
    <div className="grid gap-0.5 @xs:grid-cols-[8rem_1fr] @xs:gap-3">
      <dt className="text-muted-foreground">{props.label}</dt>
      <dd className="min-w-0 break-words">{props.children}</dd>
    </div>
  );
}

const CODE_CLASS =
  "max-h-48 min-w-0 flex-1 overflow-auto rounded-md bg-muted px-3 py-2 font-mono text-xs leading-relaxed whitespace-pre-wrap";

function Code(props: { children: string }) {
  return <pre className={CODE_CLASS}>{props.children}</pre>;
}

/** A query's text in a block the keyboard can reach, to scroll it and select what to copy. */
function QueryCode(props: { children: string; label: string }) {
  return (
    <fieldset className="m-0 flex min-w-0 flex-1 border-0 p-0">
      <legend className="sr-only">{props.label}</legend>
      {/* biome-ignore lint/a11y/noNoninteractiveTabindex: a scrollable block must be reachable by keyboard */}
      <pre tabIndex={0} className={CODE_CLASS}>
        {props.children}
      </pre>
    </fieldset>
  );
}

/** Copies `text` and announces `done`, or that it could not be copied. */
type CopyText = (text: string, done: MessageId) => void;

/** A query's text with the button that copies it (design: 「複製 SQL」). */
function CopyableCode(props: { text: string; label: string; copyLabel: string; done: MessageId; copy: CopyText }) {
  return (
    <div className="flex items-start gap-1.5">
      <QueryCode label={props.label}>{props.text}</QueryCode>
      <Button
        variant="ghost"
        size="sm"
        className="shrink-0"
        aria-label={props.copyLabel}
        onClick={() => props.copy(props.text, props.done)}
      >
        <Copy aria-hidden="true" />
      </Button>
    </div>
  );
}

/**
 * A dataset's title and description, when this result was fetched, which stretch of time it covers
 * and how many rows it has (whatever was clicked), its query, and the clicked rows.
 */
function DatasetSource(props: { data: DashboardData; dataset: Dataset; rows: readonly number[]; copy: CopyText }) {
  const { t, format } = useDashboardEnv();
  const { dataset, rows } = props;
  const state = stateOf(props.data, dataset.name);
  const source = dataset.source;
  const range = state.status === "ready" ? resultTimeRange(state.result) : null;
  return (
    <div className="grid gap-2">
      <dl className="grid gap-2 text-sm">
        <Field label={t("dashboard.source.dataset")}>
          <span className="font-medium">{dataset.title}</span>
          {dataset.description ? <p className="text-muted-foreground">{dataset.description}</p> : null}
        </Field>
        <Field label={t("dashboard.source.queried_at")}>
          {state.status === "ready" ? (
            <span className="tabular-nums">{format.instant(state.result.meta.queried_at)}</span>
          ) : (
            <span className="text-muted-foreground">{t("dashboard.source.not_run")}</span>
          )}
        </Field>
        {range !== null ? (
          <Field label={t("dashboard.source.range")}>
            <span className="tabular-nums">
              {t("dashboard.source.range_value", {
                from: format.wallTime(range.from, range.dateOnly),
                to: format.wallTime(range.to, range.dateOnly),
              })}
            </span>
          </Field>
        ) : null}
        {state.status === "ready" ? (
          <Field label={t("dashboard.source.rows")}>
            <span className="tabular-nums">{format.number(state.result.rows.length)}</span>
          </Field>
        ) : null}
        {source.kind === "sql" ? (
          <Field label={t("dashboard.source.sql")}>
            <CopyableCode
              text={source.sql}
              label={t("dashboard.source.sql_label")}
              copyLabel={t("dashboard.source.copy_sql")}
              done="dashboard.source.copied_sql"
              copy={props.copy}
            />
          </Field>
        ) : (
          <>
            <Field
              label={t(source.kind === "telemetry_usage" ? "dashboard.source.usage" : "dashboard.source.statistics")}
            >
              <RequestSummary request={source.request} />
            </Field>
            <Field label={t("dashboard.source.raw")}>
              <CopyableCode
                text={JSON.stringify(source.request, null, 2)}
                label={t("dashboard.source.request_label")}
                copyLabel={t("dashboard.source.copy_request")}
                done="dashboard.source.copied_request"
                copy={props.copy}
              />
            </Field>
          </>
        )}
      </dl>
      {state.status === "ready" && rows.length > 0 ? (
        <div className="grid gap-2">
          {rows.slice(0, MAX_LISTED_ROWS).map((row) => (
            <section key={row} aria-label={t("dashboard.source.row", { row: row + 1 })} className="grid gap-1">
              <h5 className="text-xs font-medium text-muted-foreground">
                {t("dashboard.source.row", { row: row + 1 })}
              </h5>
              {/* Capped so a name and its value stay close on a wide card; two columns only when
                  the panel has room for them. A long column name or value wraps, never cut. */}
              <dl className="grid max-w-2xl gap-x-4 gap-y-1 text-sm @lg:grid-cols-2">
                {state.result.columns.map((column, index) => (
                  <div key={column.name} className="flex min-w-0 justify-between gap-3 border-b border-dashed py-0.5">
                    <dt className="min-w-0 font-mono text-xs text-muted-foreground wrap-anywhere">{column.name}</dt>
                    <dd className="min-w-0 text-right tabular-nums wrap-anywhere" data-testid="source-cell">
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
 * reads; a constant with its note. Everything is text: SQL and parameters in a `<pre>`, which can
 * be copied. A page that can ask the assistant adds 「請 AI 解釋」 to the foot (`explainSource` of
 * `ComponentExtras`).
 */
export function SourcePanel(props: { source: SourceRef; data: DashboardData; onClose(): void }) {
  const { t, format } = useDashboardEnv();
  const { explainSource } = useComponentExtras();
  const heading = useRef<HTMLHeadingElement>(null);
  const headingId = useId();
  const { source, data } = props;
  // What copying said last, announced from a live region that is always in the panel.
  const [status, setStatus] = useState("");

  // biome-ignore lint/correctness/useExhaustiveDependencies: focus moves to the panel each time it shows a new source
  useEffect(() => {
    heading.current?.focus();
    setStatus("");
  }, [source]);

  const copy = useCallback<CopyText>(
    (text, done) => {
      const succeeded = () => setStatus(t(done));
      const failed = () => setStatus(t("dashboard.source.copy_failed"));
      try {
        // A browser without the clipboard API, or a page it is refused to, ends up in `failed`.
        navigator.clipboard.writeText(text).then(succeeded, failed);
      } catch {
        failed();
      }
    },
    [t],
  );

  let body: ReactNode = null;
  let subject: string | null = null;
  if (source.kind === "rows") {
    const dataset = datasetOf(data, source.dataset);
    body = dataset ? <DatasetSource data={data} dataset={dataset} rows={source.rows} copy={copy} /> : null;
    subject = dataset?.title ?? null;
  } else if (source.kind === "computation") {
    const computation = data.computations.find((candidate) => candidate.name === source.name);
    subject = computation?.title ?? null;
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
          <DatasetSource key={dataset.name} data={data} dataset={dataset} rows={[]} copy={copy} />
        ))}
      </div>
    ) : null;
  } else {
    const constant = data.constants.find((candidate) => candidate.name === source.name);
    subject = constant?.title ?? null;
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
    // `data-source-panel` lets the grid of a version's items (`VersionItems`) give the component
    // that holds an open panel the whole row; `@container` sizes the facts by the panel's width.
    <section
      aria-labelledby={headingId}
      data-source-panel=""
      className="@container grid gap-3 rounded-lg border bg-muted/40 p-4"
    >
      <div className="flex items-center justify-between gap-2">
        <h4 id={headingId} ref={heading} tabIndex={-1} className="text-sm font-semibold outline-none">
          {t("dashboard.source.title")}
        </h4>
        <Button variant="ghost" size="sm" onClick={props.onClose} aria-label={t("dashboard.source.close")}>
          <X aria-hidden="true" />
        </Button>
      </div>
      {body}
      <div className="flex flex-wrap items-center justify-between gap-x-3 gap-y-2">
        <p role="status" className="min-w-0 text-sm text-muted-foreground empty:sr-only">
          {status}
        </p>
        {body !== null && subject !== null ? explainSource?.({ title: subject }) : null}
      </div>
    </section>
  );
}
