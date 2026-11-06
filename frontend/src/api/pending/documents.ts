// Pending Laysi/trustiot-v2#193: replace with a module over the generated client when api/openapi.json has these endpoints.

// The knowledge service's document API as the document library screens need it (docs/design/ms4-ms5/
// design.md, section 五; Laysi/trustiot-v2#193 for documents, versions and processing status,
// Laysi/trustiot-v2#198 for the pages a citation opens). None of it is in api/openapi.json yet, so the
// shapes below come from the design and the two issues' scope; what neither states is an assumption,
// listed in the pull request of Laysi/trustiot-v2#199. The refusals follow the repository's M1 error
// body (`ApiError`) and the entitlement refusals of docs/product/plans.md:
//
// - A plan without document RAG: 403 `quota_exceeded` with the message key
//   `entitlement.feature_not_in_plan` (any call).
// - An upload that exceeds the plan's capacity: 403 `quota_exceeded`, `entitlement.limit_reached`.
// - A viewer uploading, retrying or deleting: 403 `permission_denied` (Laysi/trustiot-v2#193).
// - A document that never existed: 404 `not_found`. One that was deleted is still answered, as a
//   tombstone with `deleted_at` (decision Q13), so a citation can say it was deleted.

/** What the AI can do with a version: only a `ready` one is read (design.md 「列表」). */
export type DocumentStatus = "queued" | "processing" | "ready" | "failed";

/** Why a version is `failed`; `page` is the page that could not be processed, if one is named. */
export interface DocumentFailure {
  page: number | null;
  /** A message key the page translates when it knows it (`MessageKey` format), else a generic text. */
  message_key: string;
}

export interface DocumentVersionInfo {
  version: number;
  status: DocumentStatus;
  /** `null` until the pages are counted. */
  page_count: number | null;
  /** Pages already turned into images and indexed; for `processing` it is the progress. */
  processed_pages: number;
  size_bytes: number;
  /** The user ID; the API names users by ID only, as the dashboard API does. */
  uploaded_by: string;
  uploaded_at: string;
  failure: DocumentFailure | null;
}

/** One row of the library: the document and its latest version. */
export interface DocumentSummary {
  id: string;
  /** The uploaded file's name without `.pdf`. Text, never markup. */
  name: string;
  model_id: string;
  latest_version: DocumentVersionInfo;
}

export interface DocumentUsage {
  used_bytes: number;
  limit_bytes: number;
}

/** The tenant's documents, every one (the list is small and the screen filters it), and the capacity used. */
export interface DocumentList {
  documents: DocumentSummary[];
  usage: DocumentUsage;
}

/** A document with all its versions, newest first; a deleted one is a tombstone (no versions). */
export interface DocumentDetail {
  id: string;
  name: string;
  model_id: string;
  deleted_at: string | null;
  versions: DocumentVersionInfo[];
}

/** One page image; `image_url` is a path of this site (the page's CSP only loads `'self'` images). */
export interface DocumentPage {
  page: number;
  /** The image's size in pixels; a citation's crop box is in these units. */
  width: number;
  height: number;
  image_url: string;
}

/** Where an upload goes: a new document of the model, or a new version of `documentId`. */
export interface UploadRequest {
  modelId: string;
  documentId?: string;
}

export interface UploadOptions {
  /** Called with the share (0 to 1) of the file sent so far. */
  onProgress(fraction: number): void;
  /** Aborting rejects the upload with an `AbortError` and sends nothing more. */
  signal: AbortSignal;
}

/**
 * The calls of the document library screens. `upload` hides the mechanism (the pre-signed
 * direct upload of decision Q14: ask for a target, send the file to it, report it complete) and
 * resolves with the document it created or added a version to, queued for processing.
 */
export interface DocumentApi {
  listDocuments(signal?: AbortSignal): Promise<DocumentList>;
  getDocument(documentId: string, signal?: AbortSignal): Promise<DocumentDetail>;
  getPage(request: { documentId: string; version: number; page: number }, signal?: AbortSignal): Promise<DocumentPage>;
  upload(request: UploadRequest, file: File, options: UploadOptions): Promise<DocumentSummary>;
  /** Processes a failed version again; the pages already done are kept. */
  retryProcessing(request: { documentId: string; version: number }): Promise<void>;
  /** Deletes every version, the page images and the index; the name stays as a tombstone. */
  deleteDocument(documentId: string): Promise<void>;
  /** A short-lived address of the original PDF of a version. */
  getDownloadUrl(request: { documentId: string; version: number }, signal?: AbortSignal): Promise<string>;
}
