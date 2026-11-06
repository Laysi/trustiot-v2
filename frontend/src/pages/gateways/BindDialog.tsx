import { useMutation } from "@tanstack/react-query";
import { TriangleAlertIcon, XIcon } from "lucide-react";
import { type FormEvent, type RefObject, useEffect, useRef, useState } from "react";
import { ApiError, classifyError } from "../../api/errors.ts";
import type { Gateway } from "../../api/gateways.ts";
import { useGatewaysApi } from "../../api/pending/GatewaysApiProvider.tsx";
import { Alert, AlertDescription, AlertTitle } from "../../components/ui/alert.tsx";
import {
  AlertDialog,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
} from "../../components/ui/alert-dialog.tsx";
import { Button, focusRing } from "../../components/ui/button.tsx";
import { FormField } from "../../components/ui/form-field.tsx";
import { Input } from "../../components/ui/input.tsx";
import type { MessageId } from "../../i18n/messages.ts";
import { useI18n } from "../../i18n/useI18n.ts";
import { cn } from "../../lib/utils.ts";

/** The hardware ID's format (the `Gateway` schema in api/openapi.json). */
const HARDWARE_ID = /^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$/;

/** How long to wait when a rate limit does not say. */
const DEFAULT_RETRY_AFTER_SECS = 60;

/** Why the last attempt did not bind the gateway. */
type Failure =
  /** The pair is wrong, or the gateway cannot be bound: one answer for every reason (Laysi/trustiot-v2#114). */
  | { kind: "rejected" }
  /** No answer: the attempt may or may not have arrived, and sending it again is safe. */
  | { kind: "network" }
  /** Too many attempts: binding waits until `until` (a time in ms). */
  | { kind: "limited"; until: number }
  /** The plan's gateway limit was reached between loading the page and binding. */
  | { kind: "quota" }
  | { kind: "other"; error: unknown };

function failureOf(error: unknown, now: number): Failure {
  if (!(error instanceof ApiError)) return { kind: "other", error };
  const code = error.body?.code;
  if (code === "rate_limited" || (code === undefined && error.status === 429)) {
    const advised = error.body?.details?.retry_after_secs;
    const seconds = typeof advised === "number" && advised > 0 ? advised : DEFAULT_RETRY_AFTER_SECS;
    return { kind: "limited", until: now + seconds * 1000 };
  }
  if (code === "quota_exceeded") return { kind: "quota" };
  if (classifyError(error) === "unavailable") return { kind: "network" };
  if (
    code === "not_found" ||
    code === "conflict" ||
    code === "validation_failed" ||
    code === "invalid_request" ||
    error.body?.message_key === "gateway.claim_rejected"
  ) {
    return { kind: "rejected" };
  }
  return { kind: "other", error };
}

type FieldKey = "hardware" | "claim";

/**
 * Binds a gateway to the tenant (design `gateways--bind` and its failure states): the hardware ID
 * and the claim code from the label (decision 5). The input stays after every failure, so the
 * administrator corrects it and tries again; the page mounts the dialog while it is open, so
 * reopening starts empty. Escape and Cancel do not close it while a binding is on its way.
 */
export function BindDialog({
  tenantName,
  returnFocus,
  onClose,
  onBound,
}: {
  tenantName: string | null;
  /** Gets the focus back when the dialog closes: the button that opened it. */
  returnFocus: RefObject<HTMLElement | null>;
  onClose(): void;
  onBound(gateway: Gateway): void;
}) {
  const { t, errorMessage } = useI18n();
  const api = useGatewaysApi();
  const [hardwareId, setHardwareId] = useState("");
  const [claimCode, setClaimCode] = useState("");
  const [fieldErrors, setFieldErrors] = useState<Partial<Record<FieldKey, MessageId>>>({});
  const [failure, setFailure] = useState<Failure | null>(null);
  const [now, setNow] = useState(() => Date.now());
  const hardwareRef = useRef<HTMLInputElement>(null);
  const claimRef = useRef<HTMLInputElement>(null);

  const bind = useMutation({
    mutationFn: (request: { hardwareId: string; claimCode: string }) => api.bindGateway(request),
  });
  const pending = bind.isPending;

  const until = failure?.kind === "limited" ? failure.until : null;
  const remaining = until === null ? 0 : Math.max(0, Math.ceil((until - now) / 1000));
  useEffect(() => {
    if (until === null) return;
    setNow(Date.now());
    const timer = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(timer);
  }, [until]);
  useEffect(() => {
    if (until !== null && remaining === 0) setFailure(null);
  }, [until, remaining]);
  const limited = remaining > 0;

  const countdown = () =>
    remaining >= 60
      ? t("gateways.bind.countdown", { minutes: Math.floor(remaining / 60), seconds: remaining % 60 })
      : t("gateways.bind.countdown_seconds", { seconds: remaining });

  const submit = (event: FormEvent) => {
    event.preventDefault();
    if (pending || limited) return;
    const hardware = hardwareId.trim();
    const claim = claimCode.trim();
    const errors: Partial<Record<FieldKey, MessageId>> = {};
    if (hardware === "") errors.hardware = "gateways.bind.hardware_required";
    else if (!HARDWARE_ID.test(hardware)) errors.hardware = "gateways.bind.hardware_invalid";
    if (claim === "") errors.claim = "gateways.bind.claim_required";
    setFieldErrors(errors);
    if (errors.hardware) return hardwareRef.current?.focus();
    if (errors.claim) return claimRef.current?.focus();
    setFailure(null);
    bind.mutate(
      { hardwareId: hardware, claimCode: claim },
      {
        onSuccess: onBound,
        onError: (error) => setFailure(failureOf(error, Date.now())),
      },
    );
  };

  const alert = (() => {
    switch (failure?.kind) {
      case undefined:
        return null;
      case "rejected":
        return { title: t("gateways.bind.rejected_title"), body: t("gateways.bind.rejected_body") };
      case "network":
        return { title: t("gateways.bind.network_title"), body: t("gateways.bind.network_body") };
      case "limited":
        return {
          title: t("gateways.bind.limited_title"),
          body: t("gateways.bind.limited_body", { time: countdown() }),
        };
      case "quota":
        return { title: t("gateways.bind.quota_title"), body: t("gateways.bind.quota_body") };
      case "other":
        return { title: t("gateways.bind.other_title"), body: errorMessage(failure.error) };
    }
  })();

  const submitLabel = pending
    ? t("gateways.bind.submitting")
    : limited
      ? t("gateways.bind.wait", { time: countdown() })
      : failure?.kind === "network"
        ? t("gateways.bind.retry")
        : t("gateways.bind.submit");

  return (
    <AlertDialog open onOpenChange={(open) => !open && !pending && onClose()}>
      <AlertDialogContent
        role="dialog"
        onOpenAutoFocus={(event) => {
          event.preventDefault();
          hardwareRef.current?.focus();
        }}
        onCloseAutoFocus={(event) => {
          event.preventDefault();
          returnFocus.current?.focus();
        }}
        onEscapeKeyDown={(event) => {
          if (pending) event.preventDefault();
        }}
      >
        <AlertDialogHeader>
          <AlertDialogTitle>{t("gateways.bind.title")}</AlertDialogTitle>
          <AlertDialogDescription>
            {t("gateways.bind.description", { tenant: tenantName ?? "" })}
          </AlertDialogDescription>
        </AlertDialogHeader>
        <button
          type="button"
          aria-label={t("gateways.bind.close")}
          disabled={pending}
          onClick={onClose}
          className={cn(
            "absolute top-3 right-3 grid size-8 place-items-center rounded-md text-muted-foreground hover:bg-accent hover:text-accent-foreground",
            focusRing,
          )}
        >
          <XIcon aria-hidden="true" className="size-4" />
        </button>
        {alert ? (
          <Alert variant="destructive">
            <TriangleAlertIcon aria-hidden="true" />
            <AlertTitle>{alert.title}</AlertTitle>
            <AlertDescription>
              <p>{alert.body}</p>
            </AlertDescription>
          </Alert>
        ) : null}
        <form id="bind-gateway-form" noValidate onSubmit={submit} className="grid gap-4">
          <FormField
            label={t("gateways.bind.hardware_label")}
            description={t("gateways.bind.hardware_hint")}
            error={fieldErrors.hardware ? t(fieldErrors.hardware) : undefined}
          >
            {(control) => (
              <Input
                {...control}
                ref={hardwareRef}
                value={hardwareId}
                placeholder={t("gateways.bind.hardware_placeholder")}
                autoComplete="off"
                autoCapitalize="off"
                spellCheck={false}
                readOnly={pending}
                onChange={(event) => {
                  setHardwareId(event.target.value);
                  setFieldErrors((current) => ({ ...current, hardware: undefined }));
                }}
              />
            )}
          </FormField>
          <FormField
            label={t("gateways.bind.claim_label")}
            description={t("gateways.bind.claim_hint")}
            error={fieldErrors.claim ? t(fieldErrors.claim) : undefined}
          >
            {(control) => (
              <Input
                {...control}
                ref={claimRef}
                value={claimCode}
                placeholder={t("gateways.bind.claim_placeholder")}
                autoComplete="off"
                autoCapitalize="characters"
                spellCheck={false}
                readOnly={pending}
                onChange={(event) => {
                  setClaimCode(event.target.value);
                  setFieldErrors((current) => ({ ...current, claim: undefined }));
                }}
              />
            )}
          </FormField>
        </form>
        <AlertDialogFooter>
          <AlertDialogCancel disabled={pending}>{t("gateways.bind.cancel")}</AlertDialogCancel>
          <Button type="submit" form="bind-gateway-form" disabled={pending || limited}>
            {submitLabel}
          </Button>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  );
}
