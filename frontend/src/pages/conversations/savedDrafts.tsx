import { useQuery, useQueryClient } from "@tanstack/react-query";
import { CircleAlertIcon, CircleCheckIcon, UploadIcon } from "lucide-react";
import { createContext, type ReactNode, useCallback, useContext, useEffect, useMemo, useRef } from "react";
import { Link } from "react-router";

import type { VersionCreated } from "../../api/dashboards.ts";
import { ApiError } from "../../api/errors.ts";
import { dashboardPath } from "../../app/paths.ts";
import { Alert, AlertDescription, AlertTitle } from "../../components/ui/alert.tsx";
import { Button } from "../../components/ui/button.tsx";
import { Spinner } from "../../components/ui/spinner.tsx";
import { type I18n, useI18n } from "../../i18n/useI18n.ts";
import { useSession } from "../../session/SessionProvider.tsx";
import { usePublishDraft } from "./drafts.ts";

// Saving a draft version to the dashboard list is one action with one name, offered by the answer
// that wrote the version and by the draft panel. What it made is kept here, for the visit, so
// that saving in one place shows as saved in the other.

const savedKey = (draftId: string, version: number) => `${draftId}:${version}`;

interface SavedDrafts {
  /** What saving this draft version made, if it was saved during this visit. */
  savedAs(draftId: string, version: number): VersionCreated | undefined;
  markSaved(draftId: string, version: number, created: VersionCreated): void;
}

const SavedDraftsContext = createContext<SavedDrafts | null>(null);

/** What was saved, per tenant; a tenant switch resets it like every other tenant query. */
const savedDraftsKey = (tenant: string) => ["saved-drafts", tenant] as const;

/**
 * Remembers which draft versions were saved and as what. It lives in the query cache, not in the
 * page, so it survives a visit to the saved dashboard and back: the conversation page is shown
 * again as it was left (the answer's saved state, its link) rather than as a draft that was never
 * saved, which also keeps its height and so the scroll position the browser restores
 * (Laysi/trustiot-v2#1185). Signing out or in as someone else removes it with the rest of the
 * cache. After a reload the server's refusal of the same version (409) is what says it was saved:
 * showing it as saved from the start needs the API to say so (Laysi/trustiot-v2#1096).
 */
export function SavedDraftsProvider({ children }: { children: ReactNode }) {
  const queryClient = useQueryClient();
  const tenant = useSession().session?.activeTenant?.id ?? "";
  const saved = useQuery<Readonly<Record<string, VersionCreated>>>({
    queryKey: savedDraftsKey(tenant),
    // Nothing to read from a server: the cache holds what this tab saved.
    queryFn: () => ({}),
    initialData: {},
    staleTime: Number.POSITIVE_INFINITY,
    gcTime: Number.POSITIVE_INFINITY,
    refetchOnMount: false,
    refetchOnReconnect: false,
    refetchOnWindowFocus: false,
  }).data;
  const value = useMemo<SavedDrafts>(
    () => ({
      savedAs: (draftId, version) => saved[savedKey(draftId, version)],
      markSaved: (draftId, version, created) =>
        queryClient.setQueryData<Readonly<Record<string, VersionCreated>>>(savedDraftsKey(tenant), (current) => ({
          ...current,
          [savedKey(draftId, version)]: created,
        })),
    }),
    [saved, queryClient, tenant],
  );
  return <SavedDraftsContext.Provider value={value}>{children}</SavedDraftsContext.Provider>;
}

function useSavedDrafts(): SavedDrafts {
  const context = useContext(SavedDraftsContext);
  if (context === null) throw new Error("useSavedDrafts needs a SavedDraftsProvider");
  return context;
}

/**
 * Why a save was refused, in the user's language. A refusal because the saved dashboard's latest
 * version is not the one this draft version was based on gets one more sentence: the same refusal
 * is what a version that was saved already gets, so it says which version of the dashboard that
 * would be.
 */
export function saveFailureReason(error: unknown, i18n: Pick<I18n, "t" | "errorMessage">): string {
  const message = i18n.errorMessage(error);
  if (error instanceof ApiError && error.body?.message_key === "dashboard.saved_changed") {
    const latest = error.body.details?.latest;
    if (latest !== undefined) return `${message} ${i18n.t("draft.saved_changed_hint", { latest })}`;
  }
  return message;
}

export interface DraftSave {
  /** Saves the version; a refusal of an earlier try is cleared. */
  save(): void;
  pending: boolean;
  /** What saving made, wherever it was saved from. */
  saved: VersionCreated | undefined;
  /** This save worked just now, from here (not saved elsewhere, not already saved on arrival). */
  savedHere: boolean;
  /** Why the last try was refused, translated. */
  failure: string | null;
}

/** Saves one version of a draft to the dashboard list, and knows whether it was saved. */
export function useSaveDraft(conversationId: string, draftId: string, version: number): DraftSave {
  const i18n = useI18n();
  const { savedAs, markSaved } = useSavedDrafts();
  const publish = usePublishDraft(conversationId, markSaved);
  const { reset, mutate } = publish;
  const save = useCallback(() => {
    reset();
    mutate({ draftId, version });
  }, [reset, mutate, draftId, version]);
  return {
    save,
    pending: publish.isPending,
    saved: savedAs(draftId, version),
    savedHere: publish.isSuccess,
    failure: publish.isError ? i18n.t("draft.save_failed", { reason: saveFailureReason(publish.error, i18n) }) : null,
  };
}

/** "Save to dashboard list"; disabled while it saves, and once the version is saved (it cannot be again). */
export function SaveButton({ save }: { save: DraftSave }) {
  const { t } = useI18n();
  return (
    <Button onClick={save.save} disabled={save.pending || save.saved !== undefined} aria-busy={save.pending}>
      {save.pending ? <Spinner /> : <UploadIcon />}
      {t(save.pending ? "draft.saving" : "draft.save")}
    </Button>
  );
}

/**
 * What the last try came to: why it was refused, or that the version is saved, with the link to
 * the dashboard. The button is disabled once it worked, so the focus goes to the link, but only
 * for a save made from here: a version saved in the other place does not take the focus.
 */
export function SaveOutcome({ save }: { save: DraftSave }) {
  const { t } = useI18n();
  const openSaved = useRef<HTMLAnchorElement>(null);
  const focusLink = save.savedHere && save.saved !== undefined;
  useEffect(() => {
    if (focusLink) openSaved.current?.focus();
  }, [focusLink]);
  return (
    <>
      {save.failure !== null ? (
        <Alert variant="destructive" className="grid-cols-[1rem_1fr]">
          <CircleAlertIcon aria-hidden="true" />
          <AlertDescription>
            <p>{save.failure}</p>
          </AlertDescription>
        </Alert>
      ) : null}
      {save.saved !== undefined ? (
        <Alert role="status" className="grid-cols-[1rem_1fr]">
          <CircleCheckIcon aria-hidden="true" className="text-success" />
          <AlertTitle>{t("draft.saved.title", { version: save.saved.version })}</AlertTitle>
          <AlertDescription>
            <p>{t("draft.saved.body")}</p>
            <Link
              ref={openSaved}
              className="rounded-sm underline underline-offset-4"
              to={dashboardPath(save.saved.dashboard_id)}
            >
              {t("draft.saved.open")}
            </Link>
          </AlertDescription>
        </Alert>
      ) : null}
    </>
  );
}
