import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useId, useState } from "react";
import { ApiError } from "../../../api/errors.ts";
import { usePromptApi } from "../../../api/pending/PromptApiProvider.tsx";
import type { EvalMetrics, EvalReport, EvalRun } from "../../../api/pending/prompts.ts";
import { Alert, AlertDescription, AlertTitle } from "../../../components/ui/alert.tsx";
import {
  AlertDialog,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
} from "../../../components/ui/alert-dialog.tsx";
import { Button } from "../../../components/ui/button.tsx";
import { Card, CardContent, CardFooter, CardHeader, CardTitle } from "../../../components/ui/card.tsx";
import { Spinner } from "../../../components/ui/spinner.tsx";
import type { MessageId } from "../../../i18n/messages.ts";
import { useI18n } from "../../../i18n/useI18n.ts";
import { evalOptionsKey, evalRunKey } from "./queries.ts";
import { Cell, DataTable, Meter, Row, Th } from "./ui.tsx";

type Verdict = "better" | "worse" | "same";

const VERDICT_TEXT: Record<Verdict, MessageId> = {
  better: "platform_ai.eval.better",
  worse: "platform_ai.eval.worse",
  same: "platform_ai.eval.same",
};

function verdict(delta: number, higherIsBetter: boolean): Verdict {
  if (Math.abs(delta) < 1e-9) return "same";
  return delta > 0 === higherIsBetter ? "better" : "worse";
}

/** The delta of two latencies: worse if either got slower, better if one got faster and none slower. */
function latencyVerdict(a: EvalMetrics, b: EvalMetrics): Verdict {
  const deltas = [b.latency_p50 - a.latency_p50, b.latency_p95 - a.latency_p95];
  if (deltas.some((delta) => delta > 1e-9)) return "worse";
  return deltas.some((delta) => delta < -1e-9) ? "better" : "same";
}

function useNumber() {
  const { locale } = useI18n();
  return (value: number, digits = 1, sign = false) =>
    new Intl.NumberFormat(locale, {
      minimumFractionDigits: digits,
      maximumFractionDigits: digits,
      ...(sign ? { signDisplay: "exceptZero" as const } : {}),
    }).format(value);
}

/** The dialog that picks which part of the question bank a draft is run on, and starts the run. */
export function EvalStartDialog({
  open,
  onOpenChange,
  service,
  role,
  content,
  onStarted,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  service: string;
  role: string;
  content: string;
  onStarted: (run: EvalRun) => void;
}) {
  const { t, errorMessage } = useI18n();
  const api = usePromptApi();
  const number = useNumber();
  const options = useQuery({
    queryKey: evalOptionsKey(service, role),
    queryFn: ({ signal }) => api.evalOptions(service, role, signal),
    enabled: open,
  });
  const [chosen, setChosen] = useState<ReadonlySet<string> | null>(null);
  const subsets = options.data?.subsets ?? [];
  // Until the administrator chooses, the first two groups are selected, as the design draws it.
  const selected = chosen ?? new Set(subsets.slice(0, 2).map((subset) => subset.id));
  const questions = subsets
    .filter((subset) => selected.has(subset.id))
    .reduce((sum, subset) => sum + subset.question_count, 0);
  const cost = options.data?.estimated_cost_per_question;

  const start = useMutation({
    mutationFn: () => api.startEval(service, role, { content, subsets: [...selected] }),
    onSuccess: (run) => {
      onStarted(run);
      onOpenChange(false);
    },
  });
  const legend = useId();

  return (
    <AlertDialog
      open={open}
      onOpenChange={(next) => {
        if (start.isPending) return;
        if (next) {
          start.reset();
          setChosen(null);
        }
        onOpenChange(next);
      }}
    >
      <AlertDialogContent>
        <AlertDialogHeader>
          <AlertDialogTitle>{t("platform_ai.eval.title")}</AlertDialogTitle>
          <AlertDialogDescription>{t("platform_ai.eval.desc")}</AlertDialogDescription>
        </AlertDialogHeader>
        {options.isPending ? (
          <Spinner label={t("app.loading")} />
        ) : options.isError ? (
          <Alert variant="destructive">
            <AlertDescription>
              <p>{errorMessage(options.error)}</p>
            </AlertDescription>
          </Alert>
        ) : (
          <>
            <fieldset aria-labelledby={legend} className="grid gap-2">
              <legend id={legend} className="mb-1 text-sm font-medium">
                {t("platform_ai.eval.subsets")}
              </legend>
              {subsets.map((subset) => (
                <label key={subset.id} className="flex items-center gap-2 text-sm">
                  <input
                    type="checkbox"
                    className="size-4 accent-primary"
                    checked={selected.has(subset.id)}
                    onChange={(event) => {
                      const next = new Set(selected);
                      if (event.target.checked) next.add(subset.id);
                      else next.delete(subset.id);
                      setChosen(next);
                    }}
                  />
                  {t("platform_ai.eval.subset", { label: subset.label, count: subset.question_count })}
                </label>
              ))}
            </fieldset>
            <p className="text-sm">
              {questions === 0
                ? t("platform_ai.eval.none_selected")
                : `${t("platform_ai.eval.summary", { questions, runs: options.data.runs_per_question })}${
                    cost === null || cost === undefined
                      ? ""
                      : ` ${t("platform_ai.eval.estimate", { cost: number(questions * cost, 0) })}`
                  }`}
            </p>
          </>
        )}
        {start.isError ? (
          <Alert variant="destructive">
            <AlertDescription>
              <p>
                {start.error instanceof ApiError && start.error.status === 409
                  ? t("platform_ai.eval.running_conflict")
                  : errorMessage(start.error)}
              </p>
            </AlertDescription>
          </Alert>
        ) : null}
        <AlertDialogFooter>
          <AlertDialogCancel disabled={start.isPending}>{t("platform_ai.cancel")}</AlertDialogCancel>
          <Button disabled={start.isPending || questions === 0 || !options.isSuccess} onClick={() => start.mutate()}>
            {start.isPending ? <Spinner /> : null}
            {start.isPending ? t("platform_ai.eval.starting") : t("platform_ai.eval.start")}
          </Button>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  );
}

function ReportTable({ report, version }: { report: EvalReport; version: number }) {
  const { t } = useI18n();
  const number = useNumber();
  const { baseline: a, candidate: b } = report;
  const cell = (text: string, word: Verdict) => `${text}（${t(VERDICT_TEXT[word])}）`;
  const accuracy = b.accuracy - a.accuracy;
  const consistency = b.consistency - a.consistency;
  const cost = b.cost_per_correct - a.cost_per_correct;
  return (
    <DataTable
      caption={t("platform_ai.eval.caption")}
      head={
        <>
          <Th>{t("platform_ai.eval.col_metric")}</Th>
          <Th align="right">{t("platform_ai.eval.col_published", { version })}</Th>
          <Th align="right">{t("platform_ai.eval.col_draft")}</Th>
          <Th align="right">{t("platform_ai.eval.col_delta")}</Th>
        </>
      }
    >
      <Row>
        <Cell label={t("platform_ai.eval.col_metric")}>{t("platform_ai.eval.m_accuracy")}</Cell>
        <Cell label={t("platform_ai.eval.col_published", { version })} align="right">
          {number(a.accuracy)}%
        </Cell>
        <Cell label={t("platform_ai.eval.col_draft")} align="right">
          {number(b.accuracy)}%
        </Cell>
        <Cell label={t("platform_ai.eval.col_delta")} align="right">
          {cell(number(accuracy, 1, true), verdict(accuracy, true))}
        </Cell>
      </Row>
      <Row>
        <Cell label={t("platform_ai.eval.col_metric")}>
          {t("platform_ai.eval.m_consistency", { runs: report.runs_per_question })}
        </Cell>
        <Cell label={t("platform_ai.eval.col_published", { version })} align="right">
          {number(a.consistency)}%
        </Cell>
        <Cell label={t("platform_ai.eval.col_draft")} align="right">
          {number(b.consistency)}%
        </Cell>
        <Cell label={t("platform_ai.eval.col_delta")} align="right">
          {cell(number(consistency, 1, true), verdict(consistency, true))}
        </Cell>
      </Row>
      <Row>
        <Cell label={t("platform_ai.eval.col_metric")}>{t("platform_ai.eval.m_cost")}</Cell>
        <Cell label={t("platform_ai.eval.col_published", { version })} align="right">
          US${number(a.cost_per_correct, 3)}
        </Cell>
        <Cell label={t("platform_ai.eval.col_draft")} align="right">
          US${number(b.cost_per_correct, 3)}
        </Cell>
        <Cell label={t("platform_ai.eval.col_delta")} align="right">
          {cell(number(cost, 3, true), verdict(cost, false))}
        </Cell>
      </Row>
      <Row>
        <Cell label={t("platform_ai.eval.col_metric")}>{t("platform_ai.eval.m_latency")}</Cell>
        <Cell label={t("platform_ai.eval.col_published", { version })} align="right">
          {t("platform_ai.eval.seconds", { p50: number(a.latency_p50), p95: number(a.latency_p95) })}
        </Cell>
        <Cell label={t("platform_ai.eval.col_draft")} align="right">
          {t("platform_ai.eval.seconds", { p50: number(b.latency_p50), p95: number(b.latency_p95) })}
        </Cell>
        <Cell label={t("platform_ai.eval.col_delta")} align="right">
          {cell(
            `${number(b.latency_p50 - a.latency_p50, 1, true)} / ${number(b.latency_p95 - a.latency_p95, 1, true)}`,
            latencyVerdict(a, b),
          )}
        </Cell>
      </Row>
    </DataTable>
  );
}

/**
 * The evaluation of a draft: its progress while it runs (only every 10 percent is announced), and
 * its report when it is done, beside the published version's numbers, with what became wrong and a
 * way to publish from it. Which run it is lives in the page (`runId`), so it survives leaving it:
 * the page asks the service for the run in progress when it opens (PromptsPage).
 */
export function EvalPanel({
  service,
  role,
  runId,
  version,
  draft,
  onPublish,
  onClose,
}: {
  service: string;
  role: string;
  runId: string;
  /** The published version the report compares against. */
  version: number;
  draft: string;
  onPublish: (run: EvalRun) => void;
  onClose: () => void;
}) {
  const { t, errorMessage, formatDateTime } = useI18n();
  const api = usePromptApi();
  const queryClient = useQueryClient();
  const run = useQuery({
    queryKey: evalRunKey(service, role, runId),
    queryFn: ({ signal }) => api.getEval(service, role, runId, signal),
    refetchInterval: (query) => (query.state.data?.state === "running" ? 1000 : false),
  });
  const cancel = useMutation({
    mutationFn: () => api.cancelEval(service, role, runId),
    onSuccess: (updated) => queryClient.setQueryData(evalRunKey(service, role, runId), updated),
  });

  if (run.isPending) return <Spinner label={t("app.loading")} />;
  if (run.isError) {
    return (
      <Alert variant="destructive">
        <AlertDescription>
          <p>{errorMessage(run.error)}</p>
          <div className="flex gap-2 pt-1">
            <Button size="sm" variant="outline" onClick={() => void run.refetch()}>
              {t("platform_ai.roles.retry")}
            </Button>
            <Button size="sm" variant="ghost" onClick={onClose}>
              {t("platform_ai.close")}
            </Button>
          </div>
        </AlertDescription>
      </Alert>
    );
  }
  const data = run.data;
  if (data.state === "running") {
    const percent = data.total === 0 ? 0 : Math.floor((data.completed / data.total) * 100);
    return (
      <Card className="gap-3 py-5">
        <CardContent className="grid gap-2">
          <div className="flex flex-wrap items-center justify-between gap-2">
            <strong aria-hidden="true">
              {t("platform_ai.eval.progress", { completed: data.completed, total: data.total })}
            </strong>
            <Button variant="ghost" size="sm" disabled={cancel.isPending} onClick={() => cancel.mutate()}>
              {t("platform_ai.eval.cancel_run")}
            </Button>
          </div>
          <p role="status" className="sr-only">
            {t("platform_ai.eval.progress_sr", { percent: Math.floor(percent / 10) * 10 })}
          </p>
          <Meter percent={percent} />
          <p className="text-xs text-muted-foreground">{t("platform_ai.eval.leave_note")}</p>
          {cancel.isError ? <p className="text-sm text-destructive">{errorMessage(cancel.error)}</p> : null}
        </CardContent>
      </Card>
    );
  }
  if (data.state !== "done" || data.report === null) {
    return (
      <Alert role="status">
        <AlertTitle>
          {data.state === "cancelled" ? t("platform_ai.eval.cancelled") : t("platform_ai.eval.failed")}
        </AlertTitle>
        <AlertDescription>
          <Button size="sm" variant="ghost" onClick={onClose}>
            {t("platform_ai.close")}
          </Button>
        </AlertDescription>
      </Alert>
    );
  }
  const report = data.report;
  return (
    <Card className="gap-4 py-5">
      <CardHeader>
        <div className="grid gap-0.5">
          <CardTitle>{t("platform_ai.eval.report_title", { version })}</CardTitle>
          <p className="text-sm text-muted-foreground">
            {t("platform_ai.eval.report_meta", {
              questions: report.question_count,
              runs: report.runs_per_question,
              time: formatDateTime(report.finished_at),
            })}
          </p>
        </div>
      </CardHeader>
      <CardContent className="grid gap-3">
        {data.content !== draft ? (
          <Alert role="status">
            <AlertDescription>
              <p>{t("platform_ai.eval.stale_report")}</p>
            </AlertDescription>
          </Alert>
        ) : null}
        <ReportTable report={report} version={version} />
        {report.newly_wrong.length === 0 ? (
          <p className="text-sm">{t("platform_ai.eval.no_newly_wrong")}</p>
        ) : (
          <div className="grid gap-1 text-sm">
            <p>{t("platform_ai.eval.newly_wrong", { count: report.newly_wrong.length })}</p>
            <ul className="grid list-disc gap-0.5 pl-5">
              {report.newly_wrong.map((question) => (
                <li key={question.id} className="wrap-anywhere">
                  {question.text}
                </li>
              ))}
            </ul>
          </div>
        )}
      </CardContent>
      <CardFooter className="justify-end">
        <Button variant="outline" onClick={onClose}>
          {t("platform_ai.eval.back_edit")}
        </Button>
        <Button onClick={() => onPublish(data)}>
          {t("platform_ai.eval.publish_from_report", { version: version + 1 })}
        </Button>
      </CardFooter>
    </Card>
  );
}
