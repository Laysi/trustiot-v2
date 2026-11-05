import { FileTextIcon } from "lucide-react";
import { Link } from "react-router";

import type { DocumentSummary } from "../../api/pending/documents.ts";
import { RowMenu } from "../../components/RowMenu.tsx";
import { Button, focusRing } from "../../components/ui/button.tsx";
import { useI18n } from "../../i18n/useI18n.ts";
import { cn } from "../../lib/utils.ts";
import { documentPath } from "./paths.ts";
import { VersionStatus } from "./VersionStatus.tsx";

/**
 * The documents as the design's table, which becomes one card per row on a narrow screen
 * (`max-sm:` classes change the display of the same elements, so a screen reader and a test read
 * one list, not two). A viewer has no actions column.
 */
export function DocumentTable({
  documents,
  modelLabel,
  canWrite,
  me,
  formatTime,
  onRetry,
  onNewVersion,
  onDownload,
  onDelete,
}: {
  documents: readonly DocumentSummary[];
  modelLabel(modelId: string): string;
  canWrite: boolean;
  me: string | undefined;
  formatTime(instant: string): string;
  onRetry(document: DocumentSummary): void;
  onNewVersion(document: DocumentSummary): void;
  onDownload(document: DocumentSummary): void;
  onDelete(document: DocumentSummary): void;
}) {
  const { t } = useI18n();
  const cell = "px-3 py-3 align-top max-sm:block max-sm:px-0 max-sm:py-0";
  const labelled =
    "max-sm:before:mr-2 max-sm:before:text-xs max-sm:before:text-muted-foreground max-sm:before:content-[attr(data-label)]";
  return (
    <section className="rounded-lg border bg-card text-card-foreground">
      <table className="w-full text-sm max-sm:block">
        <caption className="sr-only">{t("documents.caption", { count: documents.length })}</caption>
        <thead className="bg-muted/60 text-left text-muted-foreground max-sm:sr-only">
          <tr>
            <th scope="col" className="px-4 py-2.5 font-medium">
              {t("documents.column.document")}
            </th>
            <th scope="col" className="px-3 py-2.5 font-medium whitespace-nowrap">
              {t("documents.column.model")}
            </th>
            <th scope="col" className="px-3 py-2.5 font-medium whitespace-nowrap">
              {t("documents.column.version")}
            </th>
            <th scope="col" className="px-3 py-2.5 font-medium">
              {t("documents.column.status")}
            </th>
            <th scope="col" className="px-3 py-2.5 font-medium whitespace-nowrap max-sm:hidden">
              {t("documents.column.uploaded")}
            </th>
            {canWrite ? (
              <th scope="col" className="px-4 py-2.5">
                <span className="sr-only">{t("documents.column.actions")}</span>
              </th>
            ) : null}
          </tr>
        </thead>
        <tbody className="max-sm:block">
          {documents.map((document) => {
            const version = document.latest_version;
            return (
              <tr key={document.id} className="border-t max-sm:grid max-sm:gap-1.5 max-sm:px-4 max-sm:py-3">
                <th
                  scope="row"
                  className="px-4 py-3 text-left align-top font-normal max-sm:block max-sm:px-0 max-sm:py-0"
                >
                  <span className="flex items-start gap-2">
                    <FileTextIcon aria-hidden="true" className="mt-0.5 size-4 shrink-0 text-muted-foreground" />
                    <span className="grid min-w-0 gap-0.5">
                      <Link
                        to={documentPath(document.id, { version: version.version })}
                        className={cn(
                          "rounded-sm font-medium wrap-anywhere underline-offset-4 hover:underline",
                          focusRing,
                        )}
                      >
                        {document.name}
                      </Link>
                      <span className="text-xs text-muted-foreground">
                        {version.page_count === null
                          ? t("documents.pages_counting")
                          : t("documents.pages", { count: version.page_count })}
                      </span>
                    </span>
                  </span>
                </th>
                <td data-label={t("documents.column.model")} className={cn(cell, labelled)}>
                  {modelLabel(document.model_id)}
                </td>
                <td data-label={t("documents.column.version")} className={cn(cell, "whitespace-nowrap", labelled)}>
                  {t("documents.version", { version: version.version })}
                </td>
                <td className={cell}>
                  <VersionStatus
                    name={document.name}
                    version={version}
                    actions={
                      canWrite ? (
                        <span className="flex flex-wrap gap-x-3">
                          <Button
                            variant="link"
                            size="sm"
                            className="h-auto text-xs"
                            aria-label={t("documents.retry_named", { name: document.name })}
                            onClick={() => onRetry(document)}
                          >
                            {t("page.action.retry")}
                          </Button>
                          <Button
                            variant="link"
                            size="sm"
                            className="h-auto text-xs"
                            aria-label={t("documents.new_version_named", { name: document.name })}
                            onClick={() => onNewVersion(document)}
                          >
                            {t("documents.new_version")}
                          </Button>
                        </span>
                      ) : undefined
                    }
                  />
                </td>
                <td className={cn(cell, "text-xs max-sm:hidden")}>
                  <span className="block text-sm">
                    {version.uploaded_by === me ? t("documents.uploader.you") : t("documents.uploader.other")}
                  </span>
                  <time dateTime={version.uploaded_at} className="text-muted-foreground tabular-nums">
                    {formatTime(version.uploaded_at)}
                  </time>
                </td>
                {canWrite ? (
                  <td className="px-4 py-3 text-right align-top max-sm:block max-sm:px-0 max-sm:py-0 max-sm:text-left">
                    <RowMenu
                      label={t("documents.actions_named", { name: document.name })}
                      items={[
                        { kind: "action", label: t("documents.new_version"), onSelect: () => onNewVersion(document) },
                        { kind: "action", label: t("documents.download"), onSelect: () => onDownload(document) },
                        {
                          kind: "action",
                          label: t("documents.delete.menu"),
                          destructive: true,
                          onSelect: () => onDelete(document),
                        },
                      ]}
                    />
                  </td>
                ) : null}
              </tr>
            );
          })}
        </tbody>
      </table>
    </section>
  );
}
