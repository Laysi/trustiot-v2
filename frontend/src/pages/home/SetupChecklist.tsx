import { CheckIcon, ChevronDownIcon, ChevronUpIcon, CircleAlertIcon, RefreshCwIcon, XIcon } from "lucide-react";
import { useEffect, useId, useLayoutEffect, useRef, useState } from "react";

import { type SetupApi, type SetupState, useSetupApi } from "../../api/pending/setup.ts";
import { Alert, AlertDescription, AlertTitle } from "../../components/ui/alert.tsx";
import { Button } from "../../components/ui/button.tsx";
import { Card } from "../../components/ui/card.tsx";
import { useI18n } from "../../i18n/useI18n.ts";
import { useMediaQuery } from "../../lib/use-media-query.ts";
import { cn } from "../../lib/utils.ts";
import { useSession } from "../../session/SessionProvider.tsx";
import { tenantName } from "../../session/tenant-name.ts";
import { SetupStepList } from "./SetupStepList.tsx";
import { currentStep, useSetupAction, useSetupState } from "./setup-queries.ts";
import { STEP_TEXT } from "./setup-steps.ts";
import { useRouteExists } from "./useRouteExists.ts";

/** After this many failed reads in a row the checklist stays away until the page is opened again. */
const GIVE_UP_AFTER = 3;

/** The width from which the whole list shows; below it the checklist is the short version (decision 13). */
const WIDE = "(min-width: 768px)";

function collapsedKey(userId: string, tenantId: string): string {
  return `trustiot.setup.collapsed.${userId}.${tenantId}`;
}

/** The collapse is the person's own view, kept in this browser; storage that is blocked just forgets it. */
function readCollapsed(key: string): boolean {
  try {
    return window.localStorage.getItem(key) === "1";
  } catch {
    return false;
  }
}

function writeCollapsed(key: string, collapsed: boolean): void {
  try {
    if (collapsed) window.localStorage.setItem(key, "1");
    else window.localStorage.removeItem(key);
  } catch {
    // Not kept: the checklist is open again next time.
  }
}

/**
 * The new tenant's setup checklist (Laysi/trustiot-v2#129, docs/design/home-ai/design.md): five
 * steps, each completed by the service that owns it, each skippable, and gone when every one is
 * done or skipped. Only a tenant admin sees it (decision 1), and only where the setup endpoints
 * are provided (`useSetupApi`): the production build has none yet, so it draws nothing there.
 * It sits above the greeting on the home page and above the overview on the generic home.
 */
export function SetupChecklist() {
  const { t } = useI18n();
  const api = useSetupApi();
  const { session } = useSession();
  const tenant = session?.activeTenant;
  if (api === null || session === null || tenant === null || tenant === undefined || tenant.role !== "admin") {
    return null;
  }
  return <Checklist api={api} tenantId={tenant.id} userId={session.user.id} tenantLabel={tenantName(tenant, t)} />;
}

function Checklist({
  api,
  tenantId,
  userId,
  tenantLabel,
}: {
  api: SetupApi;
  tenantId: string;
  userId: string;
  tenantLabel: string;
}) {
  const i18n = useI18n();
  const { t, errorMessage } = i18n;
  const setup = useSetupState(api, tenantId);
  const action = useSetupAction(api, tenantId);
  const routeExists = useRouteExists();
  const wide = useMediaQuery(WIDE);
  const listId = useId();
  const key = collapsedKey(userId, tenantId);
  const [collapsed, setCollapsed] = useState(() => readCollapsed(key));
  const [showAll, setShowAll] = useState(false);

  // The current step's main action, and the closing notice's close button: where focus goes after
  // a skip or a restore (the design's 「略過與復原」), and after the last step leaves the list.
  const actionRef = useRef<HTMLElement | null>(null);
  const closeRef = useRef<HTMLButtonElement | null>(null);
  const moveFocus = useRef(false);

  const state = setup.data;
  useLayoutEffect(() => {
    if (!moveFocus.current || state === undefined) return;
    moveFocus.current = false;
    (actionRef.current ?? closeRef.current)?.focus();
  }, [state]);

  // A step that turns done while the page is open (the first reading arriving) is said once, politely.
  const done = useRef<ReadonlySet<string> | null>(null);
  const [announcement, setAnnouncement] = useState("");
  useEffect(() => {
    if (state === undefined) return;
    const now = new Set(state.steps.filter((step) => step.status === "done").map((step) => step.id));
    const before = done.current;
    done.current = now;
    if (before === null) return;
    const finished = state.steps.find((step) => now.has(step.id) && !before.has(step.id));
    if (finished) setAnnouncement(t("setup.announce_done", { step: t(STEP_TEXT[finished.id].title) }));
  }, [state, t]);

  if (state === undefined) {
    if (setup.isError && setup.errorUpdateCount < GIVE_UP_AFTER) {
      return (
        <Alert variant="destructive" className="grid-cols-[1rem_1fr_auto] items-center gap-x-3">
          <CircleAlertIcon aria-hidden="true" />
          <div className="grid gap-0.5">
            <AlertTitle className="col-start-1">{t("setup.load_failed.title")}</AlertTitle>
            <AlertDescription className="col-start-1">
              <p>{t("setup.load_failed.body", { reason: errorMessage(setup.error) })}</p>
            </AlertDescription>
          </div>
          <Button variant="outline" size="sm" onClick={() => void setup.refetch()} disabled={setup.isFetching}>
            <RefreshCwIcon aria-hidden="true" />
            {t("page.action.retry")}
          </Button>
        </Alert>
      );
    }
    // Loading draws nothing, so the page does not jump for a checklist that is gone; a read that failed
    // again and again is given up, and the next visit tries once more.
    return null;
  }

  const finished = state.steps.every((step) => step.status !== "todo");
  if (finished) {
    if (state.completion_dismissed) return null;
    return (
      <DoneBanner
        state={state}
        i18n={i18n}
        closeRef={closeRef}
        busy={action.isPending}
        failed={action.isError && action.variables?.kind === "dismiss" ? errorMessage(action.error) : undefined}
        onClose={() => action.mutate({ kind: "dismiss" })}
      />
    );
  }

  const total = state.steps.length;
  const doneCount = state.steps.filter((step) => step.status === "done").length;
  const skippedCount = state.steps.filter((step) => step.status === "skipped").length;
  const progress =
    skippedCount > 0
      ? t("setup.progress_skipped", { done: doneCount, total, skipped: skippedCount })
      : t("setup.progress", { done: doneCount, total });
  const current = currentStep(state);

  const failure =
    action.isError && action.variables && action.variables.kind !== "dismiss"
      ? {
          kind: action.variables.kind,
          step: action.variables.kind === "choose" ? ("first_use" as const) : action.variables.step,
          message: t(
            action.variables.kind === "skip"
              ? "setup.skip_failed"
              : action.variables.kind === "restore"
                ? "setup.restore_failed"
                : "setup.choose_failed",
            { reason: errorMessage(action.error) },
          ),
        }
      : undefined;

  const collapsedView = wide && collapsed;
  const fullList = wide || showAll;
  const toggleId = `${listId}-steps`;

  return (
    <Card aria-labelledby={`${listId}-title`} className="gap-3 py-4">
      <div className="flex flex-wrap items-start justify-between gap-x-4 gap-y-2 px-4">
        <div className="grid min-w-0 gap-0.5">
          <h2 id={`${listId}-title`} className="text-base font-semibold">
            {t("setup.title")}
          </h2>
          <p className="text-sm text-muted-foreground">
            {progress}
            {wide ? t("setup.skip_hint") : ""}
          </p>
        </div>
        <Button
          variant="ghost"
          size="sm"
          aria-expanded={wide ? !collapsed : showAll}
          aria-controls={toggleId}
          onClick={() => {
            if (wide) {
              writeCollapsed(key, !collapsed);
              setCollapsed(!collapsed);
            } else {
              setShowAll(!showAll);
            }
          }}
        >
          {wide
            ? collapsed
              ? t("setup.expand")
              : t("setup.collapse")
            : showAll
              ? t("setup.fewer")
              : t("setup.all_steps")}
          {(wide ? collapsed : !showAll) ? (
            <ChevronDownIcon aria-hidden="true" />
          ) : (
            <ChevronUpIcon aria-hidden="true" />
          )}
        </Button>
      </div>
      <div aria-hidden="true" className="flex gap-1 px-4">
        {state.steps.map((step) => (
          <span
            key={step.id}
            className={cn(
              "h-1.5 flex-1 rounded-full",
              step.status === "done" && "bg-primary",
              step.status === "skipped" && "border border-dashed border-muted-foreground",
              step.status === "todo" && (step.id === current?.id ? "bg-primary/40" : "bg-muted"),
            )}
          />
        ))}
      </div>
      <div id={toggleId} hidden={collapsedView} className="px-1">
        {collapsedView ? null : (
          <SetupStepList
            state={state}
            i18n={i18n}
            id={`${listId}-list`}
            only={fullList ? undefined : current?.id}
            tenantName={tenantLabel}
            routeExists={routeExists}
            busy={action.isPending}
            failure={failure}
            onSkip={(step) => {
              moveFocus.current = true;
              action.mutate({ kind: "skip", step });
            }}
            onRestore={(step) => {
              moveFocus.current = true;
              action.mutate({ kind: "restore", step });
            }}
            onChoose={(use) => action.mutate({ kind: "choose", use })}
            actionRef={actionRef}
          />
        )}
      </div>
      <p role="status" className="sr-only">
        {announcement}
      </p>
    </Card>
  );
}

/** The one-time notice when every step is done or skipped (decision 3); closing it is the tenant's. */
function DoneBanner({
  state,
  i18n,
  closeRef,
  busy,
  failed,
  onClose,
}: {
  state: SetupState;
  i18n: ReturnType<typeof useI18n>;
  closeRef: React.Ref<HTMLButtonElement>;
  busy: boolean;
  failed: string | undefined;
  onClose(): void;
}) {
  const { t } = i18n;
  const id = useId();
  const doneCount = state.steps.filter((step) => step.status === "done").length;
  const skipped = state.steps.filter((step) => step.status === "skipped");
  return (
    <Card aria-labelledby={`${id}-title`} className="gap-1 px-4 py-3">
      <div role="status" className="flex items-start gap-3">
        <span
          aria-hidden="true"
          className="mt-0.5 grid size-6 shrink-0 place-items-center rounded-full bg-primary text-primary-foreground"
        >
          <CheckIcon className="size-3.5" />
        </span>
        <div className="min-w-0 flex-1">
          <h2 id={`${id}-title`} className="text-sm font-semibold">
            {t("setup.done.title")}
          </h2>
          <p className="text-xs text-muted-foreground">
            {skipped.length === 0
              ? t("setup.done.body", { done: doneCount })
              : t("setup.done.body_skipped", {
                  done: doneCount,
                  skipped: skipped.length,
                  steps: skipped.map((step) => t(STEP_TEXT[step.id].title)).join(t("setup.done.separator")),
                })}
          </p>
        </div>
        <Button
          variant="ghost"
          size="icon"
          ref={closeRef}
          disabled={busy}
          aria-label={t("setup.done.close")}
          onClick={onClose}
        >
          <XIcon aria-hidden="true" />
        </Button>
      </div>
      {failed ? (
        <p role="alert" className="flex items-start gap-2 text-sm text-destructive">
          <CircleAlertIcon aria-hidden="true" className="mt-0.5 size-4 shrink-0" />
          <span>{t("setup.done.close_failed", { reason: failed })}</span>
        </p>
      ) : null}
    </Card>
  );
}
