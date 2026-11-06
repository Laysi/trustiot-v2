// Pending Laysi/trustiot-v2#156: replace with a module over the generated client when api/openapi.json has this endpoint.

/**
 * The grey-text completion endpoint (J11, second layer). Neither the endpoint, the `complete`
 * model role (Laysi/trustiot-v2#157) nor the ledger's unmetered usage mark (Laysi/trustiot-v2#140)
 * exist yet, and the issue does not give the shapes, so they are assumed here and listed in the
 * pull request that added this file:
 *
 * - `POST /api/assistant/v1/completions` with `CompleteRequest`, answered with `CompleteResponse`.
 *   The tenant and the user come from the session, as in every other assistant endpoint; the
 *   request never names them. The server builds the prompt from the tenant's device and data
 *   point names (prompt caching) and decides how many tokens to write, so the client sends none.
 * - Usage is limited per user, not per tenant, and does not count against the tenant's AI quota
 *   (the ledger records it with a mark instead). Over the limit the server answers 429
 *   `rate_limited` in the M1 error shape, with `details.retry_after_secs` saying when the next
 *   request is allowed. Every other failure is not shown to the user.
 * - The call can be aborted: the next key press cancels the request in flight (`signal`).
 */

export interface CompleteRequest {
  /** What the user has typed, up to the end (the caret is always there when a suggestion is asked for). */
  text: string;
}

export interface CompleteResponse {
  /** The words that continue `text`, a dozen tokens or so, with the spacing they need. Empty: nothing to suggest. */
  completion: string;
}

export interface CompletionApi {
  /** Rejects with the signal's reason once `signal` is aborted. */
  complete(request: CompleteRequest, options: { signal: AbortSignal }): Promise<CompleteResponse>;
}
