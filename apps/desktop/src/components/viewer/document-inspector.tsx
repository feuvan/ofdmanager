import {
  BadgeCheck,
  CircleAlert,
  FileWarning,
  Fingerprint,
  LoaderCircle,
  ShieldCheck,
  ShieldQuestion,
  UserRound,
} from "lucide-react";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Separator } from "@/components/ui/separator";
import {
  formatFileSize,
  formatPageSize,
} from "@/lib/utils";
import { useViewerStore } from "@/stores/viewer-store";

function Property({
  label,
  value,
  mono = false,
}: {
  label: string;
  value: string;
  mono?: boolean;
}) {
  return (
    <div className="grid grid-cols-[74px_minmax(0,1fr)] gap-3 py-1.5 text-xs">
      <dt className="text-muted-foreground">{label}</dt>
      <dd
        className={mono ? "truncate font-mono text-[11px]" : "truncate"}
        title={value}
      >
        {value}
      </dd>
    </div>
  );
}

type BadgeVariant = "success" | "destructive";

function VerificationSummary() {
  const verification = useViewerStore((state) => state.verification);
  const verifying = useViewerStore((state) => state.verifying);
  const verify = useViewerStore((state) => state.verify);
  const signatureCount =
    useViewerStore((state) => state.document?.signatureCount) ?? 0;

  if (signatureCount === 0) {
    return (
      <div className="rounded-xl border border-border/70 bg-muted/35 p-3.5">
        <div className="flex items-center gap-2 text-xs font-medium">
          <ShieldQuestion className="size-4 text-muted-foreground" />
          未发现数字签名
        </div>
        <p className="mt-2 text-[11px] leading-5 text-muted-foreground">
          文档中没有可供完整性检查的签名信息。
        </p>
      </div>
    );
  }

  if (!verification) {
    return (
      <div className="rounded-xl border border-primary/15 bg-primary/[0.045] p-3.5">
        <div className="flex items-center gap-2 text-xs font-medium">
          <ShieldCheck className="size-4 text-primary" />
          检测到 {signatureCount} 个签名
        </div>
        <p className="mt-2 text-[11px] leading-5 text-muted-foreground">
          可检查签名覆盖文件的摘要是否与文档内容一致。
        </p>
        <Button
          variant="outline"
          size="sm"
          onClick={verify}
          disabled={verifying}
          className="mt-3 w-full bg-background"
        >
          {verifying ? (
            <LoaderCircle className="animate-spin" />
          ) : (
            <Fingerprint />
          )}
          {verifying ? "正在检查" : "检查完整性"}
        </Button>
      </div>
    );
  }

  return (
    <div className="space-y-2">
      {verification.map((report) => {
        const variant: BadgeVariant = report.integrityOk
          ? "success"
          : "destructive";
        return (
          <div
            key={report.id}
            className="rounded-xl border border-border/70 bg-muted/25 p-3"
          >
            <div className="flex items-start justify-between gap-2">
              <div className="min-w-0">
                <p className="truncate text-xs font-medium">
                  {report.provider ?? `签名 ${report.id}`}
                </p>
                <p className="mt-1 text-[10px] text-muted-foreground">
                  {report.references.length} 个受保护文件
                </p>
              </div>
              <Badge variant={variant}>
                {report.integrityOk ? "完整" : "异常"}
              </Badge>
            </div>
          </div>
        );
      })}
      <Button
        variant="ghost"
        size="sm"
        onClick={verify}
        disabled={verifying}
        className="w-full"
      >
        {verifying && <LoaderCircle className="animate-spin" />}
        重新检查
      </Button>
    </div>
  );
}

export function DocumentInspector() {
  const document = useViewerStore((state) => state.document);
  const pageIndex = useViewerStore((state) => state.pageIndex);

  if (!document) {
    return null;
  }

  const page = document.pages[pageIndex];

  return (
    <aside className="flex min-h-0 w-[276px] shrink-0 flex-col border-l border-border/80 bg-sidebar">
      <div className="border-b border-border/70 px-4 py-[15px]">
        <h2 className="text-xs font-semibold">文档检查器</h2>
        <p className="mt-1 text-[10px] text-muted-foreground">
          元数据与完整性状态
        </p>
      </div>

      <div className="min-h-0 flex-1 overflow-y-auto">
        <section className="p-4">
          <div className="mb-3 flex items-center gap-2">
            <BadgeCheck className="size-3.5 text-primary" />
            <h3 className="text-[11px] font-semibold tracking-wide text-muted-foreground uppercase">
              完整性
            </h3>
          </div>
          <VerificationSummary />
        </section>

        <Separator />

        <section className="p-4">
          <div className="mb-2 flex items-center gap-2">
            <UserRound className="size-3.5 text-muted-foreground" />
            <h3 className="text-[11px] font-semibold tracking-wide text-muted-foreground uppercase">
              文档属性
            </h3>
          </div>
          <dl>
            <Property label="标题" value={document.title ?? "未命名"} />
            <Property label="作者" value={document.author ?? "未知"} />
            <Property label="创建工具" value={document.creator ?? "未知"} />
            <Property label="创建日期" value={document.creationDate ?? "未知"} />
            <Property
              label="文档 ID"
              value={document.docId ?? "未提供"}
              mono
            />
          </dl>
        </section>

        <Separator />

        <section className="p-4">
          <div className="mb-2 flex items-center gap-2">
            <CircleAlert className="size-3.5 text-muted-foreground" />
            <h3 className="text-[11px] font-semibold tracking-wide text-muted-foreground uppercase">
              文件信息
            </h3>
          </div>
          <dl>
            <Property label="格式" value={`OFD ${document.version ?? ""}`} />
            <Property label="文件大小" value={formatFileSize(document.fileSize)} />
            <Property label="页数" value={`${document.pageCount} 页`} />
            {page && (
              <Property
                label="当前页面"
                value={formatPageSize(page.widthMm, page.heightMm)}
              />
            )}
            <Property label="路径" value={document.filePath} />
          </dl>
        </section>

        {document.warnings.length > 0 && (
          <>
            <Separator />
            <section className="p-4">
              <div className="mb-3 flex items-center gap-2">
                <FileWarning className="size-3.5 text-amber-600" />
                <h3 className="text-[11px] font-semibold tracking-wide text-muted-foreground uppercase">
                  解析提示
                </h3>
                <Badge variant="warning" className="ml-auto">
                  {document.warnings.length}
                </Badge>
              </div>
              <div className="space-y-2">
                {document.warnings.slice(0, 5).map((warning) => (
                  <p
                    key={warning}
                    className="rounded-lg bg-amber-50/80 p-2.5 text-[10px] leading-4 text-amber-900"
                  >
                    {warning}
                  </p>
                ))}
              </div>
            </section>
          </>
        )}
      </div>
    </aside>
  );
}
