import {
  FileCheck2,
  FileDown,
  FileText,
  FolderOpen,
  PanelRightClose,
  PanelRightOpen,
  SidebarClose,
  SidebarOpen,
} from "lucide-react";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { useViewerStore } from "@/stores/viewer-store";

export function DocumentHeader() {
  const document = useViewerStore((state) => state.document);
  const sidebarOpen = useViewerStore((state) => state.sidebarOpen);
  const inspectorOpen = useViewerStore((state) => state.inspectorOpen);
  const exporting = useViewerStore((state) => state.exporting);
  const batchExporting = useViewerStore((state) => state.batchExporting);
  const openSelectedDocument = useViewerStore(
    (state) => state.openSelectedDocument,
  );
  const exportCurrentPdf = useViewerStore((state) => state.exportCurrentPdf);
  const toggleSidebar = useViewerStore((state) => state.toggleSidebar);
  const toggleInspector = useViewerStore((state) => state.toggleInspector);

  return (
    <header className="window-drag-region grid h-16 shrink-0 grid-cols-[1fr_auto_1fr] items-center border-b border-border/80 bg-background/92 px-4 backdrop-blur-xl">
      <div className="flex min-w-0 items-center gap-2">
        <Button
          variant="ghost"
          size="icon"
          onClick={toggleSidebar}
          aria-label={sidebarOpen ? "收起导航栏" : "展开导航栏"}
          title={sidebarOpen ? "收起导航栏" : "展开导航栏"}
        >
          {sidebarOpen ? <SidebarClose /> : <SidebarOpen />}
        </Button>
        <div className="mx-1 h-7 w-px bg-border" />
        <Button
          variant="outline"
          size="sm"
          onClick={() => void openSelectedDocument()}
          disabled={exporting || batchExporting}
          className="bg-background/60"
        >
          <FolderOpen />
          打开
        </Button>
        <Button
          size="sm"
          onClick={() => void exportCurrentPdf()}
          disabled={!document || exporting || batchExporting}
        >
          <FileDown />
          {exporting || batchExporting ? "导出中" : "导出 PDF"}
        </Button>
      </div>

      <div className="flex min-w-0 items-center gap-3 px-8">
        <div className="grid size-9 shrink-0 place-items-center rounded-xl bg-primary text-primary-foreground shadow-sm shadow-primary/20">
          <FileText className="size-4.5" strokeWidth={2.2} />
        </div>
        <div className="min-w-0 text-center">
          <div className="flex items-center justify-center gap-2">
            <h1 className="max-w-[420px] truncate text-sm font-semibold tracking-[-0.01em]">
              {document?.fileName ?? "OFD Manager"}
            </h1>
            {document && (
              <Badge variant="outline" className="hidden sm:inline-flex">
                OFD
              </Badge>
            )}
          </div>
          <p className="mt-0.5 truncate text-[11px] text-muted-foreground">
            {document
              ? `${document.pageCount} 页 · ${document.author ?? document.creator ?? "未知创建者"}`
              : "安全、准确地阅读版式文档"}
          </p>
        </div>
      </div>

      <div className="flex items-center justify-end gap-1">
        {document && document.signatureCount > 0 && (
          <Badge variant="success" className="mr-2 hidden lg:inline-flex">
            <FileCheck2 className="size-3" />
            {document.signatureCount} 个签名
          </Badge>
        )}
        <Button
          variant="ghost"
          size="icon"
          onClick={toggleInspector}
          aria-label={inspectorOpen ? "收起检查器" : "展开检查器"}
          title={inspectorOpen ? "收起检查器" : "展开检查器"}
        >
          {inspectorOpen ? <PanelRightClose /> : <PanelRightOpen />}
        </Button>
      </div>
    </header>
  );
}
