import { ImageOffIcon } from "lucide-react";
import { useState } from "react";

import { Button } from "../../components/ui/button.tsx";
import { useI18n } from "../../i18n/useI18n.ts";
import { cn } from "../../lib/utils.ts";
import { type Box, parseBox } from "./box.ts";
import { usePageInfo } from "./usePageInfo.ts";

/** Where on the page the cited area is, for the alternative text (the image has no readable text, decision Q12). */
function position(box: Box, height: number): "top" | "middle" | "bottom" {
  const centre = (box.y + box.height / 2) / height;
  return centre < 1 / 3 ? "top" : centre < 2 / 3 ? "middle" : "bottom";
}

/** The width of `text` at `size` in the label's units: a full-width character is one size, others about 0.6. */
function textWidth(text: string, size: number): number {
  return Array.from(text).reduce((sum, char) => sum + ((char.codePointAt(0) ?? 0) > 255 ? 1 : 0.6), 0) * size;
}

/**
 * One page image with the area the AI cited drawn over it (design.md 「頁面檢視」): the frame,
 * the dimmed rest of the page and the label are an SVG whose `viewBox` is the image's pixels, so
 * the `box` parameter is used as it is and the CSP's ban on the `style` attribute does not matter.
 * The image's alternative text says the page and where the area is; what the page says is for the
 * original PDF (decision Q12), which the viewer offers.
 */
export function PageImage({
  documentId,
  version,
  page,
  rawBox,
  zoomed,
}: {
  documentId: string;
  version: number;
  page: number;
  /** The `box` parameter of the address: the cited area in the page's pixels, checked here against its size. */
  rawBox: string | null;
  zoomed: boolean;
}) {
  const { t, errorMessage } = useI18n();
  const [broken, setBroken] = useState<string | null>(null);
  const info = usePageInfo(documentId, version, page);

  if (info.isPending) {
    return (
      <div aria-busy="true" className="aspect-[794/1123] w-full animate-pulse rounded-md bg-muted">
        <p role="status" className="sr-only">
          {t("documents.viewer.page_loading", { page })}
        </p>
      </div>
    );
  }
  if (info.data === undefined || broken === info.data.image_url) {
    return (
      <div
        role="alert"
        className="grid justify-items-center gap-2 rounded-md border border-destructive/50 px-4 py-12 text-center text-sm"
      >
        <ImageOffIcon aria-hidden="true" className="size-6 text-destructive" />
        <p>
          {info.data === undefined
            ? t("documents.viewer.page_failed", { page, reason: errorMessage(info.error) })
            : t("documents.viewer.image_failed", { page })}
        </p>
        <Button
          variant="outline"
          size="sm"
          onClick={() => {
            setBroken(null);
            void info.refetch();
          }}
        >
          {t("page.action.retry")}
        </Button>
      </div>
    );
  }

  const { width, height, image_url: src } = info.data;
  const cited = parseBox(rawBox, width, height);
  const size = Math.round(width * 0.022);
  const label = t("documents.viewer.cited_label");
  const labelWidth = textWidth(label, size) + size;
  const labelY = cited === null ? 0 : cited.y - size * 1.8 >= 0 ? cited.y - size * 1.8 : cited.y;
  const alt =
    cited === null
      ? t("documents.viewer.page_alt", { page })
      : t("documents.viewer.page_alt_cited", {
          page,
          position: t(`documents.viewer.position_${position(cited, height)}`),
        });
  return (
    <figure className={cn("relative m-0", zoomed ? "w-[200%] max-w-none" : "w-full")}>
      <img
        src={src}
        alt={alt}
        width={width}
        height={height}
        className="block h-auto w-full rounded-md border bg-white"
        onError={() => setBroken(src)}
      />
      {cited !== null ? (
        <svg
          aria-hidden="true"
          focusable="false"
          viewBox={`0 0 ${width} ${height}`}
          className="absolute inset-0 size-full"
          data-testid="cited-area"
        >
          <path
            fillRule="evenodd"
            className="fill-black/40"
            d={`M0 0H${width}V${height}H0Z M${cited.x} ${cited.y}V${cited.y + cited.height}H${cited.x + cited.width}V${cited.y}Z`}
          />
          <rect
            x={cited.x}
            y={cited.y}
            width={cited.width}
            height={cited.height}
            strokeWidth={3}
            vectorEffect="non-scaling-stroke"
            className="fill-none stroke-brand"
          />
          <rect x={cited.x} y={labelY} width={labelWidth} height={size * 1.6} className="fill-brand" />
          <text x={cited.x + size / 2} y={labelY + size * 1.15} fontSize={size} className="fill-on-brand font-semibold">
            {label}
          </text>
        </svg>
      ) : null}
    </figure>
  );
}
