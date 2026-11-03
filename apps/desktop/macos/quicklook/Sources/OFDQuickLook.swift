//
//  OFDQuickLook.swift
//  Shared FFI bridge between the Quick Look extensions and the Rust renderer
//  (crates/ofd-quicklook, built into libofd_quicklook.a).
//
//  The C ABI is frozen in crates/ofd-quicklook/include/ofd_quicklook.h:
//
//      OfdQuickLookResult *ofd_quicklook_render_preview_pdf(const char *path_utf8,
//                                                           uint32_t max_pages,
//                                                           float dpi,
//                                                           uint64_t max_pixels);
//      OfdQuickLookResult *ofd_quicklook_render_thumbnail_png(const char *path_utf8,
//                                                             uint32_t max_dimension);
//      void ofd_quicklook_result_free(OfdQuickLookResult *result);
//
//  Everything in this file is plain Foundation: it is shared by both appex
//  executables and by the Swift<->Rust ABI smoke-test binary (Tests/BridgeSmoke.swift).
//  The C functions are synchronous and reentrant, so the bridge is safe to call
//  from any background thread/queue (both extensions do exactly that).
//

import Foundation

/// Status codes returned by the Rust renderer (frozen contract v1).
public enum OFDQuickLookStatus: Int32 {
    case ok = 0
    case invalidArgument = 1
    case io = 2
    case containerOrParse = 3
    case noPages = 4
    case render = 5
    case pdfEncoding = 6
    case pngEncoding = 7
    case allocationOrOther = 8

    /// Human readable name used in error messages / logs.
    public var label: String {
        switch self {
        case .ok: return "ok"
        case .invalidArgument: return "invalid argument"
        case .io: return "i/o error"
        case .containerOrParse: return "container/parse error"
        case .noPages: return "no pages"
        case .render: return "render error"
        case .pdfEncoding: return "PDF encoding error"
        case .pngEncoding: return "PNG encoding error"
        case .allocationOrOther: return "allocation/other error"
        }
    }
}

/// Error surfaced by ``OFDQuickLookBridge`` and consumed by `LocalizedError`-aware
/// code paths (Quick Look logs the localized description when a provider throws).
public struct OFDQuickLookError: Error, LocalizedError, CustomStringConvertible {
    /// Status code from the frozen C ABI (see ``OFDQuickLookStatus``).
    public let status: Int32
    /// Renderer-supplied UTF-8 error message (never empty in practice).
    public let message: String

    public init(status: Int32, message: String) {
        self.status = status
        self.message = message
    }

    public var errorDescription: String? {
        let label = OFDQuickLookStatus(rawValue: status)?.label ?? "unknown error"
        return "OFD Quick Look \(label) (status \(status)): \(message)"
    }

    public var description: String { errorDescription ?? "OFD Quick Look error \(status): \(message)" }
}

/// Thin Swift wrapper around the Rust staticlib.
///
/// Neither method touches the filesystem itself; the Rust renderer opens and
/// parses the OFD file. Both are synchronous and safe to call from any thread.
public enum OFDQuickLookBridge {

    /// Render an image-based PDF preview of an OFD document.
    ///
    /// - Parameters:
    ///   - path: filesystem path of the `.ofd` file (UTF-8).
    ///   - maxPages: maximum number of pages; `0` selects the renderer default (32).
    ///   - dpi: rasterization DPI, clamped by the renderer to 36...600; `0` selects 144.
    ///   - maxPixels: total pixel budget; `0` selects 50_000_000. The renderer lowers
    ///     the effective DPI when the budget would be exceeded.
    /// - Returns: complete PDF bytes (`%PDF-...`).
    /// - Throws: ``OFDQuickLookError`` with the renderer status code and message.
    public static func renderPreviewPDF(
        path: String,
        maxPages: UInt32 = 0,
        dpi: Float = 0,
        maxPixels: UInt64 = 0
    ) throws -> Data {
        try call(path: path) { cPath in
            ofd_quicklook_render_preview_pdf(cPath, maxPages, dpi, maxPixels)
        }
    }

    /// Render page 1 of an OFD document as an RGBA PNG thumbnail.
    ///
    /// - Parameters:
    ///   - path: filesystem path of the `.ofd` file (UTF-8).
    ///   - maxDimension: longest-edge limit in pixels; `0` selects the renderer
    ///     default (1024). Values outside 32...4096 are clamped by the renderer.
    /// - Returns: PNG bytes.
    /// - Throws: ``OFDQuickLookError`` with the renderer status code and message.
    public static func renderThumbnailPNG(path: String, maxDimension: UInt32 = 0) throws -> Data {
        try call(path: path) { cPath in
            ofd_quicklook_render_thumbnail_png(cPath, maxDimension)
        }
    }

    /// Invoke a C renderer entry point and copy the owned result buffer into `Data`.
    ///
    /// The C contract guarantees the result pointer is non-NULL except on allocation
    /// failure, `data`/`error_message` are owned by the result and released by
    /// `ofd_quicklook_result_free`, and a non-zero `status` always carries a message.
    private static func call(
        path: String,
        _ body: (UnsafePointer<CChar>) -> UnsafeMutablePointer<OfdQuickLookResult>?
    ) throws -> Data {
        guard !path.isEmpty else {
            throw OFDQuickLookError(
                status: OFDQuickLookStatus.invalidArgument.rawValue,
                message: "empty file path"
            )
        }

        // `body` only borrows the C string for the duration of the synchronous call.
        let resultPointer = path.withCString { body($0) }
        guard let result = resultPointer else {
            throw OFDQuickLookError(
                status: OFDQuickLookStatus.allocationOrOther.rawValue,
                message: "renderer returned NULL (allocation failure)"
            )
        }
        defer { ofd_quicklook_result_free(result) }

        let payload = result.pointee
        if payload.status != 0 {
            let message = payload.error_message.map { String(cString: $0) } ?? "unknown renderer error"
            throw OFDQuickLookError(status: payload.status, message: message)
        }

        // `data` may legitimately be NULL for an empty payload; treat that as a failure
        // too, since a Quick Look reply with zero bytes is useless.
        guard let dataPointer = payload.data, payload.data_len > 0 else {
            throw OFDQuickLookError(
                status: OFDQuickLookStatus.allocationOrOther.rawValue,
                message: "renderer returned status 0 but no data"
            )
        }

        return Data(bytes: UnsafeRawPointer(dataPointer), count: Int(payload.data_len))
    }
}
