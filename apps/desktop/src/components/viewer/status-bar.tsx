import { AlertTriangle, CircleCheck, FileType2 } from "lucide-react";

import { formatPageSize } from "@/lib/utils";
import { useViewerStore } from "@/stores/viewer-store";

export function StatusBar() {
  const document = useViewerStore((state) => state.document);
  const pageIndex = useViewerStore((state) => state.pageIndex);
  const zoom = useViewerStore((state) => state.zoom);
  const fitMode = useViewerStore((state) => state.fitMode);

  if (!document) {
    return null;
  }

  const page = document.pages[pageIndex];

  return (
    <footer className="flex h-7 shrink-0 items-center justify-between border-t border-border/70 bg-background px-3 text-[10px] text-muted-foreground">
      <div className="flex items-center gap-4">
        <span className="flex items-center gap-1.5">
          <FileType2 className="size-3" />
          OFD {document.version ?? "1.0"}
        </span>
        {page && (
          <span>{formatPageSize(page.widthMm, page.heightMm)}</span>
        )}
      </div>
      <div className="flex items-center gap-4 tabular-nums">
        {document.warningCount > 0 ? (
          <span className="flex items-center gap-1.5 text-amber-700">
            <AlertTriangle className="size-3" />
            {document.warningCount} 条提示
          </span>
        ) : (
          <span className="flex items-center gap-1.5">
            <CircleCheck className="size-3 text-emerald-600" />
            文档解析完成
          </span>
        )}
        <span>
          {pageIndex + 1} / {document.pageCount}
        </span>
        <span>
          {fitMode === "page"
            ? "适应页面"
            : fitMode === "width"
              ? "适应页宽"
              : `${Math.round(zoom * 100)}%`}
        </span>
      </div>
    </footer>
  );
}
