import { useMutation } from "@tanstack/react-query";
import { ArrowLeftIcon, CircleCheckIcon, GlobeIcon, MailIcon, TriangleAlertIcon } from "lucide-react";
import { type FormEvent, useEffect, useId, useRef, useState } from "react";
import { useLocation, useNavigate, useSearchParams } from "react-router";

import { ApiError } from "../api/errors.ts";
import { useIdentityApi } from "../api/IdentityApiProvider.tsx";
import type { CodeSent } from "../api/identity.ts";
import { Brand } from "../app/Layout.tsx";
import { safeNextPath } from "../app/paths.ts";
import { HANDLES_UNAUTHENTICATED } from "../app/query-client.ts";
import { Alert, AlertDescription } from "../components/ui/alert.tsx";
import { Button } from "../components/ui/button.tsx";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "../components/ui/card.tsx";
import { FormField } from "../components/ui/form-field.tsx";
import { Input } from "../components/ui/input.tsx";
import { NativeSelect } from "../components/ui/native-select.tsx";
import { Spinner } from "../components/ui/spinner.tsx";
import type { MessageId } from "../i18n/messages.ts";
import { useI18n } from "../i18n/useI18n.ts";
import { LanguageOptions, pick } from "../preferences/options.tsx";
import { usePreferences } from "../preferences/PreferencesProvider.tsx";
import { LANGUAGES } from "../preferences/preferences.ts";
import { useSession } from "../session/SessionProvider.tsx";
import type { SessionInfo } from "../session/session.ts";
import { SIGN_OUT_EVERYWHERE_UNFINISHED, signedOutScope, signOutEverywhereUnfinished } from "../session/useSignOut.ts";

/** The code arrived by email: where it went, and what the server said about it. */
interface CodeStep {
  email: string;
  sent: CodeSent;
  /** When it was sent (ms), for the wait before another may be sent. */
  sentAt: number;
  resent: boolean;
}

/**
 * Sign-in with a code sent by email (services/identity/README.md 「登入」): the address, then the
 * code. The page says the same thing whether or not the address has an account ("if … has an
 * account"), as the server does. After sign-in it goes to `next` when that is a page of this app.
 */
export function LoginPage() {
  const { t } = useI18n();
  const location = useLocation();
  const [params] = useSearchParams();
  const next = safeNextPath(params.get("next"));
  const signedOut = signedOutScope(location.state);
  const unfinished = signOutEverywhereUnfinished(location.state);
  const [step, setStep] = useState<CodeStep | null>(null);

  return (
    <div className="flex min-h-dvh flex-col items-center justify-center gap-6 bg-muted px-4 py-10">
      <main id="main" className="grid w-full max-w-sm gap-6">
        <Brand className="justify-self-center text-lg" />
        {signedOut && step === null ? (
          <Alert role="status">
            <CircleCheckIcon className="text-success" />
            <AlertDescription className="text-foreground">
              {t(signedOut === "everywhere" ? "page.login.signed_out_everywhere" : "page.login.signed_out")}
            </AlertDescription>
          </Alert>
        ) : null}
        {unfinished ? (
          // Through both steps: it is what the user has to do once signed in.
          <Alert variant="destructive">
            <TriangleAlertIcon />
            <AlertDescription>{t("page.login.sign_out_everywhere_unfinished")}</AlertDescription>
          </Alert>
        ) : null}
        {step === null ? (
          <EmailStep onSent={setStep} />
        ) : (
          // Keyed by the send time: a new code starts the step afresh, without the old code or error.
          <CodeStepForm
            key={step.sentAt}
            step={step}
            next={next}
            arrivalState={unfinished ? SIGN_OUT_EVERYWHERE_UNFINISHED : undefined}
            onResent={setStep}
            onBack={() => setStep(null)}
          />
        )}
      </main>
      <LanguagePicker />
    </div>
  );
}

function useSendCode() {
  const identity = useIdentityApi();
  return useMutation({ mutationFn: (email: string) => identity.sendCode(email), meta: HANDLES_UNAUTHENTICATED });
}

function EmailStep({ onSent }: { onSent: (step: CodeStep) => void }) {
  const { t, errorMessage, fieldError } = useI18n();
  const send = useSendCode();
  const [email, setEmail] = useState("");
  const [localProblem, setLocalProblem] = useState<MessageId | null>(null);

  const submit = (event: FormEvent) => {
    event.preventDefault();
    const address = email.trim();
    send.reset();
    if (address === "") {
      setLocalProblem("login.email_invalid");
      return;
    }
    setLocalProblem(null);
    send.mutate(address, {
      onSuccess: (sent) => onSent({ email: address, sent, sentAt: Date.now(), resent: false }),
    });
  };

  const emailError = localProblem ? t(localProblem) : fieldError(send.error, "/email");
  const formError = send.isError && emailError === undefined ? errorMessage(send.error) : null;
  return (
    <Card>
      <CardHeader>
        <CardTitle as="h1">{t("page.login.title")}</CardTitle>
        <CardDescription>{t("page.login.email_intro")}</CardDescription>
      </CardHeader>
      <CardContent>
        <form noValidate onSubmit={submit} className="grid gap-4">
          {formError ? (
            <Alert variant="destructive">
              <AlertDescription>{formError}</AlertDescription>
            </Alert>
          ) : null}
          <FormField label={t("page.login.email")} error={emailError}>
            {(control) => (
              <Input
                {...control}
                type="email"
                name="email"
                autoComplete="email"
                inputMode="email"
                spellCheck={false}
                value={email}
                onChange={(event) => {
                  setEmail(event.target.value);
                  setLocalProblem(null);
                }}
              />
            )}
          </FormField>
          <Button type="submit" disabled={send.isPending} className="w-full">
            {send.isPending ? (
              <>
                <Spinner />
                {t("page.login.sending")}
              </>
            ) : (
              <>
                <MailIcon />
                {t("page.login.send_code")}
              </>
            )}
          </Button>
        </form>
      </CardContent>
    </Card>
  );
}

/** Seconds until another code may be sent, counting down while the page is open. */
function useSecondsUntil(deadline: number): number {
  const [now, setNow] = useState(() => Date.now());
  const left = Math.max(0, Math.ceil((deadline - now) / 1000));
  useEffect(() => {
    if (left === 0) return;
    const timer = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(timer);
  }, [left]);
  return left;
}

function CodeStepForm({
  step,
  next,
  arrivalState,
  onResent,
  onBack,
}: {
  step: CodeStep;
  next: string;
  /** The history state of the page reached after sign-in, which reads it (SettingsPage). */
  arrivalState: unknown;
  onResent: (step: CodeStep) => void;
  onBack: () => void;
}) {
  const { t, errorMessage, fieldError } = useI18n();
  const identity = useIdentityApi();
  const navigate = useNavigate();
  const { signedIn, session } = useSession();
  const send = useSendCode();
  const [code, setCode] = useState("");
  const [localProblem, setLocalProblem] = useState<MessageId | null>(null);
  const [cookieMissing, setCookieMissing] = useState(false);
  const [arrival, setArrival] = useState<{ session: SessionInfo; path: string } | null>(null);
  const input = useRef<HTMLInputElement>(null);
  const length = step.sent.codeLength;
  const resendIn = useSecondsUntil(step.sentAt + step.sent.resendAfterSecs * 1000);

  const verify = useMutation({
    mutationFn: (entered: string) => identity.verifyCode({ email: step.email, code: entered, redirectTo: next }),
    meta: HANDLES_UNAUTHENTICATED,
    onSuccess: async ({ redirectTo }) => {
      const signedInAs = await signedIn();
      if (signedInAs === null) {
        setCookieMissing(true);
        return;
      }
      // The server checked redirect_to; it is checked again here and followed by the router, never
      // by the browser, so a path that decodes into another host cannot leave the app.
      setArrival({ session: signedInAs, path: safeNextPath(redirectTo) });
    },
  });

  // The page moves only once the session the pages read is the new one: the query cache tells its
  // observers on a later tick, and the signed-in layout would otherwise still see no session and
  // send the user back here.
  useEffect(() => {
    if (arrival !== null && session === arrival.session) {
      void navigate(arrival.path, { replace: true, state: arrivalState });
    }
  }, [arrival, session, navigate, arrivalState]);

  // The code field takes focus when this step opens (a new code opens it afresh: see LoginPage).
  useEffect(() => {
    input.current?.focus();
  }, []);

  const submit = (event: FormEvent) => {
    event.preventDefault();
    verify.reset();
    setCookieMissing(false);
    if (code.length !== length) {
      setLocalProblem("login.code_invalid");
      return;
    }
    setLocalProblem(null);
    verify.mutate(code);
  };

  const resend = () => {
    verify.reset();
    send.mutate(step.email, {
      onSuccess: (sent) => onResent({ email: step.email, sent, sentAt: Date.now(), resent: true }),
    });
  };

  const rejected = verify.error instanceof ApiError && verify.error.status === 401;
  const codeError = localProblem
    ? t(localProblem)
    : rejected
      ? t("page.login.code_rejected")
      : fieldError(verify.error, "/code");
  const verifyProblem =
    verify.isError && codeError === undefined
      ? (fieldError(verify.error, "/redirect_to") ?? fieldError(verify.error, "/email") ?? errorMessage(verify.error))
      : null;
  const formError = cookieMissing
    ? t("page.login.cookie_missing")
    : (verifyProblem ?? (send.isError ? errorMessage(send.error) : null));
  const busy = verify.isPending || arrival !== null;

  return (
    <Card>
      <CardHeader>
        <CardTitle as="h1">{t("page.login.code_title")}</CardTitle>
        <CardDescription>
          {t("page.login.code_intro", {
            email: step.email,
            length,
            minutes: Math.max(1, Math.round(step.sent.expiresInSecs / 60)),
          })}
        </CardDescription>
      </CardHeader>
      <CardContent>
        <form noValidate onSubmit={submit} className="grid gap-4">
          {step.resent && !send.isError ? (
            <Alert role="status">
              <CircleCheckIcon className="text-success" />
              <AlertDescription className="text-foreground">
                {t("page.login.resent", { email: step.email })}
              </AlertDescription>
            </Alert>
          ) : null}
          {formError ? (
            <Alert variant="destructive">
              <AlertDescription>{formError}</AlertDescription>
            </Alert>
          ) : null}
          <FormField label={t("page.login.code")} description={t("page.login.code_hint", { length })} error={codeError}>
            {(control) => (
              <Input
                {...control}
                ref={input}
                name="code"
                inputMode="numeric"
                autoComplete="one-time-code"
                pattern="[0-9]*"
                className="h-11 text-center font-mono text-lg tracking-[0.4em] md:text-lg"
                value={code}
                onChange={(event) => {
                  // Pasted codes often carry spaces or a hyphen ("1234 5678"): keep the digits.
                  setCode(event.target.value.replace(/\D/g, "").slice(0, length));
                  setLocalProblem(null);
                }}
              />
            )}
          </FormField>
          <Button type="submit" disabled={busy} className="w-full">
            {busy ? (
              <>
                <Spinner />
                {t("page.login.verifying")}
              </>
            ) : (
              t("page.login.verify")
            )}
          </Button>
          <div className="flex flex-wrap items-center justify-between gap-2 text-sm">
            <Button variant="link" size="sm" onClick={onBack}>
              <ArrowLeftIcon />
              {t("page.login.change_email")}
            </Button>
            <Button variant="link" size="sm" disabled={resendIn > 0 || send.isPending} onClick={resend}>
              {resendIn > 0 ? t("page.login.resend_in", { seconds: resendIn }) : t("page.login.resend")}
            </Button>
          </div>
        </form>
      </CardContent>
    </Card>
  );
}

/**
 * The interface language before sign-in, kept only in this browser; after sign-in the account's
 * setting applies (services/identity/README.md 「偏好設定」).
 */
function LanguagePicker() {
  const { t } = useI18n();
  const { preferences, update } = usePreferences();
  const id = useId();
  return (
    <div className="flex flex-wrap items-center justify-center gap-2 text-sm text-muted-foreground">
      <GlobeIcon className="size-4" />
      <label htmlFor={id}>{t("page.login.language")}</label>
      {/* Sized by its longest option, which differs by language, so no label is clipped. */}
      <div className="min-w-0 max-w-full">
        <NativeSelect
          id={id}
          className="h-8 w-auto max-w-full"
          value={preferences.language}
          onChange={(event) => {
            const language = pick(LANGUAGES, event.target.value);
            if (language) void update({ language });
          }}
        >
          <LanguageOptions />
        </NativeSelect>
      </div>
    </div>
  );
}
