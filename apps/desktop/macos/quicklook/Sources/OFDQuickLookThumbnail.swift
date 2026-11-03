//
//  OFDQuickLookThumbnail.swift
//  Quick Look thumbnail extension for `.ofd` documents.
//
//  PlugInKit instantiates the principal class named in Info.plist
//  (NSExtensionPrincipalClass = ThumbnailProvider) and Quick Look calls
//  `provideThumbnail(for:_:)`. We render page 1 to a PNG through the Rust
//  staticlib on a background queue, write it into the extension's temporary
//  directory and return `QLThumbnailReply(imageFileURL:)`.
//

import Foundation
import os
import QuickLookThumbnailing

private let thumbnailLog = Logger(subsystem: "com.ofdmanager.quicklook", category: "thumbnail")

/// Principal class of `OFDQuickLookThumbnail.appex`.
@objc(ThumbnailProvider)
final class ThumbnailProvider: QLThumbnailProvider {

    /// Renderer accepts 32...4096; clamp a bit tighter (both bounds are valid).
    private static let minDimension: Double = 64
    private static let maxDimension: Double = 2048

    override func provideThumbnail(
        for request: QLFileThumbnailRequest,
        _ handler: @escaping (QLThumbnailReply?, Error?) -> Void
    ) {
        let fileURL = request.fileURL
        let requestedEdge = max(request.maximumSize.width, request.maximumSize.height) * request.scale
        let dimension = UInt32(
            min(Self.maxDimension, max(Self.minDimension, requestedEdge.rounded(.up)))
        )

        thumbnailLog.info(
            "thumbnail request for \(fileURL.lastPathComponent, privacy: .public) maxSize=\(request.maximumSize.width, privacy: .public)x\(request.maximumSize.height, privacy: .public) scale=\(request.scale, privacy: .public) -> \(dimension, privacy: .public)px"
        )

        // The completion handler is explicitly allowed to be called asynchronously,
        // so do the render on a background queue. Don't capture `self` in the block:
        // the provider may be deallocated before the block finishes.
        DispatchQueue.global(qos: .userInitiated).async {
            do {
                let png = try OFDQuickLookBridge.renderThumbnailPNG(
                    path: fileURL.path,
                    maxDimension: dimension
                )

                // Quick Look consumes the image file after the reply is returned; leave
                // cleanup to the OS temp-file lifecycle rather than racing the reader.
                let thumbnailURL = FileManager.default.temporaryDirectory
                    .appendingPathComponent("ofd-quicklook-\(UUID().uuidString).png")
                try png.write(to: thumbnailURL, options: .atomic)

                let reply = QLThumbnailReply(imageFileURL: thumbnailURL)
                reply.extensionBadge = "OFD"
                thumbnailLog.info(
                    "thumbnail ready for \(fileURL.lastPathComponent, privacy: .public): \(png.count) bytes"
                )
                handler(reply, nil)
            } catch {
                thumbnailLog.error(
                    "thumbnail render failed for \(fileURL.lastPathComponent, privacy: .public): \(String(describing: error), privacy: .public)"
                )
                handler(nil, error)
            }
        }
    }
}
