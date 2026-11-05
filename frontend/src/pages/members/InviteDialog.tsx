import { useMutation } from "@tanstack/react-query";
import { useEffect, useId, useRef, useState } from "react";
import { ApiError } from "../../api/errors.ts";
import { useMembersApi } from "../../api/pending/MembersApiProvider.tsx";
import type { Invitation, MemberRole } from "../../api/pending/members.ts";
import { Alert, AlertDescription } from "../../components/ui/alert.tsx";
import { Button } from "../../components/ui/button.tsx";
import { FormField } from "../../components/ui/form-field.tsx";
import { Input } from "../../components/ui/input.tsx";
import { Spinner } from "../../components/ui/spinner.tsx";
import { useI18n } from "../../i18n/useI18n.ts";
import { MemberDialog } from "./MemberDialog.tsx";
import { RoleChoice } from "./RoleChoice.tsx";

const EMAIL_POINTER = "/email";

/** Something@something: the server decides the rest; this only catches an address that is plainly not one. */
const LOOKS_LIKE_EMAIL = /^[^\s@]+@[^\s@]+$/;

/**
 * The invitation dialog (docs/design/tenant-admin/members--invite-dialog): an email and a role. A
 * refusal of the email comes back as a field error and lands under the field, which takes the
 * focus; any other refusal (the plan's limit, the network) is shown in the dialog, which stays
 * open with what was typed. A new invitation closes the dialog and the page announces it.
 */
export function InviteDialog({
  open,
  onOpenChange,
  onInvited,
  onFailed,
  onCloseAutoFocus,
}: {
  open: boolean;
  onOpenChange(open: boolean): void;
  onInvited(invitation: Invitation): void;
  /** A refusal that other parts of the page may need to know about (the limit was reached). */
  onFailed(error: unknown): void;
  onCloseAutoFocus: (event: Event) => void;
}) {
  const { t, errorMessage, fieldError } = useI18n();
  const api = useMembersApi();
  const emailRef = useRef<HTMLInputElement>(null);
  const formId = useId();
  const [email, setEmail] = useState("");
  const [role, setRole] = useState<MemberRole>("viewer");
  const [local, setLocal] = useState<string | null>(null);

  const invite = useMutation({
    mutationFn: (request: { email: string; role: MemberRole }) => api.invite(request),
    onSuccess: (invitation) => {
      onOpenChange(false);
      onInvited(invitation);
    },
    onError: (error) => {
      onFailed(error);
      if (error instanceof ApiError && error.body?.field_errors?.some((item) => item.field === EMAIL_POINTER)) {
        // After the message is in the page, so the field is announced with it.
        queueMicrotask(() => emailRef.current?.focus());
      }
    },
  });

  // A dialog opened again starts empty, not with the last attempt.
  const { reset } = invite;
  useEffect(() => {
    if (!open) return;
    reset();
    setLocal(null);
    setEmail("");
    setRole("viewer");
  }, [open, reset]);

  const serverEmailError = invite.isError ? fieldError(invite.error, EMAIL_POINTER) : undefined;
  const emailError = local ?? serverEmailError;
  // Anything the server refused that no field shows: a refusal of the plan's limit or of the request as a whole.
  const generalError = invite.isError && serverEmailError === undefined ? errorMessage(invite.error) : null;

  function submit(event: React.FormEvent) {
    event.preventDefault();
    const trimmed = email.trim();
    if (!LOOKS_LIKE_EMAIL.test(trimmed)) {
      setLocal(t("members.invite.email_invalid"));
      emailRef.current?.focus();
      return;
    }
    setLocal(null);
    invite.mutate({ email: trimmed, role });
  }

  return (
    <MemberDialog
      open={open}
      onOpenChange={onOpenChange}
      title={t("members.invite.title")}
      description={t("members.invite.description")}
      busy={invite.isPending}
      onOpenAutoFocus={(event) => {
        event.preventDefault();
        emailRef.current?.focus();
      }}
      onCloseAutoFocus={onCloseAutoFocus}
    >
      <form id={formId} noValidate onSubmit={submit} className="grid gap-4">
        <FormField label={t("members.invite.email")} error={emailError}>
          {(control) => (
            <Input
              {...control}
              ref={emailRef}
              type="email"
              autoComplete="off"
              value={email}
              disabled={invite.isPending}
              onChange={(event) => {
                setEmail(event.target.value);
                setLocal(null);
              }}
            />
          )}
        </FormField>
        <RoleChoice value={role} onChange={setRole} disabled={invite.isPending} />
        {generalError ? (
          <Alert variant="destructive">
            <AlertDescription>
              <p>{generalError}</p>
            </AlertDescription>
          </Alert>
        ) : null}
        <div className="flex flex-col-reverse gap-2 sm:flex-row sm:justify-end">
          <Button type="button" variant="outline" disabled={invite.isPending} onClick={() => onOpenChange(false)}>
            {t("members.dialog.cancel")}
          </Button>
          <Button type="submit" disabled={invite.isPending}>
            {invite.isPending ? <Spinner /> : null}
            {invite.isPending ? t("members.invite.sending") : t("members.invite.submit")}
          </Button>
        </div>
      </form>
    </MemberDialog>
  );
}
