import { useEffect, useRef } from "react";
import {
  CheckCircle2,
  CircleAlert,
  FileDown,
  LoaderCircle,
  X,
} from "lucide-react";

import { DocumentHeader } from "@/components/viewer/document-header";
import { DocumentInspector } from "@/components/viewer/document-inspector";
import { DocumentSidebar } from "@/components/viewer/document-sidebar";
import { DocumentViewport } from "@/components/viewer/document-viewport";
import { EmptyState } from "@/components/viewer/empty-state";
import { StatusBar } from "@/components/viewer/status-bar";
import { ViewerToolbar } from "@/components/viewer/viewer-toolbar";
import { Button } from "@/components/ui/button";
import {
  isTauriRuntime,
  listenForExportProgress,
  listenForFileDrop,
  listenForOpenDocumentRequest,
} from "@/lib/tauri";
import { formatFileSize } from "@/lib/utils";
import { useTauriListener } from "@/hooks/use-tauri-listener";
import { useViewerStore } from "@/stores/viewer-store";

export default function App() {
  const document = useViewerStore((state) => state.document);
  const sidebarOpen = useViewerStore((state) => state.sidebarOpen);
  const inspectorOpen = useViewerStore((state) => state.inspectorOpen);
  const loading = useViewerStore((state) => state.loading);
  const exporting = useViewerStore((state) => state.exporting);
  const exportProgress = useViewerStore((state) => state.exportProgress);
  const exportResult = useViewerStore((state) => state.exportResult);
  const error = useViewerStore((state) => state.error);
  const clearExportResult = useViewerStore(
    (state) => state.clearExportResult,
  );
  const clearError = useViewerStore((state) => state.clearError);
  const setError = useViewerStore((state) => state.setError);
  const initialized = useRef(false);
  const exportDialogRef = useRef<HTMLDivElement>(null);
  const tauriRuntime = isTauriRuntime();

  useEffect(() => {
    if (!isTauriRuntime() || initialized.current) {
      return;
    }
    initialized.current = true;
    void useViewerStore.getState().initialize();
  }, []);

  useTauriListener(
    listenForOpenDocumentRequest,
    (path) => void useViewerStore.getState().openPath(path),
    setError,
    tauriRuntime,
  );
  useTauriListener(
    listenForExportProgress,
    (progress) => useViewerStore.getState().setExportProgress(progress),
    setError,
    tauriRuntime,
  );
  useTauriListener(
    listenForFileDrop,
    (paths) => {
      const path = paths.find((candidate) =>
        candidate.toLocaleLowerCase().endsWith(".ofd"),
      );
      if (path) {
        void useViewerStore.getState().openPath(path);
      }
    },
    setError,
    tauriRuntime,
  );

  useEffect(() => {
    if (!exporting) {
      return;
    }
    const frame = requestAnimationFrame(() => exportDialogRef.current?.focus());
    return () => cancelAnimationFrame(frame);
  }, [exporting]);

  useEffect(() => {
    const handleKeyDown = (event: KeyboardEvent) => {
      const state = useViewerStore.getState();
      const command = event.metaKey || event.ctrlKey;

      if (state.exporting) {
        event.preventDefault();
        return;
      }

      if (command && event.key.toLocaleLowerCase() === "o") {
        event.preventDefault();
        void state.openSelectedDocument();
        return;
      }

      if (!state.document) {
        return;
      }

      if (command && (event.key === "=" || event.key === "+")) {
        event.preventDefault();
        state.zoomIn();
      } else if (command && event.key === "-") {
        event.preventDefault();
        state.zoomOut();
      } else if (command && event.key === "0") {
        event.preventDefault();
        state.setFitMode("page");
      } else if (
        event.key === "PageDown" ||
        (event.key === "ArrowRight" &&
          !(event.target instanceof HTMLInputElement))
      ) {
        event.preventDefault();
        state.nextPage();
      } else if (
        event.key === "PageUp" ||
        (event.key === "ArrowLeft" &&
          !(event.target instanceof HTMLInputElement))
      ) {
        event.preventDefault();
        state.previousPage();
      }
    };

    window.addEventListener("keydown", handleKeyDown);
    return () => window.removeEventListener("keydown", handleKeyDown);
  }, []);

  return (
    <div className="flex h-dvh min-h-[540px] w-full min-w-[760px] flex-col overflow-hidden bg-background text-foreground">
      <DocumentHeader />

      <div className="relative flex min-h-0 flex-1">
        {document && sidebarOpen && <DocumentSidebar />}

        <div className="relative flex min-h-0 min-w-0 flex-1">
          {document ? (
            <>
              <DocumentViewport />
              <ViewerToolbar />
            </>
          ) : (
            <EmptyState />
          )}
        </div>

        {document && inspectorOpen && <DocumentInspector />}

        {loading && (
          <div className="absolute inset-x-0 top-0 z-50 h-0.5 overflow-hidden bg-primary/10">
            <div className="loading-bar h-full w-1/3 bg-primary" />
          </div>
        )}

        {loading && !document && (
          <div className="absolute inset-0 z-40 grid place-items-center bg-background/65 backdrop-blur-sm">
            <div className="flex items-center gap-2 rounded-xl border bg-background px-4 py-3 text-sm shadow-xl">
              <LoaderCircle className="size-4 animate-spin text-primary" />
              正在解析文档
            </div>
          </div>
        )}

        {exporting && (
          <div
            ref={exportDialogRef}
            role="dialog"
            aria-modal="true"
            aria-labelledby="export-dialog-title"
            aria-describedby="export-dialog-description"
            tabIndex={-1}
            className="absolute inset-0 z-50 grid place-items-center bg-background/70 p-6 outline-none backdrop-blur-sm"
          >
            <div className="w-full max-w-sm rounded-2xl border bg-background p-5 shadow-2xl">
              <div className="flex items-start gap-3">
                <div className="grid size-10 shrink-0 place-items-center rounded-xl bg-primary/10 text-primary">
                  <FileDown className="size-5" />
                </div>
                <div className="min-w-0 flex-1">
                  <p id="export-dialog-title" className="text-sm font-semibold">
                    正在导出 PDF
                  </p>
                  <p
                    id="export-dialog-description"
                    className="mt-1 text-xs text-muted-foreground"
                  >
                    逐页渲染高质量图像，请勿关闭窗口。
                  </p>
                </div>
              </div>
              <div className="mt-5 h-2 overflow-hidden rounded-full bg-muted">
                <div
                  role="progressbar"
                  aria-label="PDF 导出进度"
                  aria-valuemin={0}
                  aria-valuemax={Math.max(1, exportProgress?.total ?? 0)}
                  aria-valuenow={exportProgress?.current ?? 0}
                  className="h-full rounded-full bg-primary transition-[width] duration-300"
                  style={{
                    width: `${
                      exportProgress && exportProgress.total > 0
                        ? (exportProgress.current / exportProgress.total) * 100
                        : 4
                    }%`,
                  }}
                />
              </div>
              <div className="mt-2 flex justify-between text-[11px] text-muted-foreground tabular-nums">
                <span>
                  {exportProgress?.current
                    ? `已处理第 ${exportProgress.current} 页`
                    : "正在准备文档"}
                </span>
                <span>
                  {exportProgress?.total
                    ? `${exportProgress.current} / ${exportProgress.total}`
                    : ""}
                </span>
              </div>
            </div>
          </div>
        )}

        {error && (
          <div
            role="alert"
            className="absolute bottom-5 left-1/2 z-50 flex max-w-xl -translate-x-1/2 items-start gap-3 rounded-xl border border-red-200 bg-background px-4 py-3 shadow-2xl"
          >
            <CircleAlert className="mt-0.5 size-4 shrink-0 text-destructive" />
            <div className="min-w-0">
              <p className="text-xs font-semibold">操作失败</p>
              <p className="mt-1 line-clamp-3 text-[11px] leading-5 text-muted-foreground">
                {error}
              </p>
            </div>
            <Button
              variant="ghost"
              size="icon-sm"
              onClick={clearError}
              aria-label="关闭错误提示"
              className="-mt-1 -mr-2"
            >
              <X />
            </Button>
          </div>
        )}

        {exportResult && (
          <div className="absolute right-5 bottom-5 z-50 flex w-[360px] items-start gap-3 rounded-xl border border-emerald-200 bg-background px-4 py-3.5 shadow-2xl">
            <CheckCircle2 className="mt-0.5 size-5 shrink-0 text-emerald-600" />
            <div className="min-w-0 flex-1">
              <p className="text-xs font-semibold">PDF 导出完成</p>
              <p className="mt-1 truncate text-[11px] text-muted-foreground">
                {exportResult.path}
              </p>
              <p className="mt-1 text-[10px] text-muted-foreground">
                {exportResult.pageCount} 页 ·{" "}
                {formatFileSize(exportResult.fileSize)}
              </p>
            </div>
            <Button
              variant="ghost"
              size="icon-sm"
              onClick={clearExportResult}
              aria-label="关闭导出提示"
              className="-mt-1 -mr-2"
            >
              <X />
            </Button>
          </div>
        )}
      </div>

      <StatusBar />
    </div>
  );
}
