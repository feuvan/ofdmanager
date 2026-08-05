import { useEffect, useMemo, useRef, useState } from "react";
import { LoaderCircle, TriangleAlert } from "lucide-react";

import { renderPage } from "@/lib/tauri";
import { useViewerStore } from "@/stores/viewer-store";

const CSS_DPI = 96;
const MM_PER_INCH = 25.4;

interface ViewportSize {
  width: number;
  height: number;
}

function useElementSize() {
  const ref = useRef<HTMLDivElement>(null);
  const [size, setSize] = useState<ViewportSize>({ width: 0, height: 0 });

  useEffect(() => {
    const element = ref.current;
    if (!element) {
      return;
    }

    const observer = new ResizeObserver(([entry]) => {
      const { width, height } = entry.contentRect;
      setSize({ width, height });
    });
    observer.observe(element);
    return () => observer.disconnect();
  }, []);

  return { ref, size };
}

function getScale(
  mode: "page" | "width" | null,
  zoom: number,
  viewport: ViewportSize,
  pageWidth: number,
  pageHeight: number,
) {
  if (!mode || viewport.width === 0 || viewport.height === 0) {
    return zoom;
  }

  const widthScale = Math.max(0.1, (viewport.width - 96) / pageWidth);
  if (mode === "width") {
    return widthScale;
  }

  const heightScale = Math.max(0.1, (viewport.height - 112) / pageHeight);
  return Math.min(widthScale, heightScale);
}

export function DocumentViewport() {
  const document = useViewerStore((state) => state.document);
  const pageIndex = useViewerStore((state) => state.pageIndex);
  const zoom = useViewerStore((state) => state.zoom);
  const fitMode = useViewerStore((state) => state.fitMode);
  const setFitScale = useViewerStore((state) => state.setFitScale);
  const { ref, size } = useElementSize();
  const [source, setSource] = useState<string | null>(null);
  const [rendering, setRendering] = useState(true);
  const [renderError, setRenderError] = useState<string | null>(null);
  const sourceRef = useRef<string | null>(null);
  const sourcePageRef = useRef(pageIndex);
  const sourceDocumentRef = useRef<string | null>(null);

  const page = document?.pages[pageIndex];
  const pageWidth = ((page?.widthMm ?? 210) / MM_PER_INCH) * CSS_DPI;
  const pageHeight = ((page?.heightMm ?? 297) / MM_PER_INCH) * CSS_DPI;
  const scale = getScale(
    fitMode,
    zoom,
    size,
    pageWidth,
    pageHeight,
  );

  useEffect(() => {
    if (fitMode) {
      setFitScale(scale);
    }
  }, [fitMode, scale, setFitScale]);

  const renderDpi = useMemo(() => {
    const density = Math.min(window.devicePixelRatio || 1, 2);
    const target = Math.min(240, Math.max(72, CSS_DPI * scale * density));
    return Math.round(target / 12) * 12;
  }, [scale]);

  useEffect(() => {
    if (!document || !page) {
      return;
    }

    if (
      sourceDocumentRef.current !== document.filePath ||
      sourcePageRef.current !== pageIndex
    ) {
      sourceDocumentRef.current = document.filePath;
      sourcePageRef.current = pageIndex;
      if (sourceRef.current) {
        URL.revokeObjectURL(sourceRef.current);
        sourceRef.current = null;
      }
      setSource(null);
    }

    let cancelled = false;
    const timeout = window.setTimeout(() => {
      setRendering(true);
      setRenderError(null);

      void renderPage(pageIndex, renderDpi)
        .then((blob) => {
          if (cancelled) {
            return;
          }

          const nextSource = URL.createObjectURL(blob);
          if (sourceRef.current) {
            URL.revokeObjectURL(sourceRef.current);
          }
          sourceRef.current = nextSource;
          setSource(nextSource);
          setRendering(false);
        })
        .catch((error: unknown) => {
          if (!cancelled) {
            setRendering(false);
            setRenderError(error instanceof Error ? error.message : String(error));
          }
        });
    }, 100);

    return () => {
      cancelled = true;
      window.clearTimeout(timeout);
    };
  }, [document, page, pageIndex, renderDpi]);

  useEffect(
    () => () => {
      if (sourceRef.current) {
        URL.revokeObjectURL(sourceRef.current);
      }
    },
    [],
  );

  if (!document || !page) {
    return null;
  }

  return (
    <main
      ref={ref}
      className="viewer-grid relative min-h-0 min-w-0 flex-1 overflow-auto bg-canvas outline-none"
      tabIndex={0}
    >
      <div className="flex min-h-full min-w-full items-center justify-center px-12 py-14">
        <div
          className="relative shrink-0 overflow-hidden bg-white shadow-[0_2px_4px_rgba(20,24,32,0.06),0_18px_55px_rgba(20,24,32,0.16)] ring-1 ring-black/[0.08]"
          style={{
            width: `${pageWidth * scale}px`,
            height: `${pageHeight * scale}px`,
          }}
        >
          {source ? (
            <img
              src={source}
              alt={`第 ${pageIndex + 1} 页`}
              draggable={false}
              className="h-full w-full select-none object-fill"
            />
          ) : (
            <div className="h-full w-full bg-white" />
          )}

          {rendering && (
            <div className="absolute right-3 bottom-3 flex items-center gap-2 rounded-lg border border-border/70 bg-background/90 px-2.5 py-1.5 text-[11px] text-muted-foreground shadow-sm backdrop-blur">
              <LoaderCircle className="size-3.5 animate-spin" />
              正在渲染
            </div>
          )}

          {renderError && (
            <div className="absolute inset-0 grid place-items-center bg-white/96 p-8 text-center">
              <div>
                <TriangleAlert className="mx-auto mb-3 size-7 text-destructive" />
                <p className="text-sm font-medium">页面渲染失败</p>
                <p className="mt-2 max-w-sm text-xs leading-5 text-muted-foreground">
                  {renderError}
                </p>
              </div>
            </div>
          )}
        </div>
      </div>
    </main>
  );
}
