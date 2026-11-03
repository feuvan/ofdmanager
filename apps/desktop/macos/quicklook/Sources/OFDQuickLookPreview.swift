//
//  OFDQuickLookPreview.swift
//  Data-based Quick Look preview extension for `.ofd` documents.
//
//  PlugInKit instantiates the principal class named in Info.plist
//  (NSExtensionPrincipalClass = PreviewProvider) and Quick Look calls
//  `providePreview(for:)` on it. We render the document to an image-based PDF
//  through the Rust staticlib on a background task, then hand Quick Look a
//  `QLPreviewReply` whose data-creation block simply returns the already-rendered
//  bytes (no AppKit drawing happens in that block).
//

import Foundation
import os
import QuickLookUI
import UniformTypeIdentifiers

private let previewLog = Logger(subsystem: "com.ofdmanager.quicklook", category: "preview")

/// Principal class of `OFDQuickLookPreview.appex`.
@objc(PreviewProvider)
final class PreviewProvider: QLPreviewProvider, QLPreviewingController {

    /// A4 at 72 dpi — used when the PDF's own page size cannot be determined.
    private static let fallbackPageSize = CGSize(width: 595, height: 842)

    func providePreview(for request: QLFilePreviewRequest) async throws -> QLPreviewReply {
        let fileURL = request.fileURL
        previewLog.info("preview request for \(fileURL.lastPathComponent, privacy: .public)")

        let pdfData: Data
        do {
            // Rust parsing + rasterization + PDF encoding is CPU heavy; keep it off
            // the extension's main thread (Quick Look's XPC queue).
            pdfData = try await Task.detached(priority: .userInitiated) { () throws -> Data in
                try OFDQuickLookBridge.renderPreviewPDF(path: fileURL.path)
            }.value
        } catch {
            previewLog.error(
                "preview render failed for \(fileURL.lastPathComponent, privacy: .public): \(String(describing: error), privacy: .public)"
            )
            // Quick Look falls back to a generic preview when the provider throws.
            throw error
        }

        previewLog.info(
            "preview ready for \(fileURL.lastPathComponent, privacy: .public): \(pdfData.count) bytes"
        )

        let contentSize = Self.pdfPageSize(from: pdfData) ?? Self.fallbackPageSize
        let reply = QLPreviewReply(dataOfContentType: .pdf, contentSize: contentSize) { _ in
            pdfData
        }
        reply.title = fileURL.lastPathComponent
        return reply
    }

    /// Best-effort extraction of the first page's MediaBox from the rendered PDF so
    /// Quick Look can show loading UI at the right size before the data arrives.
    /// PDFs carry their intrinsic page size, so a miss only costs a layout hint.
    private static func pdfPageSize(from data: Data) -> CGSize? {
        guard let head = String(data: data.prefix(16 * 1024), encoding: .isoLatin1),
              let mediaBox = head.range(of: "/MediaBox"),
              let open = head[mediaBox.upperBound...].firstIndex(of: "["),
              let close = head[open...].firstIndex(of: "]")
        else {
            return nil
        }

        let numbers = head[head.index(after: open)..<close]
            .split(whereSeparator: { $0 == " " || $0 == "\n" || $0 == "\r" || $0 == "\t" })
            .compactMap { Double($0) }
        guard numbers.count >= 4 else { return nil }

        let width = numbers[2] - numbers[0]
        let height = numbers[3] - numbers[1]
        // Sanity-check so a malformed box can never produce a broken reply hint.
        guard width >= 36, height >= 36, width <= 200_000, height <= 200_000 else { return nil }
        return CGSize(width: width, height: height)
    }
}
