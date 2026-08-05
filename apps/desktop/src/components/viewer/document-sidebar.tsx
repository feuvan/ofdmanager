import { useEffect, useRef, useState } from "react";
import { ChevronRight, FileStack, ListTree } from "lucide-react";

import { Skeleton } from "@/components/ui/skeleton";
import { renderPage, type OutlineNode } from "@/lib/tauri";
import { cn } from "@/lib/utils";
import { useViewerStore } from "@/stores/viewer-store";

function PageThumbnail({
  documentKey,
  index,
  active,
  onSelect,
}: {
  documentKey: string;
  index: number;
  active: boolean;
  onSelect: () => void;
}) {
  const rootRef = useRef<HTMLButtonElement>(null);
  const [visible, setVisible] = useState(false);
  const [source, setSource] = useState<string | null>(null);
  const [renderError, setRenderError] = useState(false);
  const [retry, setRetry] = useState(0);

  useEffect(() => {
    const element = rootRef.current;
    if (!element) {
      return;
    }

    const observer = new IntersectionObserver(
      ([entry]) => {
        if (entry.isIntersecting) {
          setVisible(true);
          observer.disconnect();
        }
      },
      { rootMargin: "160px" },
    );

    observer.observe(element);
    return () => observer.disconnect();
  }, []);

  useEffect(() => {
    if (!visible) {
      return;
    }

    setSource(null);
    setRenderError(false);
    let cancelled = false;
    let objectUrl: string | null = null;

    void renderPage(index, 36)
      .then((blob) => {
        if (cancelled) {
          return;
        }
        objectUrl = URL.createObjectURL(blob);
        setSource(objectUrl);
      })
      .catch(() => {
        if (!cancelled) {
          setRenderError(true);
        }
      });

    return () => {
      cancelled = true;
      if (objectUrl) {
        URL.revokeObjectURL(objectUrl);
      }
      setSource(null);
    };
  }, [documentKey, index, retry, visible]);

  return (
    <button
      ref={rootRef}
      type="button"
      onClick={() => {
        if (renderError) {
          setRenderError(false);
          setRetry((value) => value + 1);
        }
        onSelect();
      }}
      aria-current={active ? "page" : undefined}
      className={cn(
        "group flex w-full flex-col items-center gap-2 rounded-xl border p-2.5 text-left transition-all outline-none focus-visible:ring-2 focus-visible:ring-primary/25",
        active
          ? "border-primary/30 bg-primary/[0.07] shadow-sm"
          : "border-transparent hover:border-border hover:bg-accent/60",
      )}
    >
      <div
        className={cn(
          "relative grid aspect-[0.707] w-[112px] place-items-center overflow-hidden rounded-[3px] border bg-white shadow-[0_5px_16px_rgba(40,45,55,0.09)] transition-transform group-hover:-translate-y-0.5",
          active ? "border-primary/30" : "border-black/10",
        )}
      >
        {source && !renderError ? (
          <img
            src={source}
            alt={`第 ${index + 1} 页缩略图`}
            className="h-full w-full object-contain"
          />
        ) : renderError ? (
          <span className="px-2 text-center text-[10px] leading-4 text-destructive">
            点击重试
          </span>
        ) : (
          <Skeleton className="h-full w-full rounded-none bg-stone-100" />
        )}
        {active && (
          <span className="absolute inset-y-0 left-0 w-0.5 bg-primary" />
        )}
      </div>
      <span
        className={cn(
          "text-xs tabular-nums",
          active ? "font-semibold text-primary" : "text-muted-foreground",
        )}
      >
        {index + 1}
      </span>
    </button>
  );
}

function OutlineTree({
  nodes,
  depth = 0,
}: {
  nodes: OutlineNode[];
  depth?: number;
}) {
  const goToPage = useViewerStore((state) => state.goToPage);

  return nodes.map((node, index) => (
    <div key={`${depth}-${index}-${node.title}`}>
      <button
        type="button"
        disabled={node.pageIndex === null}
        onClick={() => node.pageIndex !== null && goToPage(node.pageIndex)}
        className="flex min-h-9 w-full items-center gap-1.5 rounded-md pr-2 text-left text-xs text-muted-foreground transition-colors hover:bg-accent hover:text-foreground disabled:cursor-default disabled:opacity-70"
        style={{ paddingLeft: `${10 + depth * 14}px` }}
      >
        <ChevronRight className="size-3 shrink-0 opacity-50" />
        <span className="truncate">{node.title}</span>
      </button>
      {node.children.length > 0 && (
        <OutlineTree nodes={node.children} depth={depth + 1} />
      )}
    </div>
  ));
}

export function DocumentSidebar() {
  const document = useViewerStore((state) => state.document);
  const pageIndex = useViewerStore((state) => state.pageIndex);
  const sidebarView = useViewerStore((state) => state.sidebarView);
  const setSidebarView = useViewerStore((state) => state.setSidebarView);
  const goToPage = useViewerStore((state) => state.goToPage);

  if (!document) {
    return null;
  }

  return (
    <aside className="flex min-h-0 w-[190px] shrink-0 flex-col border-r border-border/80 bg-sidebar">
      <div className="m-3 grid grid-cols-2 rounded-lg bg-muted/70 p-1">
        <button
          type="button"
          onClick={() => setSidebarView("pages")}
          aria-pressed={sidebarView === "pages"}
          className={cn(
            "flex h-8 items-center justify-center gap-1.5 rounded-md text-xs font-medium transition-all",
            sidebarView === "pages"
              ? "bg-background text-foreground shadow-sm"
              : "text-muted-foreground hover:text-foreground",
          )}
        >
          <FileStack className="size-3.5" />
          页面
        </button>
        <button
          type="button"
          onClick={() => setSidebarView("outline")}
          aria-pressed={sidebarView === "outline"}
          className={cn(
            "flex h-8 items-center justify-center gap-1.5 rounded-md text-xs font-medium transition-all",
            sidebarView === "outline"
              ? "bg-background text-foreground shadow-sm"
              : "text-muted-foreground hover:text-foreground",
          )}
        >
          <ListTree className="size-3.5" />
          目录
        </button>
      </div>

      <div className="min-h-0 flex-1 overflow-y-auto px-3 pb-4">
        {sidebarView === "pages" ? (
          <div className="space-y-2">
            {document.pages.map((page) => (
              <PageThumbnail
                key={`${document.filePath}-${page.index}`}
                documentKey={document.filePath}
                index={page.index}
                active={page.index === pageIndex}
                onSelect={() => goToPage(page.index)}
              />
            ))}
          </div>
        ) : document.outline.length > 0 ? (
          <OutlineTree nodes={document.outline} />
        ) : (
          <div className="grid place-items-center px-3 py-16 text-center">
            <ListTree className="mb-3 size-6 text-muted-foreground/50" />
            <p className="text-xs font-medium">无文档目录</p>
            <p className="mt-1 text-[11px] leading-5 text-muted-foreground">
              此文档未包含可用的书签或大纲。
            </p>
          </div>
        )}
      </div>
    </aside>
  );
}
