// The channel between two UI preview fakes (frontend/README.md, 「UI 預覽」): the questions fake
// (fake-questions.ts) answers the conversation paths, the dashboards fake (fake-dashboards.ts) holds
// the drafts, and a question that writes a draft (`!draft`) needs both: the new conversation's ID
// only exists at runtime, and each fake is its own declaration. The dashboards fake registers how a
// draft is written when the preview creates it; the questions fake looks the writer up when the
// scripted model "calls its dashboard tool", so a rebuilt preview (the toolbar's seed switch) is
// always the one that answers. Not a `fake-*.ts` file: the preview loads those as declarations.

/** The draft version an answer names (the API's `AnswerDraftVersion`). */
export interface WrittenDraft {
  dashboard_id: string;
  version: number;
}

/** Writes a draft of `conversationId` for `question`, as the assistant's dashboard tool does. */
export type DraftWriter = (conversationId: string, question: string) => WrittenDraft;

let current: DraftWriter | null = null;

/** The dashboards fake says how a draft is written; the last registration is the live preview's. */
export function registerPreviewDraftWriter(writer: DraftWriter): void {
  current = writer;
}

/** The writer of the preview in use; a `!draft` question without a dashboards fake is a mistake in the preview. */
export function previewDraftWriter(): DraftWriter {
  if (current === null) {
    throw new Error("UI preview: a `!draft` question needs the dashboards fake (fake-dashboards.ts) to hold the draft");
  }
  return current;
}
