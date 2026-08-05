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

## 项目结构

```text
crates/
  ofd-core/     可移植的渲染核心（解析 → 模型 → 位图），不负责 I/O 或 UI。
  ofd-cli/      命令行入口，支持 render 和 verify。
apps/desktop/   基于 Tauri v2、React 和 TypeScript 的桌面查看器。
fixtures/       OFD 示例文件及回归测试用参考页面图像。
docs/           GB/T 33190 标准及相关资料。
scripts/        辅助脚本，例如 fetch-fonts.sh。
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
