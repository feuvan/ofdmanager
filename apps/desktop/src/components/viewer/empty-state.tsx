import {
  FileCheck2,
  FileText,
  FolderOpen,
  Layers3,
  ShieldCheck,
} from "lucide-react";

import { Button } from "@/components/ui/button";
import { useViewerStore } from "@/stores/viewer-store";

export function EmptyState() {
  const openSelectedDocument = useViewerStore(
    (state) => state.openSelectedDocument,
  );

  return (
    <main className="viewer-grid relative grid min-h-0 flex-1 place-items-center overflow-hidden bg-canvas p-8">
      <div className="absolute top-[18%] left-[12%] size-44 rounded-full bg-primary/[0.035] blur-2xl" />
      <div className="absolute right-[8%] bottom-[12%] size-56 rounded-full bg-amber-500/[0.04] blur-3xl" />

      <div className="relative flex max-w-xl flex-col items-center text-center">
        <div className="relative mb-8 h-36 w-48">
          <div className="absolute top-3 left-12 h-28 w-20 rotate-[-8deg] rounded border border-border bg-white shadow-lg" />
          <div className="absolute top-3 right-12 h-28 w-20 rotate-[8deg] rounded border border-border bg-white shadow-lg" />
          <div className="absolute inset-x-14 top-0 h-32 rounded-md border border-black/10 bg-white shadow-[0_18px_45px_rgba(31,35,43,0.14)]">
            <div className="mx-4 mt-5 h-2 rounded-full bg-primary/15" />
            <div className="mx-4 mt-2 h-1.5 w-8 rounded-full bg-muted" />
            <div className="mx-4 mt-5 space-y-2">
              <div className="h-1 rounded-full bg-muted" />
              <div className="h-1 rounded-full bg-muted" />
              <div className="h-1 w-2/3 rounded-full bg-muted" />
            </div>
            <div className="absolute right-3 bottom-3 grid size-8 place-items-center rounded-full border-2 border-primary/35 text-primary">
              <FileCheck2 className="size-3.5" />
            </div>
          </div>
          <div className="absolute -bottom-1 left-1/2 grid size-10 -translate-x-1/2 place-items-center rounded-xl bg-primary text-primary-foreground shadow-lg shadow-primary/20">
            <FileText className="size-5" />
          </div>
        </div>

        <h2 className="text-2xl font-semibold tracking-[-0.035em]">
          打开一份 OFD 文档
        </h2>
        <p className="mt-3 max-w-md text-sm leading-6 text-muted-foreground">
          准确呈现电子发票、证书与公文，并在本机完成文档解析和签名完整性检查。
        </p>

        <Button
          size="lg"
          onClick={() => void openSelectedDocument()}
          className="mt-7 min-w-36"
        >
          <FolderOpen />
          选择文件
        </Button>
        <p className="mt-3 text-[11px] text-muted-foreground">
          或将 .ofd 文件拖放到窗口中
        </p>

        <div className="mt-9 flex flex-wrap justify-center gap-x-5 gap-y-2 text-[11px] text-muted-foreground">
          <span className="flex items-center gap-1.5">
            <Layers3 className="size-3.5 text-primary/70" />
            高保真渲染
          </span>
          <span className="flex items-center gap-1.5">
            <ShieldCheck className="size-3.5 text-primary/70" />
            本地安全处理
          </span>
          <span className="flex items-center gap-1.5">
            <FileCheck2 className="size-3.5 text-primary/70" />
            签名完整性检查
          </span>
        </div>
      </div>
    </main>
  );
}
