import { Spinner } from "../components/ui/spinner.tsx";
import { useI18n } from "../i18n/useI18n.ts";

/** A whole page that is not there yet: the session is being read, or the page's code is loading. */
export function PageLoading() {
  const { t } = useI18n();
  return (
    <div className="flex min-h-dvh items-center justify-center">
      <Spinner label={t("app.loading")} />
    </div>
  );
}
