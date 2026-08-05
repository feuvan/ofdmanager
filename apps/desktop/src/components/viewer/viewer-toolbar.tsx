import { useEffect, useState } from "react";
import {
  ChevronsLeft,
  ChevronsRight,
  Minus,
  MoveHorizontal,
  Plus,
  Scan,
} from "lucide-react";

import { Button } from "@/components/ui/button";
import { Separator } from "@/components/ui/separator";
import { cn } from "@/lib/utils";
import { useViewerStore } from "@/stores/viewer-store";

export function ViewerToolbar() {
  const document = useViewerStore((state) => state.document);
  const pageIndex = useViewerStore((state) => state.pageIndex);
  const zoom = useViewerStore((state) => state.zoom);
  const fitMode = useViewerStore((state) => state.fitMode);
  const previousPage = useViewerStore((state) => state.previousPage);
  const nextPage = useViewerStore((state) => state.nextPage);
  const goToPage = useViewerStore((state) => state.goToPage);
  const zoomIn = useViewerStore((state) => state.zoomIn);
  const zoomOut = useViewerStore((state) => state.zoomOut);
  const setFitMode = useViewerStore((state) => state.setFitMode);
  const [pageInput, setPageInput] = useState("1");

  useEffect(() => {
    setPageInput(String(pageIndex + 1));
  }, [pageIndex]);

  if (!document) {
    return null;
  }

  const commitPage = () => {
    const requested = Number.parseInt(pageInput, 10);
    if (Number.isFinite(requested)) {
      goToPage(requested - 1);
    }
    setPageInput(String(useViewerStore.getState().pageIndex + 1));
  };

  return (
    <div className="pointer-events-none absolute inset-x-0 top-4 z-20 flex justify-center px-4">
      <div className="pointer-events-auto flex h-11 items-center gap-1 rounded-xl border border-border/80 bg-background/94 p-1 shadow-[0_8px_30px_rgba(35,38,45,0.12)] backdrop-blur-xl">
        <Button
          variant="ghost"
          size="icon-sm"
          onClick={previousPage}
          disabled={pageIndex === 0}
          aria-label="上一页"
          title="上一页"
        >
          <ChevronsLeft />
        </Button>

        <div className="flex items-center gap-1 px-1 text-xs tabular-nums">
          <input
            value={pageInput}
            onChange={(event) =>
              setPageInput(event.target.value.replace(/\D/g, ""))
            }
            onBlur={commitPage}
            onKeyDown={(event) => {
              if (event.key === "Enter") {
                event.currentTarget.blur();
              }
            }}
            aria-label="页码"
            className="h-7 w-9 rounded-md border border-border bg-muted/45 text-center font-medium outline-none transition focus:border-primary/40 focus:ring-2 focus:ring-primary/15"
          />
          <span className="text-muted-foreground">/ {document.pageCount}</span>
        </div>

        <Button
          variant="ghost"
          size="icon-sm"
          onClick={nextPage}
          disabled={pageIndex >= document.pageCount - 1}
          aria-label="下一页"
          title="下一页"
        >
          <ChevronsRight />
        </Button>

        <Separator orientation="vertical" className="mx-1 h-5" />

        <Button
          variant="ghost"
          size="icon-sm"
          onClick={zoomOut}
          aria-label="缩小"
          title="缩小"
        >
          <Minus />
        </Button>
        <span className="w-14 text-center text-xs font-medium tabular-nums">
          {fitMode === "page"
            ? "整页"
            : fitMode === "width"
              ? "页宽"
              : `${Math.round(zoom * 100)}%`}
        </span>
        <Button
          variant="ghost"
          size="icon-sm"
          onClick={zoomIn}
          aria-label="放大"
          title="放大"
        >
          <Plus />
        </Button>

        <Separator orientation="vertical" className="mx-1 h-5" />

        <Button
          variant="ghost"
          size="icon-sm"
          onClick={() => setFitMode("page")}
          aria-label="适应页面"
          title="适应页面"
          aria-pressed={fitMode === "page"}
          className={cn(fitMode === "page" && "bg-accent text-foreground")}
        >
          <Scan />
        </Button>
        <Button
          variant="ghost"
          size="icon-sm"
          onClick={() => setFitMode("width")}
          aria-label="适应页宽"
          title="适应页宽"
          aria-pressed={fitMode === "width"}
          className={cn(fitMode === "width" && "bg-accent text-foreground")}
        >
          <MoveHorizontal />
        </Button>
      </div>
    </div>
  );
}
