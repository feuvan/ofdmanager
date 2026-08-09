import { revealItemInDir } from "@tauri-apps/plugin-opener";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { open, save } from "@tauri-apps/plugin-dialog";

export interface PageSummary {
  index: number;
  widthMm: number;
  heightMm: number;
}

export interface OutlineNode {
  title: string;
  pageIndex: number | null;
  children: OutlineNode[];
}

export interface DocumentSummary {
  filePath: string;
  fileName: string;
  fileSize: number;
  version: string | null;
  docType: string | null;
  pageCount: number;
  title: string | null;
  author: string | null;
  subject: string | null;
  creator: string | null;
  creationDate: string | null;
  docId: string | null;
  warningCount: number;
  signatureCount: number;
  sealCount: number;
  annotationCount: number;
  warnings: string[];
  pages: PageSummary[];
  outline: OutlineNode[];
}

export type DigestStatus =
  | "ok"
  | "mismatch"
  | "fileMissing"
  | "resourceLimit"
  | "readError"
  | "unsupportedMethod"
  | "badCheckValue";

export interface ReferenceReport {
  fileRef: string;
  method: string;
  status: DigestStatus;
}

export interface SignatureReport {
  id: string;
  signatureType: "seal" | "sign";
  provider: string | null;
  signatureMethod: string | null;
  signatureDateTime: string | null;
  integrityOk: boolean;
  references: ReferenceReport[];
}

export interface ExportProgress {
  current: number;
  total: number;
}

export interface BatchExportProgress {
  current: number;
  total: number;
  fileName: string;
}

export interface ExportResult {
  path: string;
  pageCount: number;
  fileSize: number;
}

export function isTauriRuntime() {
  return "__TAURI_INTERNALS__" in window;
}

export async function chooseDocument() {
  const path = await open({
    multiple: false,
    directory: false,
    filters: [{ name: "OFD 文档", extensions: ["ofd"] }],
  });

  return typeof path === "string" ? path : null;
}

export async function openDocument(path: string) {
  return invoke<DocumentSummary>("open_document", { path });
}

export async function openLaunchDocument() {
  return invoke<DocumentSummary | null>("open_launch_document");
}

export async function closeDocument() {
  return invoke<void>("close_document");
}

export async function renderPage(pageIndex: number, dpi: number) {
  const bytes = await invoke<ArrayBuffer | Uint8Array>("render_page", {
    pageIndex,
    dpi,
  });
  const buffer =
    bytes instanceof Uint8Array ? bytes.slice().buffer : bytes;
  return new Blob([buffer as ArrayBuffer], { type: "image/png" });
}

export async function verifyDocument() {
  return invoke<SignatureReport[]>("verify_document");
}

export async function choosePdfDestination(fileName: string) {
  const defaultName = fileName.replace(/\.ofd$/i, "") + ".pdf";
  return save({
    defaultPath: defaultName,
    filters: [{ name: "PDF 文档", extensions: ["pdf"] }],
  });
}

export async function exportDocumentPdf(path: string, dpi = 200) {
  return invoke<ExportResult>("export_pdf", { path, dpi });
}

export async function exportBatchDocumentPdf(paths: string[], dpi = 200) {
  return invoke<ExportResult[]>("export_batch_pdf", { paths, dpi });
}

export async function revealExportedFile(path: string) {
  return revealItemInDir(path);
}

export async function listenForExportProgress(
  onProgress: (progress: ExportProgress) => void,
) {
  return listen<ExportProgress>("export-progress", (event) => {
    onProgress(event.payload);
  });
}

export async function listenForBatchExportProgress(
  onProgress: (progress: BatchExportProgress) => void,
) {
  return listen<BatchExportProgress>("batch-export-progress", (event) => {
    onProgress(event.payload);
  });
}

export async function listenForOpenDocumentRequest(
  onRequest: (path: string) => void,
) {
  return listen<string>("open-document-request", (event) => {
    onRequest(event.payload);
  });
}

export async function listenForFileDrop(
  onDrop: (paths: string[]) => void,
) {
  return getCurrentWebview().onDragDropEvent((event) => {
    if (event.payload.type === "drop") {
      onDrop(event.payload.paths);
    }
  });
}
