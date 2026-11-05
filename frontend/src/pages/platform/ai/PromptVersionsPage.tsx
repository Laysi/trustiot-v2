import { useInfiniteQuery, useMutation, useQueries, useQuery, useQueryClient } from "@tanstack/react-query";
import { FileClockIcon } from "lucide-react";
import { useEffect, useState } from "react";
import { Link, useLocation, useNavigate, useParams } from "react-router";
import { ApiError } from "../../../api/errors.ts";
import { usePromptApi } from "../../../api/pending/PromptApiProvider.tsx";
import type { PromptDetail, PromptVersion } from "../../../api/pending/prompts.ts";
import { EmptyState } from "../../../components/EmptyState.tsx";
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
import { Card, CardContent, CardFooter, CardHeader, CardTitle } from "../../../components/ui/card.tsx";
import { FormField } from "../../../components/ui/form-field.tsx";
import { Input } from "../../../components/ui/input.tsx";
import { Spinner } from "../../../components/ui/spinner.tsx";
import { useI18n } from "../../../i18n/useI18n.ts";
import { useMediaQuery } from "../../../lib/use-media-query.ts";
import { cn } from "../../../lib/utils.ts";
import { PageHeader } from "../../dashboards/PageHeader.tsx";
import { DiffView } from "./DiffView.tsx";
import { applyDefaultChange } from "./diff.ts";
import { NOTE_MAX } from "./PublishDialog.tsx";
import { promptPath } from "./paths.ts";
import { promptKey, promptsKey, versionKey, versionsKey } from "./queries.ts";
import { AuditNote, Badge, Cell, DataTable, Loading, Row, Skeleton, Th } from "./ui.tsx";

/** How many versions can be compared at once. */
const COMPARED = 2;

function sourceText(t: ReturnType<typeof useI18n>["t"], version: PromptVersion): string {
  const source = version.source ?? "";
  switch (version.origin) {
    case "edit":
      return version.evaluated ? t("platform_ai.versions.src_edit_evaluated") : t("platform_ai.versions.src_edit");
    case "revert":
      return t("platform_ai.versions.src_revert", { source });
    case "apply_default":
      return t("platform_ai.versions.src_apply", { source });
    case "default":
      return t("platform_ai.versions.src_default", { source });
  }
}

function RevertDialog({
  service,
  role,
  target,
  detail,
  onDone,
  onClose,
}: {
  service: string;
  role: string;
  target: number;
  detail: PromptDetail;
  onDone: (message: string) => void;
  onClose: () => void;
}) {
  const { t, errorMessage } = useI18n();
  const api = usePromptApi();
  const queryClient = useQueryClient();
  const [reason, setReason] = useState("");
  const [touched, setTouched] = useState(false);
  const content = useQuery({
    queryKey: versionKey(service, role, target),
    queryFn: ({ signal }) => api.getVersion(service, role, target, signal),
  });
  const next = detail.version + 1;
  const revert = useMutation({
    mutationFn: () =>
      api.revert(service, role, { to_version: target, reason: reason.trim(), expected_version: detail.version }),
    onSuccess: (updated) => {
      queryClient.setQueryData(promptKey(service, role), updated);
      void queryClient.invalidateQueries({ queryKey: versionsKey(service, role) });
      void queryClient.invalidateQueries({ queryKey: promptsKey });
      onDone(t("platform_ai.versions.revert_notice", { role, version: target, next: updated.version }));
    },
    onError: (error) => {
      if (error instanceof ApiError && error.status === 409) {
        void queryClient.invalidateQueries({ queryKey: promptKey(service, role) });
        void queryClient.invalidateQueries({ queryKey: versionsKey(service, role) });
      }
    },
  });
  const missing = reason.trim() === "";

  return (
    <AlertDialog
      open
      onOpenChange={(open) => {
        if (!open && !revert.isPending) onClose();
      }}
    >
      <AlertDialogContent className="max-h-[calc(100dvh-2rem)] overflow-y-auto sm:max-w-2xl">
        <AlertDialogHeader>
          <AlertDialogTitle>{t("platform_ai.versions.revert_title", { role, version: target })}</AlertDialogTitle>
          <AlertDialogDescription>
            {t("platform_ai.versions.revert_desc", { version: target, next })}
          </AlertDialogDescription>
        </AlertDialogHeader>
        {content.isPending ? (
          <Spinner label={t("app.loading")} />
        ) : content.isError ? (
          <Alert variant="destructive">
            <AlertDescription>
              <p>{errorMessage(content.error)}</p>
            </AlertDescription>
          </Alert>
        ) : (
          <DiffView
            label={t("platform_ai.versions.revert_diff_label")}
            before={detail.content}
            after={content.data.content}
            beforeLabel={t("platform_ai.versions.revert_now", { version: detail.version })}
            afterLabel={t("platform_ai.versions.revert_after", { next, version: target })}
          />
        )}
        <FormField
          label={t("platform_ai.versions.revert_reason")}
          error={touched && missing ? t("platform_ai.versions.revert_reason_required") : undefined}
        >
          {(control) => (
            <Input
              {...control}
              required
              value={reason}
              maxLength={NOTE_MAX}
              onChange={(event) => setReason(event.target.value)}
              onBlur={() => setTouched(true)}
            />
          )}
        </FormField>
        <AuditNote>{t("platform_ai.versions.revert_audit", { version: target, next })}</AuditNote>
        {revert.isError ? (
          <Alert variant="destructive">
            <AlertDescription>
              <p>
                {revert.error instanceof ApiError && revert.error.status === 409
                  ? t("platform_ai.versions.revert_conflict")
                  : errorMessage(revert.error)}
              </p>
            </AlertDescription>
          </Alert>
        ) : null}
        <AlertDialogFooter>
          <AlertDialogCancel disabled={revert.isPending}>{t("platform_ai.cancel")}</AlertDialogCancel>
          <Button
            disabled={revert.isPending || !content.isSuccess}
            onClick={() => {
              setTouched(true);
              if (!missing) revert.mutate();
            }}
          >
            {revert.isPending ? <Spinner /> : null}
            {revert.isPending
              ? t("platform_ai.versions.revert_pending")
              : t("platform_ai.versions.revert_submit", { version: target, next })}
          </Button>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  );
}

function UseDefaultDialog({
  service,
  role,
  detail,
  onDone,
  onClose,
}: {
  service: string;
  role: string;
  detail: PromptDetail;
  onDone: (message: string) => void;
  onClose: () => void;
}) {
  const { t, errorMessage } = useI18n();
  const api = usePromptApi();
  const queryClient = useQueryClient();
  const update = detail.default_update;
  const next = detail.version + 1;
  const adopt = useMutation({
    mutationFn: () => api.adoptDefault(service, role, detail.version),
    onSuccess: (updated) => {
      queryClient.setQueryData(promptKey(service, role), updated);
      void queryClient.invalidateQueries({ queryKey: versionsKey(service, role) });
      void queryClient.invalidateQueries({ queryKey: promptsKey });
      onDone(t("platform_ai.versions.use_notice", { label: update?.to_label ?? "", version: updated.version }));
    },
    onError: (error) => {
      if (error instanceof ApiError && error.status === 409) {
        void queryClient.invalidateQueries({ queryKey: promptKey(service, role) });
      }
    },
  });
  if (update === null) return null;
  return (
    <AlertDialog
      open
      onOpenChange={(open) => {
        if (!open && !adopt.isPending) onClose();
      }}
    >
      <AlertDialogContent className="max-h-[calc(100dvh-2rem)] overflow-y-auto sm:max-w-2xl">
        <AlertDialogHeader>
          <AlertDialogTitle>{t("platform_ai.versions.use_title", { label: update.to_label })}</AlertDialogTitle>
          <AlertDialogDescription>
            {t("platform_ai.versions.use_desc", { label: update.to_label, next })}
          </AlertDialogDescription>
        </AlertDialogHeader>
        <DiffView
          label={t("platform_ai.versions.use_diff_label")}
          before={detail.content}
          after={update.to_content}
          beforeLabel={t("platform_ai.versions.revert_now", { version: detail.version })}
          afterLabel={t("platform_ai.versions.default_label", { label: update.to_label })}
        />
        <AuditNote>{t("platform_ai.versions.use_audit", { label: update.to_label, next })}</AuditNote>
        {adopt.isError ? (
          <Alert variant="destructive">
            <AlertDescription>
              <p>{errorMessage(adopt.error)}</p>
            </AlertDescription>
          </Alert>
        ) : null}
        <AlertDialogFooter>
          <AlertDialogCancel disabled={adopt.isPending}>{t("platform_ai.cancel")}</AlertDialogCancel>
          <Button disabled={adopt.isPending} onClick={() => adopt.mutate()}>
            {adopt.isPending ? <Spinner /> : null}
            {adopt.isPending ? t("platform_ai.versions.use_pending") : t("platform_ai.versions.use_submit", { next })}
          </Button>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  );
}

/**
 * A prompt's versions (`/platform/ai/prompts/:service/:role/versions`; design:
 * docs/design/platform-admin/ai-prompt-versions.html): every publish, revert and applied default
 * is a version and none is deleted; any two can be compared, an older one can be reverted to (the
 * reason is required: decision Q9), and when the default has moved the page shows what it changed
 * and the three things to do about it.
 */
export function PromptVersionsPage() {
  const { t, errorMessage, formatDateTime } = useI18n();
  const api = usePromptApi();
  const { service = "", role = "" } = useParams();
  const location = useLocation();
  const navigate = useNavigate();
  const wide = useMediaQuery("(min-width: 768px)");
  const [picked, setPicked] = useState<number[]>([]);
  const [view, setView] = useState<"split" | "merged" | null>(null);
  const [reverting, setReverting] = useState<number | null>(null);
  const [usingDefault, setUsingDefault] = useState(false);
  const [dismissedDefault, setDismissedDefault] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);

  const detail = useQuery({
    queryKey: promptKey(service, role),
    queryFn: ({ signal }) => api.getPrompt(service, role, signal),
  });
  const versions = useInfiniteQuery({
    queryKey: versionsKey(service, role),
    queryFn: ({ pageParam, signal }) => api.listVersions(service, role, pageParam, signal),
    initialPageParam: undefined as string | undefined,
    getNextPageParam: (last) => last.next_cursor,
  });
  const all = versions.data?.pages.flatMap((page) => page.versions) ?? [];

  const [older, newer] = [...picked].sort((a, b) => a - b);
  const contents = useQueries({
    queries: [older, newer].map((version) => ({
      queryKey: versionKey(service, role, version ?? 0),
      queryFn: ({ signal }: { signal: AbortSignal }) => api.getVersion(service, role, version ?? 0, signal),
      enabled: picked.length === COMPARED && version !== undefined,
    })),
  });

  useEffect(() => {
    if (location.hash === "#default" && detail.data?.default_update) {
      document.getElementById("default")?.scrollIntoView?.();
    }
  }, [location.hash, detail.data?.default_update]);

  const toggle = (version: number) =>
    setPicked((current) =>
      current.includes(version) ? current.filter((item) => item !== version) : [...current, version].slice(-COMPARED),
    );
  const done = (message: string) => {
    setNotice(message);
    setReverting(null);
    setUsingDefault(false);
    setPicked([]);
  };

  const title = t("platform_ai.versions.title", { role });
  const crumbs = [{ label: t("platform_ai.prompts.title"), to: promptPath(service, role) }];
  if (detail.isPending || versions.isPending) {
    return (
      <div className="grid gap-6">
        <PageHeader title={title} crumbs={crumbs} />
        <Loading label={t("platform_ai.prompts.loading")} className="grid gap-3">
          <Skeleton className="h-10" />
          <Skeleton className="h-40" />
        </Loading>
      </div>
    );
  }
  if (detail.isError || versions.isError || detail.data === undefined) {
    return (
      <div className="grid gap-6">
        <PageHeader title={title} crumbs={crumbs} />
        <Alert variant="destructive">
          <AlertTitle>{t("platform_ai.versions.load_failed")}</AlertTitle>
          <AlertDescription>
            <p>{errorMessage(detail.error ?? versions.error)}</p>
            <Button
              size="sm"
              variant="outline"
              onClick={() => {
                void detail.refetch();
                void versions.refetch();
              }}
            >
              {t("platform_ai.roles.retry")}
            </Button>
          </AlertDescription>
        </Alert>
      </div>
    );
  }
  const current = detail.data;
  const update = current.default_update;
  const [first, second] = contents;
  const mode = view ?? (wide ? "split" : "merged");

  return (
    <div className="grid gap-6">
      <PageHeader title={title} meta={t("platform_ai.versions.intro")} crumbs={crumbs} />
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

      {all.length <= 1 && all[0]?.origin === "default" ? (
        <Card>
          <EmptyState
            centered
            icon={<FileClockIcon />}
            title={t("platform_ai.versions.only_default_title", { role })}
            action={
              <Link to={promptPath(service, role)} className={cn(buttonVariants({ variant: "outline", size: "sm" }))}>
                {t("platform_ai.versions.edit_link", { role })}
              </Link>
            }
          >
            <p>{t("platform_ai.versions.only_default_body")}</p>
          </EmptyState>
        </Card>
      ) : (
        <>
          <DataTable
            caption={t("platform_ai.versions.caption")}
            head={
              <>
                <Th>
                  <span className="sr-only">{t("platform_ai.versions.col_compare")}</span>
                </Th>
                <Th>{t("platform_ai.versions.col_version")}</Th>
                <Th>{t("platform_ai.versions.col_source")}</Th>
                <Th>{t("platform_ai.versions.col_by")}</Th>
                <Th>{t("platform_ai.versions.col_note")}</Th>
                <Th>
                  <span className="sr-only">{t("platform_ai.versions.col_actions")}</span>
                </Th>
              </>
            }
          >
            {all.map((version) => {
              const active = version.version === current.version;
              return (
                <Row key={version.version} className={picked.includes(version.version) ? "bg-accent/50" : undefined}>
                  <Cell label={t("platform_ai.versions.col_compare")}>
                    <input
                      type="checkbox"
                      className="size-4 accent-primary"
                      checked={picked.includes(version.version)}
                      aria-label={t("platform_ai.versions.pick", { version: version.version })}
                      onChange={() => toggle(version.version)}
                    />
                  </Cell>
                  <Cell label={t("platform_ai.versions.col_version")}>
                    <span className="inline-flex flex-wrap items-center gap-1.5">
                      <strong>v{version.version}</strong>
                      {active ? <Badge tone="success">{t("platform_ai.versions.active")}</Badge> : null}
                    </span>
                  </Cell>
                  <Cell label={t("platform_ai.versions.col_source")}>{sourceText(t, version)}</Cell>
                  <Cell label={t("platform_ai.versions.col_by")}>
                    <span className="grid">
                      {version.created_by}
                      <span className="text-xs text-muted-foreground">{formatDateTime(version.created_at)}</span>
                    </span>
                  </Cell>
                  <Cell label={t("platform_ai.versions.col_note")}>
                    {version.note ?? <span className="text-muted-foreground">—</span>}
                  </Cell>
                  <Cell label={t("platform_ai.versions.col_actions")}>
                    {active ? null : (
                      <Button variant="outline" size="sm" onClick={() => setReverting(version.version)}>
                        {t("platform_ai.versions.revert_btn")}
                      </Button>
                    )}
                  </Cell>
                </Row>
              );
            })}
          </DataTable>
          <div className="flex flex-wrap items-center justify-between gap-2 text-sm text-muted-foreground">
            <span>
              {picked.length === COMPARED && older !== undefined && newer !== undefined
                ? t("platform_ai.versions.selected_two", { a: older, b: newer })
                : t("platform_ai.versions.select_hint")}
            </span>
            {versions.hasNextPage ? (
              <Button
                variant="outline"
                size="sm"
                disabled={versions.isFetchingNextPage}
                onClick={() => void versions.fetchNextPage()}
              >
                {t("platform_ai.versions.load_more")}
              </Button>
            ) : null}
          </div>
        </>
      )}

      {picked.length === COMPARED && older !== undefined && newer !== undefined ? (
        <section
          className="grid gap-2"
          aria-label={t("platform_ai.versions.compare_title", { from: older, to: newer })}
        >
          <div className="flex flex-wrap items-center justify-between gap-2">
            <h2 className="font-semibold">{t("platform_ai.versions.compare_title", { from: older, to: newer })}</h2>
            <fieldset
              aria-label={t("platform_ai.versions.view_label")}
              className="m-0 inline-flex min-w-0 rounded-md border p-0.5"
            >
              {(["split", "merged"] as const).map((option) => (
                <Button
                  key={option}
                  size="sm"
                  variant={mode === option ? "secondary" : "ghost"}
                  aria-pressed={mode === option}
                  onClick={() => setView(option)}
                >
                  {t(option === "split" ? "platform_ai.versions.view_split" : "platform_ai.versions.view_merged")}
                </Button>
              ))}
            </fieldset>
          </div>
          {first?.isPending || second?.isPending ? (
            <Spinner label={t("platform_ai.versions.compare_loading")} />
          ) : first?.isError || second?.isError ? (
            <Alert variant="destructive">
              <AlertDescription>
                <p>{errorMessage(first?.error ?? second?.error)}</p>
              </AlertDescription>
            </Alert>
          ) : first?.data && second?.data ? (
            <DiffView
              label={t("platform_ai.versions.compare_title", { from: older, to: newer })}
              before={first.data.content}
              after={second.data.content}
              beforeLabel={`v${older}`}
              afterLabel={newer === current.version ? `v${newer} (${t("platform_ai.versions.active")})` : `v${newer}`}
              mode={mode}
            />
          ) : null}
        </section>
      ) : null}

      {update && !dismissedDefault ? (
        <Card id="default" className="gap-4 py-5">
          <CardHeader>
            <div className="grid gap-0.5">
              <CardTitle>{t("platform_ai.versions.default_title", { label: update.to_label })}</CardTitle>
              <p className="text-sm text-muted-foreground">
                {t("platform_ai.versions.default_desc", {
                  version: current.version,
                  from: update.from_label,
                  to: update.to_label,
                })}
              </p>
            </div>
          </CardHeader>
          <CardContent>
            <DiffView
              label={t("platform_ai.versions.default_diff_label")}
              before={update.from_content}
              after={update.to_content}
              beforeLabel={t("platform_ai.versions.default_label", { label: update.from_label })}
              afterLabel={t("platform_ai.versions.default_label", { label: update.to_label })}
            />
          </CardContent>
          <CardFooter className="flex-wrap justify-end">
            <Button variant="ghost" onClick={() => setDismissedDefault(true)}>
              {t("platform_ai.versions.default_later")}
            </Button>
            <Button
              variant="outline"
              onClick={() =>
                void navigate(promptPath(service, role), {
                  state: { prefill: applyDefaultChange(current.content, update.from_content, update.to_content) },
                })
              }
            >
              {t("platform_ai.versions.default_merge")}
            </Button>
            <Button onClick={() => setUsingDefault(true)}>{t("platform_ai.versions.default_use")}</Button>
          </CardFooter>
        </Card>
      ) : null}

      {reverting !== null ? (
        <RevertDialog
          service={service}
          role={role}
          target={reverting}
          detail={current}
          onDone={done}
          onClose={() => setReverting(null)}
        />
      ) : null}
      {usingDefault ? (
        <UseDefaultDialog
          service={service}
          role={role}
          detail={current}
          onDone={done}
          onClose={() => setUsingDefault(false)}
        />
      ) : null}
    </div>
  );
}
