import { type ReactNode, useState } from "react";

import { useModelRoleApi } from "../../../api/ModelRoleApiProvider.tsx";
import type { ModelProvider, ModelRole } from "../../../api/modelRoles.ts";
import { Alert, AlertDescription, AlertTitle } from "../../../components/ui/alert.tsx";
import {
  AlertDialog,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
  AlertDialogTrigger,
} from "../../../components/ui/alert-dialog.tsx";
import { Button } from "../../../components/ui/button.tsx";
import { Spinner } from "../../../components/ui/spinner.tsx";
import { useI18n } from "../../../i18n/useI18n.ts";
import { cn } from "../../../lib/utils.ts";
import { ModelListEditor, modelName } from "./ModelListEditor.tsx";
import { AuditNote, ChangePreview, ImpactList } from "./ui.tsx";
import { type ChangeState, useRoleChange } from "./useRoleChange.ts";

function sameList(a: readonly string[], b: readonly string[]): boolean {
  return a.length === b.length && a.every((item, index) => item === b[index]);
}

/** Why a change did not go through, or what the re-read found, as the design draws it (`ai-model-roles.html`). */
function Outcome({
  state,
  role,
  onCheckLatest,
  onResend,
  onDrop,
}: {
  state: ChangeState;
  role: string;
  onCheckLatest: () => void;
  onResend: () => void;
  onDrop: () => void;
}) {
  const { t, errorMessage } = useI18n();
  switch (state.kind) {
    case "conflict":
      return (
        <Alert>
          <AlertTitle>
            {t("platform_ai.roles.conflict_title", { role, from: state.currentVersion, to: state.currentVersion })}
          </AlertTitle>
          <AlertDescription>
            <p>{t("platform_ai.roles.conflict_body")}</p>
            {state.rereadFailed ? (
              <p role="alert" className="font-medium text-destructive">
                {t("platform_ai.roles.conflict_reread_failed")}
              </p>
            ) : null}
            <div className="flex flex-wrap gap-2 pt-1">
              <Button size="sm" onClick={onCheckLatest}>
                {t("platform_ai.roles.conflict_check", { version: state.currentVersion })}
              </Button>
              <Button size="sm" variant="ghost" onClick={onDrop}>
                {t("platform_ai.roles.conflict_drop")}
              </Button>
            </div>
          </AlertDescription>
        </Alert>
      );
    case "checking":
      return (
        <Alert>
          <AlertTitle>{t("platform_ai.roles.unknown_title")}</AlertTitle>
          <AlertDescription>
            <p>{t("platform_ai.roles.unknown_body")}</p>
          </AlertDescription>
        </Alert>
      );
    case "applied":
      return (
        <Alert role="status">
          <AlertTitle>{t("platform_ai.roles.applied_title", { version: state.role.version })}</AlertTitle>
          <AlertDescription>
            <p>{t("platform_ai.roles.applied_body")}</p>
          </AlertDescription>
        </Alert>
      );
    case "not_applied":
      return (
        <Alert variant="destructive">
          <AlertTitle>{t("platform_ai.roles.not_applied_title")}</AlertTitle>
          <AlertDescription>
            <p>{t("platform_ai.roles.not_applied_body", { role, version: state.role.version })}</p>
            <Button size="sm" variant="outline" onClick={onResend}>
              {t("platform_ai.roles.resend")}
            </Button>
          </AlertDescription>
        </Alert>
      );
    case "unknown":
      return (
        <Alert variant="destructive">
          <AlertDescription>
            <p>{t("platform_ai.roles.reload_failed")}</p>
          </AlertDescription>
        </Alert>
      );
    case "error":
      return (
        <Alert variant="destructive">
          <AlertDescription>
            <p>{errorMessage(state.error)}</p>
          </AlertDescription>
        </Alert>
      );
    default:
      return null;
  }
}

/** One side of the before-and-after: the models in order, the ones that differ marked in words and color. */
function ModelColumn({
  title,
  models,
  marked,
  mark,
  tone,
}: {
  title: string;
  models: readonly string[];
  marked: ReadonlySet<string>;
  mark: string;
  tone: "added" | "removed";
}) {
  const { t } = useI18n();
  return (
    <div className="grid content-start gap-1.5">
      <div className="text-sm font-medium">{title}</div>
      <ol className="grid gap-1.5">
        {models.map((model, index) => {
          const isMarked = marked.has(model);
          return (
            <li
              key={model}
              className={cn(
                "flex flex-wrap items-center gap-2 rounded-md border px-3 py-1.5",
                isMarked && (tone === "added" ? "bg-success/15" : "bg-destructive/10"),
              )}
            >
              <span aria-hidden="true" className="text-sm text-muted-foreground tabular-nums">
                {index + 1}
              </span>
              <span className={cn("min-w-0 font-mono text-sm wrap-anywhere", isMarked && tone === "removed" && "line-through")}>
                {model}
              </span>
              <span className="text-xs text-muted-foreground">
                {index === 0 ? t("platform_ai.roles.primary") : t("platform_ai.roles.fallback")}
                {isMarked ? ` · ${mark}` : ""}
              </span>
            </li>
          );
        })}
      </ol>
    </div>
  );
}

/**
 * Changing a role's models in two steps, as design.md sets for a change that applies to every
 * tenant but can be changed again: edit the list, then see the before and the after and confirm.
 * Nothing is typed to confirm. A 409 keeps the draft, a 504 is read back before anything is sent
 * again (`useRoleChange`), and the page shows what the server answered, not a guess.
 */
export function ChangeRoleDialog({
  role,
  providers,
  onChanged,
}: {
  role: ModelRole;
  providers: readonly ModelProvider[];
  onChanged: (role: ModelRole, kind: "set") => void;
}) {
  const { t, fieldError, locale } = useI18n();
  const api = useModelRoleApi();
  const [open, setOpen] = useState(false);
  const [step, setStep] = useState<"edit" | "confirm">("edit");
  const [base, setBase] = useState(role);
  const [draft, setDraft] = useState<string[]>([...role.models]);

  const change = useRoleChange({
    base,
    send: (current) => api.set(current.role, draft, current.version),
    tookEffect: (fresh) => sameList(fresh.models, draft),
    onApplied: (updated) => {
      setOpen(false);
      onChanged(updated, "set");
    },
  });
  const { state } = change;
  const pending = state.kind === "pending" || state.kind === "checking";
  const finished = state.kind === "applied";

  const reset = () => {
    setBase(role);
    setDraft([...role.models]);
    setStep("edit");
    change.setState({ kind: "idle" });
  };
  const sameProtocol = providers.filter((provider) => provider.protocol === role.protocol);
  const unchanged = sameList(draft, base.models);
  const valid = draft.length >= 1 && draft.length <= 8 && !unchanged;

  const added = new Set(draft.filter((model) => !base.models.includes(model)));
  const removed = new Set(base.models.filter((model) => !draft.includes(model)));
  const primaryChanged = draft[0] !== base.models[0];
  const refusal = state.kind === "error" ? fieldError(state.error, "/models") : undefined;

  const checkLatest = async () => {
    const fresh = state.kind === "conflict" ? (state.fresh ?? (await change.reread())) : null;
    if (fresh === null) {
      // The draft stays: say the read failed and let the button be pressed again.
      if (state.kind === "conflict") change.setState({ ...state, rereadFailed: true });
      return;
    }
    setBase(fresh);
    setStep("confirm");
    change.setState({ kind: "idle" });
  };

  const outcomeHandlers = {
    onCheckLatest: () => void checkLatest(),
    onResend: () => void change.run(),
    onDrop: () => setOpen(false),
  };

  let body: ReactNode;
  if (step === "edit") {
    body = (
      <>
        <AlertDialogHeader>
          <AlertDialogTitle>{t("platform_ai.edit.title", { role: role.role })}</AlertDialogTitle>
          <AlertDialogDescription>{t("platform_ai.edit.desc", { protocol: protocolName(role.protocol) })}</AlertDialogDescription>
        </AlertDialogHeader>
        <ModelListEditor
          models={draft}
          providers={sameProtocol}
          onChange={setDraft}
          label={t("platform_ai.edit.list_label")}
        />
        {refusal ? (
          <p role="alert" className="text-sm font-medium text-destructive">
            {refusal}
          </p>
        ) : null}
        {state.kind === "error" && !refusal ? <Outcome {...outcomeHandlers} state={state} role={role.role} /> : null}
        {unchanged ? <p className="text-sm text-muted-foreground">{t("platform_ai.edit.unchanged")}</p> : null}
        <AlertDialogFooter>
          <AlertDialogCancel>{t("platform_ai.cancel")}</AlertDialogCancel>
          <Button
            disabled={!valid}
            onClick={() => {
              change.setState({ kind: "idle" });
              setStep("confirm");
            }}
          >
            {t("platform_ai.edit.next")}
          </Button>
        </AlertDialogFooter>
      </>
    );
  } else {
    body = (
      <>
        <AlertDialogHeader>
          <AlertDialogTitle>{t("platform_ai.confirm.title", { role: role.role })}</AlertDialogTitle>
          <AlertDialogDescription>{t("platform_ai.confirm.desc")}</AlertDialogDescription>
        </AlertDialogHeader>
        <div className="grid gap-4 sm:grid-cols-2">
          <ModelColumn
            title={t("platform_ai.confirm.now", { version: base.version })}
            models={base.models}
            marked={removed}
            mark={t("platform_ai.confirm.removed")}
            tone="removed"
          />
          <ModelColumn
            title={t("platform_ai.confirm.after")}
            models={draft}
            marked={added}
            mark={t("platform_ai.confirm.added")}
            tone="added"
          />
        </div>
        <ImpactList
          title={t("platform_ai.confirm.impact_title")}
          items={[
            primaryChanged ? t("platform_ai.confirm.impact_cache") : t("platform_ai.confirm.impact_fallback"),
            t("platform_ai.confirm.impact_now"),
          ]}
        />
        <AuditNote>{t("platform_ai.confirm.audit")}</AuditNote>
        <Outcome {...outcomeHandlers} state={state} role={role.role} />
        <AlertDialogFooter>
          {finished ? (
            <AlertDialogCancel>{t("platform_ai.close")}</AlertDialogCancel>
          ) : (
            <>
              <Button variant="outline" disabled={pending} onClick={() => setStep("edit")}>
                {t("platform_ai.confirm.back")}
              </Button>
              <Button disabled={pending || state.kind === "conflict" || state.kind === "not_applied"} onClick={() => void change.run()}>
                {pending ? <Spinner /> : null}
                {pending ? t("platform_ai.confirm.pending") : t("platform_ai.confirm.submit", { role: role.role })}
              </Button>
            </>
          )}
        </AlertDialogFooter>
      </>
    );
  }

  return (
    <AlertDialog
      open={open}
      onOpenChange={(next) => {
        if (pending) return;
        if (next) reset();
        setOpen(next);
      }}
    >
      <AlertDialogTrigger asChild>
        <Button variant="outline" size="sm" aria-label={t("platform_ai.roles.change_label", { role: role.role })}>
          {t("platform_ai.roles.change")}
        </Button>
      </AlertDialogTrigger>
      <AlertDialogContent className="max-h-[calc(100dvh-2rem)] overflow-y-auto sm:max-w-2xl">{body}</AlertDialogContent>
    </AlertDialog>
  );
}

/** Resetting a role to the models the configuration gives it, after seeing both. */
export function ResetRoleDialog({
  role,
  onChanged,
}: {
  role: ModelRole;
  onChanged: (role: ModelRole, kind: "reset") => void;
}) {
  const { t } = useI18n();
  const api = useModelRoleApi();
  const [open, setOpen] = useState(false);
  const [base, setBase] = useState(role);
  const change = useRoleChange({
    base,
    send: (current) => api.reset(current.role, current.version),
    tookEffect: (fresh) => fresh.source === "configuration" && fresh.ignored === null,
    onApplied: (updated) => {
      setOpen(false);
      onChanged(updated, "reset");
    },
  });
  const { state } = change;
  const pending = state.kind === "pending" || state.kind === "checking";
  // After a conflict the latest version may already be on the default: there is nothing to reset.
  const alreadyDefault = base.source === "configuration" && base.ignored === null;

  const outcomeHandlers = {
    onCheckLatest: async () => {
      const fresh = state.kind === "conflict" ? (state.fresh ?? (await change.reread())) : null;
      if (fresh === null) {
        if (state.kind === "conflict") change.setState({ ...state, rereadFailed: true });
        return;
      }
      setBase(fresh);
      change.setState({ kind: "idle" });
    },
    onResend: () => void change.run(),
    onDrop: () => setOpen(false),
  };

  return (
    <AlertDialog
      open={open}
      onOpenChange={(next) => {
        if (pending) return;
        if (next) {
          setBase(role);
          change.setState({ kind: "idle" });
        }
        setOpen(next);
      }}
    >
      <AlertDialogTrigger asChild>
        <Button variant="ghost" size="sm" aria-label={t("platform_ai.roles.reset_label", { role: role.role })}>
          {t("platform_ai.roles.reset")}
        </Button>
      </AlertDialogTrigger>
      <AlertDialogContent className="sm:max-w-xl">
        <AlertDialogHeader>
          <AlertDialogTitle>{t("platform_ai.reset.title", { role: role.role })}</AlertDialogTitle>
          <AlertDialogDescription>{t("platform_ai.confirm.desc")}</AlertDialogDescription>
        </AlertDialogHeader>
        <ChangePreview
          beforeLabel={t("platform_ai.reset.now")}
          before={base.models.map(modelName).join(" → ")}
          afterLabel={t("platform_ai.reset.default")}
          after={base.configured_models.map(modelName).join(" → ")}
        />
        {alreadyDefault ? (
          <p role="status" className="text-sm text-muted-foreground">
            {t("platform_ai.reset.already_default")}
          </p>
        ) : (
          <AuditNote>{t("platform_ai.reset.audit")}</AuditNote>
        )}
        <Outcome
          state={state}
          role={role.role}
          onCheckLatest={() => void outcomeHandlers.onCheckLatest()}
          onResend={outcomeHandlers.onResend}
          onDrop={outcomeHandlers.onDrop}
        />
        <AlertDialogFooter>
          {state.kind === "applied" ? (
            <AlertDialogCancel>{t("platform_ai.close")}</AlertDialogCancel>
          ) : (
            <>
              <AlertDialogCancel disabled={pending}>{t("platform_ai.cancel")}</AlertDialogCancel>
              <Button
                disabled={pending || alreadyDefault || state.kind === "conflict" || state.kind === "not_applied"}
                onClick={() => void change.run()}
              >
                {pending ? <Spinner /> : null}
                {pending ? t("platform_ai.reset.pending") : t("platform_ai.reset.submit", { role: role.role })}
              </Button>
            </>
          )}
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  );
}

/** The names of the protocols the gateway speaks, as the vendors write them; another one is shown as it is. */
export function protocolName(protocol: string): string {
  switch (protocol) {
    case "anthropic-messages":
      return "Anthropic Messages";
    case "openai-decisions":
      return "OpenAI Decisions";
    default:
      return protocol;
  }
}
