import { CircleCheckIcon, LogOutIcon, MonitorSmartphoneIcon } from "lucide-react";
import { useMemo, useState } from "react";

import { Alert, AlertDescription } from "../components/ui/alert.tsx";
import {
  AlertDialog,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
  AlertDialogTrigger,
} from "../components/ui/alert-dialog.tsx";
import { Button } from "../components/ui/button.tsx";
import { Card, CardAction, CardContent, CardDescription, CardHeader, CardTitle } from "../components/ui/card.tsx";
import { FormField } from "../components/ui/form-field.tsx";
import { NativeSelect } from "../components/ui/native-select.tsx";
import { Spinner } from "../components/ui/spinner.tsx";
import { useI18n } from "../i18n/useI18n.ts";
import { AiLanguageOptions, LanguageOptions, pick, ThemeOptions } from "../preferences/options.tsx";
import { usePreferences } from "../preferences/PreferencesProvider.tsx";
import {
  FIELD_POINTERS,
  LANGUAGES,
  type PreferenceField,
  type Preferences,
  THEMES,
} from "../preferences/preferences.ts";
import { useSession } from "../session/SessionProvider.tsx";
import { useSignOut } from "../session/useSignOut.ts";

/** The personal settings: preferences (saved to the account), the account, and every session. */
export function SettingsPage() {
  const { t } = useI18n();
  return (
    <div className="mx-auto grid w-full max-w-3xl gap-6">
      <header className="grid gap-1">
        <h1 className="text-2xl font-semibold tracking-tight">{t("settings.title")}</h1>
        <p className="text-muted-foreground">{t("settings.description")}</p>
      </header>
      <PreferencesCard />
      <AccountCard />
      <SessionsCard />
    </div>
  );
}

function timeZones(): readonly string[] {
  try {
    return Intl.supportedValuesOf("timeZone");
  } catch {
    return [];
  }
}

/**
 * Each change is saved on its own as soon as it is made, and applies at once. The server's answer
 * is what stays: a refused value goes back to the saved one, and the reason is shown on that field.
 */
function PreferencesCard() {
  const { t, errorMessage, fieldError } = useI18n();
  const { preferences, source, error, reload, update, saving, browserZone } = usePreferences();
  // The errors are kept, not their text, so a change of language translates them again.
  const [failures, setFailures] = useState<Partial<Record<PreferenceField, unknown>>>({});
  const [saved, setSaved] = useState(false);
  const zones = useMemo(() => {
    const all = timeZones();
    // A zone saved from another browser stays selectable even if this one does not list it.
    return preferences.timeZone !== "auto" && !all.includes(preferences.timeZone)
      ? [preferences.timeZone, ...all]
      : all;
  }, [preferences.timeZone]);
  const disabled = source !== "server";

  const change = async (field: PreferenceField, value: Partial<Preferences>) => {
    setSaved(false);
    setFailures(({ [field]: _cleared, ...rest }) => rest);
    const result = await update(value);
    if (result.ok) {
      setSaved(true);
    } else {
      setFailures((current) => ({ ...current, [field]: result.error }));
    }
  };

  const errorFor = (field: PreferenceField): string | undefined => {
    if (!(field in failures)) return undefined;
    const failure = failures[field];
    return fieldError(failure, FIELD_POINTERS[field]) ?? t("settings.save_failed", { reason: errorMessage(failure) });
  };

  return (
    <Card aria-labelledby="settings-preferences">
      <CardHeader>
        <CardTitle id="settings-preferences">{t("settings.preferences.title")}</CardTitle>
        <CardDescription>{t("settings.preferences.description")}</CardDescription>
        <CardAction>
          <p role="status" className="flex h-5 items-center gap-1.5 text-sm text-muted-foreground">
            {saving ? (
              <>
                <Spinner />
                {t("settings.saving")}
              </>
            ) : saved ? (
              <>
                <CircleCheckIcon className="size-4 text-success" />
                {t("settings.saved")}
              </>
            ) : null}
          </p>
        </CardAction>
      </CardHeader>
      <CardContent className="grid gap-6">
        {source === "loading" ? <Spinner label={t("app.loading")} /> : null}
        {source === "error" ? (
          <Alert variant="destructive" className="grid-cols-[1fr_auto] items-center gap-x-3">
            <AlertDescription className="col-start-1">
              <p>{t("settings.load_failed")}</p>
              <p>{errorMessage(error)}</p>
            </AlertDescription>
            <Button variant="outline" size="sm" onClick={reload}>
              {t("settings.retry")}
            </Button>
          </Alert>
        ) : null}
        <div className="grid gap-6 sm:grid-cols-2">
          <FormField label={t("settings.language")} error={errorFor("language")}>
            {(control) => (
              <NativeSelect
                {...control}
                disabled={disabled}
                value={preferences.language}
                onChange={(event) => {
                  const language = pick(LANGUAGES, event.target.value);
                  if (language) void change("language", { language });
                }}
              >
                <LanguageOptions />
              </NativeSelect>
            )}
          </FormField>
          <FormField
            label={t("settings.ai_language")}
            description={t("settings.ai_language.description")}
            error={errorFor("aiLanguage")}
          >
            {(control) => (
              <NativeSelect
                {...control}
                disabled={disabled}
                value={preferences.aiLanguage}
                onChange={(event) => {
                  const aiLanguage = pick(LANGUAGES, event.target.value);
                  if (aiLanguage) void change("aiLanguage", { aiLanguage });
                }}
              >
                <AiLanguageOptions />
              </NativeSelect>
            )}
          </FormField>
          <FormField
            label={t("settings.time_zone")}
            description={t("settings.time_zone.description")}
            error={errorFor("timeZone")}
          >
            {(control) => (
              <NativeSelect
                {...control}
                disabled={disabled}
                value={preferences.timeZone}
                onChange={(event) => void change("timeZone", { timeZone: event.target.value })}
              >
                <option value="auto">{t("settings.time_zone.auto", { zone: browserZone })}</option>
                {zones.map((zone) => (
                  <option key={zone} value={zone}>
                    {zone}
                  </option>
                ))}
              </NativeSelect>
            )}
          </FormField>
          <FormField label={t("settings.theme")} error={errorFor("theme")}>
            {(control) => (
              <NativeSelect
                {...control}
                disabled={disabled}
                value={preferences.theme}
                onChange={(event) => {
                  const theme = pick(THEMES, event.target.value);
                  if (theme) void change("theme", { theme });
                }}
              >
                <ThemeOptions />
              </NativeSelect>
            )}
          </FormField>
        </div>
      </CardContent>
    </Card>
  );
}

function AccountCard() {
  const { t } = useI18n();
  const { session } = useSession();
  return (
    <Card aria-labelledby="settings-account">
      <CardHeader>
        <CardTitle id="settings-account">{t("settings.account.title")}</CardTitle>
      </CardHeader>
      <CardContent>
        <dl className="grid gap-1 text-sm">
          <dt className="text-muted-foreground">{t("settings.account.email")}</dt>
          <dd className="flex flex-wrap items-center gap-2 font-medium break-all">
            {session?.user.email ?? t("settings.account.no_email")}
            {session?.user.platformAdmin ? (
              <span className="rounded-full border px-2 py-0.5 text-xs font-medium text-muted-foreground">
                {t("settings.account.platform_admin")}
              </span>
            ) : null}
          </dd>
        </dl>
      </CardContent>
    </Card>
  );
}

/** Signing out of every device, behind a confirmation: it cannot be undone from here. */
function SessionsCard() {
  const { t, errorMessage } = useI18n();
  const signOut = useSignOut();
  return (
    <Card aria-labelledby="settings-sessions">
      <CardHeader>
        <CardTitle id="settings-sessions">{t("settings.sessions.title")}</CardTitle>
        <CardDescription>{t("settings.sessions.description")}</CardDescription>
      </CardHeader>
      <CardContent>
        <AlertDialog onOpenChange={(open) => open && signOut.reset()}>
          <AlertDialogTrigger asChild>
            <Button variant="outline" className="text-destructive hover:text-destructive">
              <MonitorSmartphoneIcon />
              {t("settings.sessions.sign_out_everywhere")}
            </Button>
          </AlertDialogTrigger>
          <AlertDialogContent>
            <AlertDialogHeader>
              <AlertDialogTitle>{t("settings.sessions.confirm_title")}</AlertDialogTitle>
              <AlertDialogDescription>{t("settings.sessions.confirm_body")}</AlertDialogDescription>
            </AlertDialogHeader>
            {signOut.isError ? (
              <Alert variant="destructive">
                <AlertDescription>
                  {t("user_menu.sign_out_failed", { reason: errorMessage(signOut.error) })}
                </AlertDescription>
              </Alert>
            ) : null}
            <AlertDialogFooter>
              <AlertDialogCancel disabled={signOut.isPending}>{t("settings.sessions.cancel")}</AlertDialogCancel>
              <Button variant="destructive" disabled={signOut.isPending} onClick={() => signOut.mutate("everywhere")}>
                {signOut.isPending ? <Spinner /> : <LogOutIcon />}
                {signOut.isPending ? t("settings.sessions.signing_out") : t("settings.sessions.sign_out_everywhere")}
              </Button>
            </AlertDialogFooter>
          </AlertDialogContent>
        </AlertDialog>
      </CardContent>
    </Card>
  );
}
