import { type ComponentType, createElement, type ReactNode, useEffect } from "react";

import { DeviceApiProvider } from "../api/DeviceApiProvider.tsx";
import { ApiError, type ErrorBody } from "../api/errors.ts";
import { DocumentApiProvider } from "../api/pending/DocumentApiProvider.tsx";
import type {
  DocumentApi,
  DocumentDetail,
  DocumentFailure,
  DocumentList,
  DocumentPage,
  DocumentStatus,
  DocumentSummary,
  DocumentVersionInfo,
  UploadOptions,
  UploadRequest,
} from "../api/pending/documents.ts";
import { FakeDeviceServer, fakeUuid } from "./fake-devices.ts";

/**
 * The knowledge service's document API (Laysi/trustiot-v2#193, still pending: api/pending/
 * documents.ts), held in memory. It implements the interface itself, not an HTTP layer, because
 * the endpoints do not exist to answer at that level. The rules are the ones that file lists:
 *
 * - Every call is refused with 403 `quota_exceeded` / `entitlement.feature_not_in_plan` while
 *   `plan` is `"starter"`; an upload larger than the capacity left with `entitlement.limit_reached`.
 * - A document that never existed is 404 `not_found`; a deleted one stays as a tombstone (name,
 *   `deleted_at`, no versions) and leaves the list.
 * - An upload with a `documentId` adds a version to that document, otherwise a new document is
 *   made; the result is queued. Processing moves on only through `advance` (a test) or `autoProcess`
 *   (the UI preview), one page at a time; a version with `failAtPage` fails there.
 *
 * Every call is recorded in `calls`. `failNext` answers the next call of a method with an error.
 * Uploads are held (`uploads`) until the test moves them (`progress`, `finish`, `fail`) unless
 * `autoUpload` is on, which is how the preview shows a progress bar moving.
 */

type Method = keyof DocumentApi;

export interface FakeModel {
  id: string;
  name: string;
}

export interface FakeCall {
  method: Method;
  args: unknown[];
}

/** One upload in flight: the test reports its progress and decides how it ends. */
export interface FakeUpload {
  request: UploadRequest;
  file: File;
  fraction: number;
  settled: boolean;
  /** Reports the share sent. */
  progress(fraction: number): void;
  /** Ends it as stored: the document is made and queued. */
  finish(): void;
  /** Ends it with `error`. */
  fail(error: unknown): void;
}

export function refusal(
  status: number,
  code: ErrorBody["code"],
  messageKey: string,
  details?: Record<string, string | number>,
) {
  return new ApiError(status, { code, message_key: messageKey, ...(details ? { details } : {}) });
}

const DAY = 86_400_000;

export const FAKE_ME = "00000000-0000-4000-8000-0000000000a1";
export const FAKE_OTHER = "00000000-0000-4000-8000-0000000000b2";

/** The page image size of every fake page: A4 at about 96 dpi. */
export const PAGE_WIDTH = 794;
export const PAGE_HEIGHT = 1123;

/** A page drawn as an SVG, the way a scanned manual page looks like lines and a table. */
function pageSvg(name: string, page: number): string {
  const escaped = name.replace(/[&<>"]/g, (c) => `&#${c.charCodeAt(0)};`);
  const lines = Array.from(
    { length: 10 },
    (_, i) => `<rect x="60" y="${420 + i * 40}" width="${i % 3 === 0 ? 560 : 674}" height="10" fill="#d4d4d8"/>`,
  ).join("");
  const rows = Array.from(
    { length: 4 },
    (_, i) => `<rect x="60" y="${190 + i * 40}" width="674" height="30" fill="#e4e4e7"/>`,
  ).join("");
  return `<svg xmlns="http://www.w3.org/2000/svg" width="${PAGE_WIDTH}" height="${PAGE_HEIGHT}" viewBox="0 0 ${PAGE_WIDTH} ${PAGE_HEIGHT}"><rect width="${PAGE_WIDTH}" height="${PAGE_HEIGHT}" fill="#ffffff"/><rect x="60" y="60" width="420" height="28" fill="#a1a1aa"/><text x="60" y="130" font-size="22" fill="#52525b">${escaped} - ${page}</text>${rows}${lines}</svg>`;
}

const blobUrls = new Map<string, string>();

function blobUrl(key: string, make: () => Blob): string {
  const known = blobUrls.get(key);
  if (known !== undefined) return known;
  // Without `URL.createObjectURL` (a test DOM) the address is a path that is never fetched.
  const url =
    typeof URL.createObjectURL === "function"
      ? URL.createObjectURL(make())
      : `/api/knowledge/v1/fake/${encodeURIComponent(key)}`;
  blobUrls.set(key, url);
  return url;
}

export class FakeDocumentServer implements DocumentApi {
  readonly documents = new Map<string, DocumentDetail>();
  readonly calls: FakeCall[] = [];
  readonly uploads: FakeUpload[] = [];
  /** `"starter"` has no document RAG. */
  plan: "starter" | "business" = "business";
  limitBytes = 5_000_000_000;
  /** The server's clock. */
  now: () => number = () => Date.now();
  /** The user the uploads are from. */
  me = FAKE_ME;
  /** Uploads move by themselves, over about two seconds (the preview). */
  autoUpload = false;
  /** Queued and processing versions move on every 1.5 s (the preview). */
  autoProcess = false;
  /** The pages every version has. */
  pagesPerDocument = 12;
  /** Versions that fail while processing, by document name: the page they stop at. */
  failAtPage = new Map<string, number>();
  /** Errors the next call of a method throws (`failNext`). */
  private readonly failures = new Map<Method, unknown[]>();
  private timer: ReturnType<typeof setInterval> | undefined;

  constructor(readonly models: readonly FakeModel[] = []) {}

  /** A document with these versions (the first is version 1); returns it. */
  addDocument(options: {
    name: string;
    modelId: string;
    versions?: Partial<Omit<DocumentVersionInfo, "version">>[];
  }): DocumentDetail {
    const created = this.now() - 3 * DAY;
    const versions: DocumentVersionInfo[] = (options.versions ?? [{}]).map((partial, index) => ({
      version: index + 1,
      status: "ready" as DocumentStatus,
      page_count: this.pagesPerDocument,
      processed_pages: partial.page_count ?? this.pagesPerDocument,
      size_bytes: 20_000_000,
      uploaded_by: this.me,
      uploaded_at: new Date(created + index * DAY).toISOString(),
      failure: null,
      ...partial,
    }));
    const document: DocumentDetail = {
      id: fakeUuid(),
      name: options.name,
      model_id: options.modelId,
      deleted_at: null,
      versions: versions.reverse(),
    };
    this.documents.set(document.id, document);
    return document;
  }

  /** The next call of `method` throws `error`. */
  failNext(method: Method, error: unknown): void {
    this.failures.set(method, [...(this.failures.get(method) ?? []), error]);
  }

  /** Moves every queued or processing version one step: queued starts, processing does one page. */
  advance(): void {
    for (const document of this.documents.values()) {
      if (document.deleted_at !== null) continue;
      for (const version of document.versions) {
        const failAt = this.failAtPage.get(document.name);
        if (version.status === "queued") {
          version.status = "processing";
          version.page_count = version.page_count ?? this.pagesPerDocument;
        } else if (version.status === "processing") {
          const total = version.page_count ?? this.pagesPerDocument;
          const next = version.processed_pages + 1;
          if (failAt !== undefined && next >= failAt) {
            version.status = "failed";
            version.failure = { page: failAt, message_key: "knowledge.page_render_failed" } satisfies DocumentFailure;
          } else {
            version.processed_pages = next;
            if (next >= total) version.status = "ready";
          }
        }
      }
    }
  }

  /** Starts `autoProcess`'s clock; the preview calls it once, and `stop` ends it. */
  startProcessing(): void {
    if (this.timer !== undefined) return;
    this.timer = setInterval(() => {
      if (this.autoProcess) this.advance();
    }, 1500);
  }

  stop(): void {
    if (this.timer !== undefined) clearInterval(this.timer);
    this.timer = undefined;
  }

  private enter(method: Method, args: unknown[]): void {
    this.calls.push({ method, args });
    const failure = this.failures.get(method)?.shift();
    if (failure !== undefined) throw failure;
    if (this.plan === "starter") {
      throw refusal(403, "quota_exceeded", "entitlement.feature_not_in_plan", { entitlement: "document_rag" });
    }
  }

  private used(): number {
    let total = 0;
    for (const document of this.documents.values()) {
      if (document.deleted_at !== null) continue;
      for (const version of document.versions) total += version.size_bytes;
    }
    return total;
  }

  private summary(document: DocumentDetail): DocumentSummary {
    const [latest] = document.versions;
    if (latest === undefined) throw new Error("a deleted document has no summary");
    return { id: document.id, name: document.name, model_id: document.model_id, latest_version: { ...latest } };
  }

  async listDocuments(): Promise<DocumentList> {
    this.enter("listDocuments", []);
    const documents = [...this.documents.values()]
      .filter((document) => document.deleted_at === null)
      .map((document) => this.summary(document))
      .sort((a, b) => b.latest_version.uploaded_at.localeCompare(a.latest_version.uploaded_at));
    return { documents, usage: { used_bytes: this.used(), limit_bytes: this.limitBytes } };
  }

  async getDocument(documentId: string): Promise<DocumentDetail> {
    this.enter("getDocument", [documentId]);
    const document = this.documents.get(documentId);
    if (document === undefined) throw refusal(404, "not_found", "error.not_found");
    return structuredClone(document);
  }

  async getPage(request: { documentId: string; version: number; page: number }): Promise<DocumentPage> {
    this.enter("getPage", [request]);
    const document = this.documents.get(request.documentId);
    const version = document?.versions.find((item) => item.version === request.version);
    if (document === undefined || document.deleted_at !== null || version === undefined) {
      throw refusal(404, "not_found", "error.not_found");
    }
    if (version.page_count === null || request.page < 1 || request.page > version.page_count) {
      throw refusal(404, "not_found", "error.not_found");
    }
    return {
      page: request.page,
      width: PAGE_WIDTH,
      height: PAGE_HEIGHT,
      image_url: blobUrl(
        `${document.id}-${request.version}-${request.page}.svg`,
        () => new Blob([pageSvg(document.name, request.page)], { type: "image/svg+xml" }),
      ),
    };
  }

  upload(request: UploadRequest, file: File, options: UploadOptions): Promise<DocumentSummary> {
    return new Promise<DocumentSummary>((resolve, reject) => {
      try {
        this.enter("upload", [request, file.name]);
        if (this.used() + file.size > this.limitBytes) {
          throw refusal(403, "quota_exceeded", "entitlement.limit_reached", {
            entitlement: "document_storage",
            max: this.limitBytes,
          });
        }
      } catch (error) {
        reject(error);
        return;
      }
      const upload: FakeUpload = {
        request,
        file,
        fraction: 0,
        settled: false,
        progress: (fraction) => {
          if (upload.settled) return;
          upload.fraction = fraction;
          options.onProgress(fraction);
        },
        finish: () => {
          if (upload.settled) return;
          upload.settled = true;
          options.signal.removeEventListener("abort", onAbort);
          options.onProgress(1);
          resolve(this.store(request, file));
        },
        fail: (error) => {
          if (upload.settled) return;
          upload.settled = true;
          options.signal.removeEventListener("abort", onAbort);
          reject(error);
        },
      };
      const onAbort = () => {
        if (upload.settled) return;
        upload.settled = true;
        reject(new DOMException("The upload was cancelled.", "AbortError"));
      };
      if (options.signal.aborted) {
        onAbort();
        return;
      }
      options.signal.addEventListener("abort", onAbort);
      this.uploads.push(upload);
      if (this.autoUpload) {
        let step = 0;
        const timer = setInterval(() => {
          step += 1;
          if (upload.settled) {
            clearInterval(timer);
            return;
          }
          if (step >= 8) {
            clearInterval(timer);
            upload.finish();
          } else {
            upload.progress(step / 8);
          }
        }, 250);
      }
    });
  }

  /** Makes the document or version an upload stands for, queued. */
  private store(request: UploadRequest, file: File): DocumentSummary {
    const base: Omit<DocumentVersionInfo, "version"> = {
      status: "queued",
      page_count: null,
      processed_pages: 0,
      size_bytes: file.size,
      uploaded_by: this.me,
      uploaded_at: new Date(this.now()).toISOString(),
      failure: null,
    };
    const existing = request.documentId === undefined ? undefined : this.documents.get(request.documentId);
    if (existing !== undefined && existing.deleted_at === null) {
      existing.versions.unshift({ ...base, version: (existing.versions[0]?.version ?? 0) + 1 });
      return this.summary(existing);
    }
    const document: DocumentDetail = {
      id: fakeUuid(),
      name: file.name.replace(/\.pdf$/i, ""),
      model_id: request.modelId,
      deleted_at: null,
      versions: [{ ...base, version: 1 }],
    };
    this.documents.set(document.id, document);
    return this.summary(document);
  }

  async retryProcessing(request: { documentId: string; version: number }): Promise<void> {
    this.enter("retryProcessing", [request]);
    const version = this.documents.get(request.documentId)?.versions.find((item) => item.version === request.version);
    if (version === undefined) throw refusal(404, "not_found", "error.not_found");
    if (version.status !== "failed") throw refusal(409, "conflict", "error.conflict");
    version.status = "queued";
    version.failure = null;
  }

  async deleteDocument(documentId: string): Promise<void> {
    this.enter("deleteDocument", [documentId]);
    const document = this.documents.get(documentId);
    if (document === undefined) throw refusal(404, "not_found", "error.not_found");
    document.deleted_at = new Date(this.now()).toISOString();
    document.versions = [];
  }

  async getDownloadUrl(request: { documentId: string; version: number }): Promise<string> {
    this.enter("getDownloadUrl", [request]);
    if (!this.documents.get(request.documentId)?.versions.some((item) => item.version === request.version)) {
      throw refusal(404, "not_found", "error.not_found");
    }
    return blobUrl(
      `${request.documentId}-${request.version}.pdf`,
      () => new Blob(["%PDF-1.4\n%%EOF\n"], { type: "application/pdf" }),
    );
  }
}

/**
 * The demo library of the UI preview and of tests that want a normal one: the four documents of
 * docs/design/ms4-ms5/documents.html, over `models` (the model names are the design's).
 */
export function seedDocuments(server: FakeDocumentServer): void {
  const [inverter, meter, compressor] = server.models;
  if (inverter === undefined || meter === undefined || compressor === undefined) {
    throw new Error("seedDocuments needs three models");
  }
  const ago = (ms: number) => new Date(server.now() - ms).toISOString();
  server.addDocument({
    name: "XG-200 使用手冊",
    modelId: inverter.id,
    versions: [
      { page_count: 118, processed_pages: 118, size_bytes: 61_000_000, uploaded_at: ago(40 * DAY) },
      { page_count: 120, processed_pages: 120, size_bytes: 64_000_000, uploaded_at: ago(5 * DAY) },
    ],
  });
  server.addDocument({
    name: "XG-200 故障碼表",
    modelId: inverter.id,
    versions: [
      {
        status: "processing",
        page_count: 8,
        processed_pages: 5,
        size_bytes: 3_000_000,
        uploaded_by: FAKE_OTHER,
        uploaded_at: ago(5 * 60_000),
      },
    ],
  });
  server.addDocument({
    name: "PM-3200 電表手冊",
    modelId: meter.id,
    versions: [
      {
        status: "failed",
        page_count: 64,
        processed_pages: 63,
        size_bytes: 58_000_000,
        uploaded_at: ago(14 * DAY),
        failure: { page: 34, message_key: "knowledge.page_render_failed" },
      },
    ],
  });
  server.addDocument({
    name: "AC-75 空壓機保養手冊",
    modelId: compressor.id,
    versions: [
      {
        status: "queued",
        page_count: null,
        processed_pages: 0,
        size_bytes: 12_000_000,
        uploaded_by: FAKE_OTHER,
        uploaded_at: ago(20_000),
      },
    ],
  });
}

/** The model names of the design's demo library. */
export const DEMO_MODELS = ["XG-200 變頻器", "PM-3200 電表", "AC-75 空壓機"] as const;

/** A fake server over the demo models, with a device server that lists the same models. */
export function demoLibrary(seed: "normal" | "empty"): { documents: FakeDocumentServer; devices: FakeDeviceServer } {
  const devices = new FakeDeviceServer();
  const models = DEMO_MODELS.map((name) => {
    const model = devices.addModel(name);
    return { id: model.model_id, name };
  });
  const documents = new FakeDocumentServer(models);
  documents.autoUpload = true;
  documents.autoProcess = true;
  if (seed === "normal") seedDocuments(documents);
  return { documents, devices };
}

// The UI preview (Laysi/trustiot-v2#1065) loads every `fake-*.ts` and reads `preview`: the
// documents API is a provider, not HTTP, so the preview wraps the app in it. The page reads the
// model names through the real device API, so the same provider gives it a device server that
// lists the demo models. Nothing leaves the browser.
export const preview = {
  name: "Documents (knowledge service pending)",
  provider: (seed: "normal" | "empty"): ComponentType<{ children: ReactNode }> => {
    const { documents, devices } = demoLibrary(seed);
    const api = devices.api();
    return function DocumentsPreviewProvider({ children }: { children: ReactNode }) {
      // The clock that moves processing along runs only while the preview shows the page.
      useEffect(() => {
        documents.startProcessing();
        return () => documents.stop();
      }, []);
      return createElement(
        DeviceApiProvider,
        { api },
        createElement(DocumentApiProvider, { api: documents }, children),
      );
    };
  },
};
