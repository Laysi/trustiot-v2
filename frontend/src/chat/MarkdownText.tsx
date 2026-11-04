import { MarkdownTextPrimitive } from "@assistant-ui/react-markdown";
import { type ComponentProps, createContext, useContext, useMemo } from "react";
import { Link } from "react-router";
import remarkGfm from "remark-gfm";

import { fromConversationState } from "../app/back-to-conversation.ts";
import { spaPath } from "../app/paths.ts";
import { focusRing } from "../components/ui/button.tsx";
import { useI18n } from "../i18n/useI18n.ts";
import { cn } from "../lib/utils.ts";
import { ALLOWED_ELEMENTS, answerUrlTransform, IMAGE_TEXT_CLASS, rehypeSafeAnswer } from "./safe-markdown.ts";

/**
 * Where a link in an answer was opened from: the conversation, so that the page it opens can
 * lead back to it (docs/design/app-shell.md, 「原則」; app/back-to-conversation.ts).
 */
export const AnswerOriginContext = createContext<{ conversationId: string } | null>(null);

const REMARK_PLUGINS = [remarkGfm];

/** A link of an answer: only a path of this SPA (safe-markdown.ts made every other one text). */
function AnswerLink({ href, children }: ComponentProps<"a">) {
  const origin = useContext(AnswerOriginContext);
  const path = spaPath(href);
  if (path === null) return <span>{children}</span>;
  return (
    <Link
      to={path}
      state={origin ? fromConversationState(origin.conversationId) : undefined}
      className={cn("rounded-sm font-medium text-primary underline underline-offset-4", focusRing)}
    >
      {children}
    </Link>
  );
}

/** The table of an answer, in a frame that scrolls sideways on a narrow screen. */
function AnswerTable({ children }: ComponentProps<"table">) {
  return (
    <div className="relative my-1 w-full overflow-x-auto rounded-lg border">
      <table className="w-full text-sm">{children}</table>
    </div>
  );
}

/**
 * Every element an answer may have, drawn with the design system's type and colors; none of them
 * passes on an attribute from the Markdown except what safe-markdown.ts kept. An image never gets
 * here (it is text by then), so `img` draws nothing in case it ever did.
 */
const COMPONENTS = {
  a: AnswerLink,
  img: () => null,
  p: ({ children }: ComponentProps<"p">) => <p>{children}</p>,
  h1: ({ children }: ComponentProps<"h1">) => <h3 className="text-xl font-semibold">{children}</h3>,
  h2: ({ children }: ComponentProps<"h2">) => <h4 className="text-lg font-semibold">{children}</h4>,
  h3: ({ children }: ComponentProps<"h3">) => <h5 className="text-base font-semibold">{children}</h5>,
  h4: ({ children }: ComponentProps<"h4">) => <h6 className="text-base font-semibold">{children}</h6>,
  h5: ({ children }: ComponentProps<"h5">) => <h6 className="text-base font-semibold">{children}</h6>,
  h6: ({ children }: ComponentProps<"h6">) => <h6 className="text-base font-semibold">{children}</h6>,
  ul: ({ children }: ComponentProps<"ul">) => <ul className="grid list-disc gap-1 pl-6">{children}</ul>,
  ol: ({ children, start }: ComponentProps<"ol">) => (
    <ol start={start} className="grid list-decimal gap-1 pl-6">
      {children}
    </ol>
  ),
  li: ({ children }: ComponentProps<"li">) => <li>{children}</li>,
  blockquote: ({ children }: ComponentProps<"blockquote">) => (
    <blockquote className="grid gap-2 border-l-2 pl-4 text-muted-foreground">{children}</blockquote>
  ),
  hr: () => <hr className="border-border" />,
  table: AnswerTable,
  thead: ({ children }: ComponentProps<"thead">) => (
    <thead className="bg-muted/60 text-left text-muted-foreground">{children}</thead>
  ),
  tr: ({ children }: ComponentProps<"tr">) => <tr className="border-t first:border-t-0">{children}</tr>,
  th: ({ children, className }: ComponentProps<"th">) => (
    <th scope="col" className={cn("px-3 py-2 font-medium", className)}>
      {children}
    </th>
  ),
  td: ({ children, className }: ComponentProps<"td">) => <td className={cn("px-3 py-2", className)}>{children}</td>,
  pre: ({ children }: ComponentProps<"pre">) => (
    <pre className="overflow-x-auto rounded-lg border bg-muted p-3 text-sm leading-6 [&_code]:bg-transparent [&_code]:p-0">
      {children}
    </pre>
  ),
  code: ({ children, className }: ComponentProps<"code">) => (
    <code className={cn("rounded-sm bg-muted px-1 py-0.5 font-mono text-[0.875em]", className)}>{children}</code>
  ),
  span: ({ children, className }: ComponentProps<"span">) =>
    className === IMAGE_TEXT_CLASS ? <span className="text-muted-foreground italic">{children}</span> : children,
};

/**
 * An answer's text as Markdown, through assistant-ui's official Markdown component
 * (`MarkdownTextPrimitive`, react-markdown with GFM) under the rules of safe-markdown.ts. It is a
 * message part of the assistant-ui thread (Thread.tsx), which gives it the text.
 */
export function MarkdownText() {
  const { t } = useI18n();
  const rehypePlugins = useMemo<ComponentProps<typeof MarkdownTextPrimitive>["rehypePlugins"]>(
    () => [
      [
        rehypeSafeAnswer,
        {
          imageText: (alt: string) =>
            alt.trim() === "" ? t("chat.answer.image") : t("chat.answer.image_named", { alt }),
        },
      ],
    ],
    [t],
  );
  return (
    <MarkdownTextPrimitive
      smooth={false}
      remarkPlugins={REMARK_PLUGINS}
      rehypePlugins={rehypePlugins}
      allowedElements={ALLOWED_ELEMENTS}
      unwrapDisallowed
      urlTransform={answerUrlTransform}
      components={COMPONENTS}
      className="grid max-w-3xl gap-3 text-base leading-7 wrap-anywhere"
    />
  );
}
