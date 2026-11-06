import { useQuery, useQueryClient } from "@tanstack/react-query";
import { type FormEvent, useRef, useState } from "react";

import { usePlatformApi } from "../../../api/pending/PlatformApiProvider.tsx";
import type { MemberRole, TenantSummary } from "../../../api/pending/platform.ts";
import { Alert, AlertDescription } from "../../../components/ui/alert.tsx";
import { Button } from "../../../components/ui/button.tsx";
import { Card, CardContent, CardDescription, CardFooter, CardHeader, CardTitle } from "../../../components/ui/card.tsx";
import { FormField } from "../../../components/ui/form-field.tsx";
import { Input } from "../../../components/ui/input.tsx";
import { NativeSelect } from "../../../components/ui/native-select.tsx";
import { useI18n } from "../../../i18n/useI18n.ts";
import { MemberRoleDialog } from "../dialogs/MemberRoleDialog.tsx";
import { isFinal } from "../format.ts";
import { membersKey, platformKey } from "../queries.ts";
import { Skeleton } from "../ui/skeleton.tsx";
import { StackTable, Td, Th, Tr } from "../ui/stack-table.tsx";

const ROLES: readonly MemberRole[] = ["admin", "editor", "viewer"];
const EMAIL = /^[^@\s]+@[^@\s]+\.[^@\s]+$/;

/**
 * A tenant's members and the rescue form (docs/design/platform-admin/tenant-detail.html, 「成員分頁」):
 * the list, and a form that sets an existing user's role in this tenant, for when the tenant's own
 * admins are locked out. It does not create accounts: the server refuses an email nobody has
 * (422 `member.email_unknown`) and the confirmation says so. A tenant whose contract was terminated
 * is not changed any more, so the form is not offered.
 */
export function TenantMembers({ tenant, onDone }: { tenant: TenantSummary; onDone(message: string): void }) {
  const { t, errorMessage } = useI18n();
  const api = usePlatformApi();
  const queryClient = useQueryClient();
  const [email, setEmail] = useState("");
  const [role, setRole] = useState<MemberRole>("admin");
  const [checked, setChecked] = useState(false);
  const [confirming, setConfirming] = useState(false);
  const submit = useRef<HTMLButtonElement>(null);

  const members = useQuery({
    queryKey: membersKey(tenant.id),
    queryFn: ({ signal }) => api.listMembers(tenant.id, signal),
  });

  const trimmed = email.trim();
  const valid = EMAIL.test(trimmed);
  const current = members.data?.find((member) => member.email.toLowerCase() === trimmed.toLowerCase())?.role ?? null;

  const review = (event: FormEvent) => {
    event.preventDefault();
    setChecked(true);
    if (valid) setConfirming(true);
  };

  return (
    <div className="grid gap-4 lg:grid-cols-[minmax(0,1fr)_minmax(0,22rem)]">
      {members.isPending ? (
        <section aria-busy="true" className="grid gap-4 rounded-lg border bg-card px-4 py-5">
          <p role="status" className="sr-only">
            {t("platform.members.loading")}
          </p>
          {[0, 1, 2].map((row) => (
            <div key={row} className="flex items-center justify-between gap-4">
              <Skeleton className="w-2/5" />
              <Skeleton className="w-1/6" />
            </div>
          ))}
        </section>
      ) : members.data === undefined ? (
        <Alert variant="destructive" className="grid-cols-[1fr_auto] items-center gap-x-3">
          <AlertDescription className="col-start-1">
            <p className="font-medium">{t("platform.members.load_failed")}</p>
            <p>{errorMessage(members.error)}</p>
          </AlertDescription>
          <Button variant="outline" onClick={() => void members.refetch()} disabled={members.isFetching}>
            {t("page.action.retry")}
          </Button>
        </Alert>
      ) : members.data.length === 0 ? (
        <div role="status" className="rounded-lg border px-4 py-10 text-center text-sm text-muted-foreground">
          {t("platform.members.empty")}
        </div>
      ) : (
        <StackTable
          caption={t("platform.members.caption", { count: members.data.length })}
          head={
            <>
              <Th>{t("platform.members.column.member")}</Th>
              <Th>{t("platform.members.column.role")}</Th>
            </>
          }
        >
          {members.data.map((member) => (
            <Tr key={member.userId}>
              <Td label={t("platform.members.column.member")}>{member.email}</Td>
              <Td label={t("platform.members.column.role")}>{member.role}</Td>
            </Tr>
          ))}
        </StackTable>
      )}

      {isFinal(tenant) ? null : (
        <form noValidate onSubmit={review}>
          <Card>
            <CardHeader>
              <CardTitle>{t("platform.members.rescue_title")}</CardTitle>
              <CardDescription>{t("platform.members.rescue_description")}</CardDescription>
            </CardHeader>
            <CardContent className="grid gap-4">
              <FormField
                label={t("platform.members.email")}
                error={checked && !valid ? t("member.email_invalid") : undefined}
              >
                {(control) => (
                  <Input
                    {...control}
                    type="email"
                    autoComplete="off"
                    value={email}
                    onChange={(event) => setEmail(event.target.value)}
                  />
                )}
              </FormField>
              <FormField label={t("platform.members.role")}>
                {(control) => (
                  <NativeSelect
                    {...control}
                    value={role}
                    onChange={(event) => setRole(event.target.value as MemberRole)}
                  >
                    {ROLES.map((value) => (
                      <option key={value} value={value}>
                        {value}
                      </option>
                    ))}
                  </NativeSelect>
                )}
              </FormField>
              <p className="text-sm text-muted-foreground">{t("platform.members.rescue_note")}</p>
            </CardContent>
            <CardFooter className="justify-end">
              <Button ref={submit} type="submit">
                {t("platform.members.review")}
              </Button>
            </CardFooter>
          </Card>
        </form>
      )}

      {confirming ? (
        <MemberRoleDialog
          open
          tenantId={tenant.id}
          tenantName={tenant.name}
          email={trimmed}
          role={role}
          current={current}
          onOpenChange={(open) => {
            if (!open) setConfirming(false);
          }}
          onDone={(message) => {
            setConfirming(false);
            setEmail("");
            setChecked(false);
            void queryClient.invalidateQueries({ queryKey: platformKey });
            onDone(message);
          }}
          returnFocus={() => submit.current}
        />
      ) : null}
    </div>
  );
}
