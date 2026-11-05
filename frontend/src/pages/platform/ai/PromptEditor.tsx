import { useQuery, useQueryClient } from "@tanstack/react-query";
import { LockIcon, TriangleAlertIcon } from "lucide-react";
import { useCallback, useEffect, useId, useRef, useState } from "react";
import { Link, useBlocker, useLocation, useNavigate } from "react-router";

import { usePromptApi } from "../../../api/pending/PromptApiProvider.tsx";
import type { EvalRun, PromptDetail } from "../../../api/pending/prompts.ts";
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
import { Button, buttonVariants } from "../../../components/ui/button.tsx";
import { useI18n } from "../../../i18n/useI18n.ts";
import { cn } from "../../../lib/utils.ts";
import { DiffView } from "./DiffView.tsx";
import { unknownVariables } from "./diff.ts";
import { EvalPanel, EvalStartDialog } from "./EvalPanel.tsx";
import { PromptField } from "./PromptField.tsx";
import { PublishDialog } from "./PublishDialog.tsx";
import { promptVersionsPath } from "./paths.ts";
import { currentEvalKey, evalRunKey, promptKey, promptsKey } from "./queries.ts";
import { Loading, Skeleton } from "./ui.tsx";

/** What a prompt page can be opened with: text for the draft (the default's change applied to the published prompt). */
export interface PromptLocationState {
  prefill?: string;
}

function prefillOf(state: unknown): string | null {
  if (typeof state !== "object" || state === null || !("prefill" in state)) return null;
  return typeof state.prefill === "string" ? state.prefill : null;
}

/**
 * One prompt, as docs/design/platform-admin/ai-prompts.html draws it: the notice that the default
 * moved, the fixed sections (code: shown, never editable), the editable text with its variables
 * checked as it is typed, and the way to publish it, with or without running the question bank
 * first. The draft lives only in this tab's memory (decision Q8): leaving with a draft asks first.
 */
export function PromptEditor({ service, role }: { service: string; role: string }) {
  const { t, errorMessage, formatDateTime } = useI18n();
  const api = usePromptApi();
  const queryClient = useQueryClient();
  const location = useLocation();
  const navigate = useNavigate();
  const fieldId = useId();
  const errorId = useId();
  const textareaRef = useRef<HTMLTextAreaElement | null>(null);

  const detail = useQuery({
    queryKey: promptKey(service, role),
    queryFn: ({ signal }) => api.getPrompt(service, role, signal),
  });
  const data = detail.data;

  /** The draft, or null while nothing was edited, and the version it was edited from. */
  const [draft, setDraft] = useState<string | null>(null);
  const [base, setBase] = useState<number | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [dismissedDefault, setDismissedDefault] = useState(false);
  const [publishOpen, setPublishOpen] = useState(false);
  const [evalOpen, setEvalOpen] = useState(false);
  const [compareOpen, setCompareOpen] = useState(false);
  const [discardOpen, setDiscardOpen] = useState(false);
  const [runId, setRunId] = useState<string | null>(null);

  const edit = useCallback(
    (text: string) => {
      setDraft(text);
      setBase((current) => current ?? data?.version ?? null);
    },
    [data?.version],
  );

  // The default's change applied by the versions page: it arrives as the draft, once the prompt is read.
  const prefill = prefillOf(location.state);
  useEffect(() => {
    if (prefill === null || data === undefined) return;
    edit(prefill);
    void navigate(`${location.pathname}${location.search}`, { replace: true, state: null });
  }, [prefill, data, edit, navigate, location.pathname, location.search]);

  // An evaluation that is still running when the page opens keeps showing its progress.
  const running = useQuery({
    queryKey: currentEvalKey(service, role),
    queryFn: ({ signal }) => api.currentEval(service, role, signal),
  });
  useEffect(() => {
    if (running.data && runId === null) setRunId(running.data.id);
  }, [running.data, runId]);

  const dirty = data !== undefined && draft !== null && draft !== data.content;
  const blocker = useBlocker(
    ({ currentLocation, nextLocation }) => dirty && currentLocation.pathname !== nextLocation.pathname,
  );
  useEffect(() => {
    if (!dirty) return;
    const warn = (event: BeforeUnloadEvent) => event.preventDefault();
    window.addEventListener("beforeunload", warn);
    return () => window.removeEventListener("beforeunload", warn);
  }, [dirty]);

  if (detail.isPending) {
    return (
      <Loading label={t("platform_ai.prompts.loading")} className="grid gap-3">
        <Skeleton className="h-10" />
        <Skeleton className="h-24" />
        <Skeleton className="h-48" />
      </Loading>
    );
  }
  if (detail.isError || data === undefined) {
    return (
      <Alert variant="destructive">
        <AlertTitle>{t("platform_ai.prompts.detail_failed", { role })}</AlertTitle>
        <AlertDescription>
          <p>
            {errorMessage(detail.error)} {t("platform_ai.prompts.detail_failed_note")}
          </p>
          <Button size="sm" variant="outline" onClick={() => void detail.refetch()}>
            {t("platform_ai.roles.retry")}
          </Button>
        </AlertDescription>
      </Alert>
    );
  }

  const text = draft ?? data.content;
  const unknown = draft === null ? [] : unknownVariables(text, data.variables);
  const stale = draft !== null && base !== null && base !== data.version;
  const blocked = unknown.length > 0 || !dirty || stale;
  const discard = () => {
    setDraft(null);
    setBase(null);
  };
  const insert = (name: string) => {
    const field = textareaRef.current;
    const token = `{{${name}}}`;
    const start = field?.selectionStart ?? text.length;
    const end = field?.selectionEnd ?? text.length;
    edit(`${text.slice(0, start)}${token}${text.slice(end)}`);
    requestAnimationFrame(() => {
      field?.focus();
      field?.setSelectionRange(start + token.length, start + token.length);
    });
  };
  /** Whether the run shown is done and was run on exactly this draft. */
  const reportFor = (id: string | null, text: string): boolean => {
    if (id === null) return false;
    const run = queryClient.getQueryData<EvalRun>(evalRunKey(service, role, id));
    return run?.state === "done" && run.content === text;
  };
  const published = (next: PromptDetail) => {
    queryClient.setQueryData(promptKey(service, role), next);
    void queryClient.invalidateQueries({ queryKey: promptsKey });
    discard();
    setRunId(null);
    setNotice(t("platform_ai.prompts.published_notice", { role, version: next.version }));
  };

  return (
    <div className="grid min-w-0 gap-4">
      <div className="flex flex-wrap items-start justify-between gap-3">
        <p className="text-sm text-muted-foreground">
          {t("platform_ai.prompts.published_line", {
            version: data.version,
            time: formatDateTime(data.updated_at),
            by: data.updated_by,
          })}
        </p>
        <Link to={promptVersionsPath(service, role)} className={cn(buttonVariants({ variant: "outline", size: "sm" }))}>
          {t("platform_ai.prompts.versions_link")}
        </Link>
      </div>

      {notice ? (
        <Alert role="status" className="grid-cols-[1fr_auto] items-center gap-x-3">
          <AlertDescription className="col-start-1">
            <p className="text-foreground">{notice}</p>
          </AlertDescription>
          <Button variant="ghost" size="sm" onClick={() => setNotice(null)}>
            {t("page.action.dismiss")}
          </Button>
        </Alert>
      ) : null}

      {data.default_update && !dismissedDefault ? (
        <Alert role="status">
          <TriangleAlertIcon aria-hidden="true" className="text-warning" />
          <AlertTitle>
            {t("platform_ai.prompts.default_updated_title", { label: data.default_update.to_label })}
          </AlertTitle>
          <AlertDescription>
            <p>{t("platform_ai.prompts.default_updated_body")}</p>
            <div className="flex flex-wrap gap-2 pt-1">
              <Link
                to={`${promptVersionsPath(service, role)}#default`}
                className={cn(buttonVariants({ variant: "outline", size: "sm" }))}
              >
                {t("platform_ai.prompts.default_view")}
              </Link>
              <Button size="sm" variant="ghost" onClick={() => setDismissedDefault(true)}>
                {t("platform_ai.prompts.default_later")}
              </Button>
            </div>
          </AlertDescription>
        </Alert>
      ) : null}

      {data.fixed_sections.map((section) => (
        <section
          key={section.id}
          aria-label={t("platform_ai.prompts.fixed_title", { title: section.title })}
          className="grid gap-2 rounded-md border border-dashed bg-muted/50 p-3"
        >
          <div className="flex flex-wrap items-center justify-between gap-2 text-sm">
            <strong className="flex items-center gap-1.5">
              <LockIcon aria-hidden="true" className="size-4" />
              {t("platform_ai.prompts.fixed_title", { title: section.title })}
            </strong>
            <span className="text-xs text-muted-foreground">{t("platform_ai.prompts.fixed_note")}</span>
          </div>
          {/* Text, not a field: nothing here can be edited, and the text is printed as text. */}
          <pre className="font-mono text-sm leading-relaxed whitespace-pre-wrap wrap-anywhere text-muted-foreground">
            {section.content}
          </pre>
        </section>
      ))}

      <section className="grid gap-2" aria-labelledby={`${fieldId}-label`}>
        <div className="flex flex-wrap items-center justify-between gap-2 text-sm">
          <label id={`${fieldId}-label`} htmlFor={fieldId} className="font-semibold">
            {t("platform_ai.prompts.editable")}
          </label>
          <span className="text-xs text-muted-foreground">
            {dirty ? t("platform_ai.prompts.draft_dirty") : t("platform_ai.prompts.draft_clean")}
            {unknown.length > 0 ? ` · ${t("platform_ai.prompts.problems", { count: unknown.length })}` : ""}
          </span>
        </div>
        <PromptField
          id={fieldId}
          value={text}
          onChange={edit}
          allowed={data.variables}
          invalid={unknown.length > 0}
          describedBy={unknown.length > 0 ? errorId : undefined}
          textareaRef={textareaRef}
        />
        {unknown.length > 0 ? (
          <ul id={errorId} role="alert" className="grid gap-0.5 text-sm font-medium text-destructive">
            {unknown.map((variable) => (
              <li key={variable.start}>
                {t("platform_ai.prompts.var_error", { variable: variable.raw, line: variable.line })}
              </li>
            ))}
          </ul>
        ) : null}
      </section>

      <div className="grid gap-1.5">
        <div className="text-sm font-medium">{t("platform_ai.prompts.vars_title")}</div>
        <div className="flex flex-wrap gap-1.5">
          {data.variables.map((name) => (
            <Button key={name} variant="outline" size="sm" className="font-mono" onClick={() => insert(name)}>
              {`{{${name}}}`}
            </Button>
          ))}
        </div>
        <p className="text-xs text-muted-foreground">{t("platform_ai.prompts.vars_note")}</p>
      </div>

      {stale ? (
        <Alert role="alert">
          <TriangleAlertIcon aria-hidden="true" className="text-warning" />
          <AlertTitle>
            {t("platform_ai.prompts.stale_title", { by: data.updated_by, version: data.version })}
          </AlertTitle>
          <AlertDescription>
            <p>{t("platform_ai.prompts.stale_body", { base: base ?? 0, version: data.version })}</p>
            <Button size="sm" onClick={() => setCompareOpen(true)}>
              {t("platform_ai.prompts.stale_compare", { version: data.version })}
            </Button>
          </AlertDescription>
        </Alert>
      ) : null}

      <div className="flex flex-wrap items-center justify-between gap-2 border-t pt-4">
        <Button variant="ghost" disabled={draft === null} onClick={() => (dirty ? setDiscardOpen(true) : discard())}>
          {t("platform_ai.prompts.discard")}
        </Button>
        <div className="flex flex-wrap gap-2">
          <Button
            variant="outline"
            disabled={blocked || runId !== null}
            {...(unknown.length > 0 ? { "aria-describedby": errorId } : {})}
            onClick={() => setEvalOpen(true)}
          >
            {t("platform_ai.prompts.evaluate")}
          </Button>
          <Button
            disabled={blocked}
            {...(unknown.length > 0 ? { "aria-describedby": errorId } : {})}
            onClick={() => setPublishOpen(true)}
          >
            {t("platform_ai.prompts.publish")}
          </Button>
        </div>
      </div>

      {runId !== null ? (
        <EvalPanel
          service={service}
          role={role}
          runId={runId}
          version={data.version}
          draft={text}
          onClose={() => setRunId(null)}
          onPublish={() => setPublishOpen(true)}
        />
      ) : null}

      {draft !== null && base !== null ? (
        <PublishDialog
          open={publishOpen}
          onOpenChange={setPublishOpen}
          service={service}
          role={role}
          detail={data}
          draft={draft}
          baseVersion={base}
          evaluated={reportFor(runId, draft)}
          onEvaluate={() => setEvalOpen(true)}
          onPublished={published}
          onConflict={() => void detail.refetch()}
        />
      ) : null}
      <EvalStartDialog
        open={evalOpen}
        onOpenChange={setEvalOpen}
        service={service}
        role={role}
        content={text}
        onStarted={(run) => setRunId(run.id)}
      />

      <AlertDialog open={compareOpen} onOpenChange={setCompareOpen}>
        <AlertDialogContent className="max-h-[calc(100dvh-2rem)] overflow-y-auto sm:max-w-2xl">
          <AlertDialogHeader>
            <AlertDialogTitle>{t("platform_ai.prompts.compare_title", { version: data.version })}</AlertDialogTitle>
            <AlertDialogDescription>
              {t("platform_ai.prompts.compare_desc", { version: data.version })}
            </AlertDialogDescription>
          </AlertDialogHeader>
          <DiffView
            label={t("platform_ai.prompts.compare_title", { version: data.version })}
            before={data.content}
            after={text}
            beforeLabel={t("platform_ai.publish.published", { version: data.version })}
            afterLabel={t("platform_ai.publish.draft")}
          />
          <AlertDialogFooter>
            <AlertDialogCancel>{t("platform_ai.close")}</AlertDialogCancel>
            <Button
              onClick={() => {
                setBase(data.version);
                setCompareOpen(false);
              }}
            >
              {t("platform_ai.prompts.stale_rebase", { version: data.version })}
            </Button>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>

      <AlertDialog open={discardOpen} onOpenChange={setDiscardOpen}>
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>{t("platform_ai.prompts.leave_title", { role })}</AlertDialogTitle>
            <AlertDialogDescription>{t("platform_ai.prompts.leave_body")}</AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogCancel>{t("platform_ai.prompts.leave_stay")}</AlertDialogCancel>
            <Button
              variant="destructive"
              onClick={() => {
                discard();
                setDiscardOpen(false);
              }}
            >
              {t("platform_ai.prompts.discard_confirm")}
            </Button>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>

      <AlertDialog
        open={blocker.state === "blocked"}
        onOpenChange={(open) => {
          if (!open && blocker.state === "blocked") blocker.reset();
        }}
      >
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>{t("platform_ai.prompts.leave_title", { role })}</AlertDialogTitle>
            <AlertDialogDescription>{t("platform_ai.prompts.leave_body")}</AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogCancel onClick={() => blocker.state === "blocked" && blocker.reset()}>
              {t("platform_ai.prompts.leave_stay")}
            </AlertDialogCancel>
            <Button variant="destructive" onClick={() => blocker.state === "blocked" && blocker.proceed()}>
              {t("platform_ai.prompts.leave_discard")}
            </Button>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
    </div>
  );
}
