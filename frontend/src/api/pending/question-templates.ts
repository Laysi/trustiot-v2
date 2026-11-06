// Pending Laysi/trustiot-v2#155: replace with a module over the generated client when api/openapi.json has this endpoint.
import type { QuestionTemplate, TemplateSource } from "../../chat/completion/templates.tsx";

/**
 * The question templates of the composer (docs/design/home-ai/design.md, 「問句範本」). No endpoint
 * serves them yet: the semantic-layer tools exist (Laysi/trustiot-v2#142) but nothing lists the
 * questions they answer. This is the interface the composer is built against; the types are in
 * `chat/completion/templates.tsx` because the product's composer reads them and the build must not
 * carry this directory (`check:dist`). What the interface assumes, for the service to settle:
 *
 * - `GET /api/assistant/v1/question-templates?locale=<zh-TW|en>` answers `{ templates: [...] }`,
 *   readable by every role of the tenant (it is static text plus one example, never tenant data
 *   beyond the example's device name).
 * - Each template has an `id` that stays the same across languages, the `group` it is listed under,
 *   its `text` in the asked language with each blank written as `［label］`, the `slots` in order
 *   (the words in the brackets and what fills them: `device`, `point`, `model` or `range`), the
 *   `tool` that answers it, and an `example` with this tenant's own device filled in (`null` when it
 *   has none yet).
 * - Suggested questions (Laysi/trustiot-v2#158) are the same templates with the blanks filled in,
 *   so the text and the parameter format are the same on both sides.
 * - Nothing here asks an LLM, so the list is the same on-premises.
 */
export type QuestionTemplatesApi = TemplateSource;
export type { QuestionTemplate };
