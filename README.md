# OFD Manager

一个跨平台的 **OFD** 文件查看与处理工具。OFD 即开放版式文档
（GB/T 33190-2016），是中国用于增值税电子发票、电子票据、证书和政务
文件等场景的固定版式文档格式。

[English](README_en.md)

项目的核心思路是使用可移植的 Rust 实现可靠的 OFD **解析与渲染**，再将
核心能力用于桌面端、移动端和 Web。目前 Rust 核心、命令行工具和 Tauri
桌面查看器已经可以工作，并能够高保真渲染真实文档。

> 当前状态：**早期但可用**。渲染引擎已经覆盖真实发票、机票和证书等文档，
> 并通过参考渲染结果进行验证。

## 项目组成

本仓库包含以下三个主要部分：

- **Rust core 库**（`crates/ofd-core`）：负责解析 OFD 容器并将页面渲染为
  位图，保持与 UI 和文件系统解耦，以便跨平台复用。
- **CLI 实现**（`crates/ofd-cli`）：提供 `render` 和 `verify` 命令，用于
  渲染页面和校验签名文件摘要完整性。
- **桌面 app**（`apps/desktop`）：基于 Tauri v2、React 和 TypeScript 的
  跨平台桌面应用，支持缩略图、目录、元数据、签名校验和图像型 PDF 导出。

## macOS 首页截图

![OFD Manager macOS app 首页](docs/macos-home.png)

## 已实现功能

- **页面渲染**：支持文本（包括嵌入字体）、矢量路径、位图、图层、模板、
  裁剪和透明度。
- **字体处理**：优先使用文档内嵌字体，支持 TrueType、OpenType/CFF 以及
  bare-CFF；未嵌入字体时使用确定性的 CJK 后备字体，并支持 OFD 按字形的
  显式定位和竖排文本。
- **电子签章**（GB/T 38540）：支持渲染位图印章和矢量印章（嵌入式 OFD
  印章），并解析 SES 结构。
- **JBIG2 图像**：支持发票二维码和黑白扫描图像。
- **签名校验**：解析数字签名，并校验文件摘要完整性（SM3/SHA-256/MD5），
  检测受保护文件是否被篡改。
- **导出**：支持按任意 DPI 将页面渲染为 PNG，桌面端支持将 OFD 页面导出
  为图像型 PDF。
- **桌面查看器**：支持文件打开、缩略图、目录导航、缩放、元数据、签名
  校验、拖放和深色模式。
- **Quick Look（macOS）**：在 Finder 中选中 `.ofd` 文件按空格即可原生预览，
  并显示 Finder 缩略图；两个 Quick Look 扩展随桌面应用一起打包（扩展需要
  macOS 12+）。

当前已知缺口包括完整的签名真实性校验（SM2 和证书链）、CCITT/G4 图像，
以及少量高级批注混合模式。

## 快速开始

要求：较新的 Rust 工具链。

```bash
# 将 OFD 的第一页渲染为 300 DPI PNG
cargo run -p ofd-cli -- render invoice.ofd out.png --dpi 300

# 渲染指定页面，或裁剪区域（单位：像素）
cargo run -p ofd-cli -- render doc.ofd p2.png --dpi 300 --page 1
cargo run -p ofd-cli -- render doc.ofd crop.png --dpi 300 --region 0,0,800,400

# 校验签名文档的完整性
cargo run -p ofd-cli -- verify invoice.ofd
```

对于未嵌入字体的文档，可以获取项目提供的后备字体，以获得跨平台一致的
中文渲染效果。字体约 47 MB，不会提交到 Git：

```bash
scripts/fetch-fonts.sh
```

## macOS Quick Look 预览

桌面应用内置两个 macOS Quick Look 扩展（`.appex`），随 `.app` 一起分发：

- **预览**（`OFDQuickLookPreview`）：在 Finder 中选中 `.ofd` 文件按空格，
  直接查看原生渲染的多页预览（默认最多前 32 页、144 DPI）。
- **缩略图**（`OFDQuickLookThumbnail`）：在 Finder 图标视图和快速查看列表中
  显示首页缩略图。

| 项目 | 说明 |
| --- | --- |
| 系统要求 | 扩展需要 macOS 12+；应用本体的平台要求不变 |
| 支持的 UTI | `public.ofd`（macOS 27.2+ 系统声明）和 `com.ofdmanager.ofd`（应用导出，旧系统回退） |
| 安装 | 把 `OFD Manager.app` 放到 `/Applications`（或其他被 LaunchServices 索引的位置）并至少启动一次，扩展才会注册 |
| 验证 | Finder 中选中 `.ofd` 按空格；或 `qlmanage -p invoice.ofd`、`qlmanage -x -t -s 512 -o out invoice.ofd` |

macOS 27.2 起系统自带 OFD Quick Look 扩展（`com.apple.OFDPreview`）。若系统扩展
和应用扩展同时启用，由系统决定使用哪一个；可以在「系统设置 > 通用 > 登录项与
扩展」中调整，也可以用 `pluginkit` 选择：

```bash
# 优先使用本应用的预览扩展；恢复系统扩展：pluginkit -e default -i com.apple.OFDPreview
pluginkit -e use -i com.ofdmanager.desktop.quicklook.preview
pluginkit -e ignore -i com.apple.OFDPreview
```

构建方式、验证步骤与架构说明见 [`docs/quicklook.md`](docs/quicklook.md)。

## 项目结构

```text
crates/
  ofd-core/       可移植的渲染核心（解析 → 模型 → 位图），不负责 I/O 或 UI。
  ofd-cli/        命令行入口，支持 render 和 verify。
  ofd-quicklook/  macOS Quick Look 扩展使用的 C ABI 渲染静态库（复用 ofd-core）。
apps/desktop/     基于 Tauri v2、React 和 TypeScript 的桌面查看器。
  macos/quicklook/  macOS Quick Look 预览 / 缩略图扩展（Swift + .appex）。
fixtures/         OFD 示例文件及回归测试用参考页面图像。
docs/             GB/T 33190 标准及相关资料。
scripts/          辅助脚本，例如 fetch-fonts.sh、build-quicklook.sh 和 install-hooks.sh。
```

核心库保持纯函数式边界：输入字节，输出模型或图像，不自行访问文件系统，
也不创建线程。这样可以在未来编译为 WebAssembly，用于浏览器查看器，也可
复用于原生桌面和移动端应用。

## 构建与测试

```bash
cargo build --workspace
cargo test --workspace
```

测试套件包含**黄金图像回归测试**：每个示例文件都会被渲染，并将存在参考
图像的页面进行感知比较，以检测布局、颜色和字体方面的回归。

桌面端前端检查：

```bash
npm --prefix apps/desktop install
npm --prefix apps/desktop run lint
npm --prefix apps/desktop run test
npm --prefix apps/desktop run build
```

macOS Quick Look 扩展可以单独构建并自检（其他平台会跳过）：

```bash
scripts/build-quicklook.sh --check
```

完整打包应用时会自动构建扩展并嵌入 `Contents/PlugIns`：

```bash
cd apps/desktop && npx tauri build --bundles app
```

## Git 提交检查

启用项目共享的 pre-commit hook（每个 clone 执行一次）：

```bash
scripts/install-hooks.sh
```

Hook 会先检查暂存区空白问题；Rust 或桌面端文件发生变化时，分别执行与 CI 一致的格式化、Clippy、测试，以及前端 lint、测试和构建检查。

## 路线图

- 持续完善 Tauri v2 + React 桌面应用
- 完善 OFD 到 PDF 的导出能力
- Android、iOS 和纯 WebAssembly Web 查看器
- 完整的签名真实性校验（SM2 签名和证书验证）
- 本地文件管理（浏览、最近使用、收藏）

## 标准

- **GB/T 33190-2016**：OFD 文档格式，解析和渲染目标
- **GB/T 38540 / GM/T 0031**：电子签章（SES）结构
- **GB/T 32905**：SM3 哈希算法，用于签名摘要校验

## 字体说明

内嵌字体的文档可以获得稳定的渲染结果。对于只声明字体名称、但未嵌入字体
的文档（例如宋体或 SimSun），渲染结果取决于系统中可用的字体。通过脚本
获取的后备字体集合（Windows 常用 CJK 字体）可以提供确定性的跨平台输出，
但这些字体不会被重新分发到本仓库中。

## 测试文件来源

`fixtures/` 中的 OFD 测试文件及其来源链接、SHA-256 校验值和许可说明见
[`fixtures/README.md`](fixtures/README.md)。
