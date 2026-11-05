import { createContext, type ReactNode, useContext } from "react";

import { useSessionBound } from "../../session/PinSession.tsx";
import type { MentionKind } from "./text.ts";

/** A blank of a question template and what fills it. */
export interface TemplateSlot {
  /** The words in the brackets of the template's text. */
  label: string;
  kind: MentionKind;
}

/**
 * A question template (design 「問句範本」): the semantic-layer tool it asks, and the question with
 * its blanks written as `［設備］`. The suggested questions of Laysi/trustiot-v2#158 are the same
 * templates with the blanks filled in.
 */
export interface QuestionTemplate {
  id: string;
  /** The heading it is listed under: the kind of question (energy use, anomalies and lost links). */
  group: string;
  /** The question, in the language the list was asked for, each blank as `［label］`. */
  text: string;
  slots: TemplateSlot[];
  /** The semantic-layer tool that answers it, as a person reads it. */
  tool: string;
  /** The question with this tenant's own device filled in, or `null` when there is no device yet. */
  example: string | null;
}

/** Where the composer gets the templates from; the endpoint is pending (api/pending/question-templates.ts). */
export interface TemplateSource {
  listTemplates(locale: string, signal?: AbortSignal): Promise<QuestionTemplate[]>;
}

const TemplateSourceContext = createContext<TemplateSource | null>(null);

/**
 * Says to the composer that templates are on offer and where they come from. The product mounts
 * none until the service lists them, so its composer has no template button and no `/`; the UI
 * preview and the tests mount a fake (api/pending/QuestionTemplatesProvider.tsx). It lives here,
 * not next to the pending API, because the composer reads it: the build must not carry
 * `src/api/pending/` (`check:dist`).
 */
export function TemplateSourceProvider({ source, children }: { source: TemplateSource; children: ReactNode }) {
  return <TemplateSourceContext.Provider value={source}>{children}</TemplateSourceContext.Provider>;
}

/** The template source, bound to the page (`PinSession`), or `null` where the build offers no templates. */
export function useTemplateSource(): TemplateSource | null {
  const value = useContext(TemplateSourceContext);
  const bound = useSessionBound(value ?? NO_SOURCE);
  return value === null ? null : bound;
}

const NO_SOURCE: TemplateSource = { listTemplates: () => Promise.resolve([]) };
