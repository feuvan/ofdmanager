//
//  BridgeSmoke.swift
//  Standalone Swift<->Rust ABI smoke test for the OFD Quick Look bridge.
//
//  Compiled by scripts/test-quicklook.sh against the real `libofd_quicklook.a`
//  with the same header/bridging-header setup as the appexes. It exercises the
//  three C entry points through the shared Swift bridge and validates the returned
//  bytes (`%PDF-` magic, PNG signature + IHDR dimensions) plus the error paths.
//  This catches Swift struct-layout / calling-convention / ownership mistakes that
//  a plain `swiftc` compile of the appex cannot.
//
//  Usage:
//      ofd-quicklook-bridge-smoke <fixture.ofd> [<fixture.ofd> ...]
//

import Foundation
import PDFKit

@main
struct BridgeSmoke {

    static func main() {
        let fixturePaths = Array(CommandLine.arguments.dropFirst())
        guard !fixturePaths.isEmpty else {
            FileHandle.standardError.write(
                Data("usage: ofd-quicklook-bridge-smoke <fixture.ofd> [<fixture.ofd> ...]\n".utf8)
            )
            exit(2)
        }

        var failures = 0
        func check(_ condition: Bool, _ message: String) {
            if condition {
                print("PASS \(message)")
            } else {
                print("FAIL \(message)")
                failures += 1
            }
        }

        for fixture in fixturePaths {
            let name = (fixture as NSString).lastPathComponent
            guard FileManager.default.fileExists(atPath: fixture) else {
                check(false, "fixture missing: \(fixture)")
                continue
            }

            // --- preview PDF (default knobs: maxPages=0, dpi=0, maxPixels=0) ---
            do {
                let pdf = try OFDQuickLookBridge.renderPreviewPDF(path: fixture)
                check(
                    pdf.starts(with: Data("%PDF".utf8)) && pdf.count > 1024,
                    "preview PDF \(name): \(pdf.count) bytes, %PDF magic"
                )
                // Parse with Apple's PDFKit: proves the bytes are a real PDF the Quick
                // Look host can display, and lets us report page count / page size.
                if let document = PDFDocument(data: pdf) {
                    let page = document.page(at: 0)
                    let bounds = page?.bounds(for: .mediaBox) ?? .zero
                    check(
                        document.pageCount >= 1 && document.pageCount <= 32,
                        "preview PDF \(name) parses via PDFKit: \(document.pageCount) page(s), first page "
                            + "\(Int(bounds.width.rounded()))x\(Int(bounds.height.rounded())) pt"
                    )
                } else {
                    check(false, "preview PDF \(name) is not parseable by PDFKit")
                }
                // A second call with explicit knobs exercises non-default argument passing
                // and max_pages truncation.
                let pdf2 = try OFDQuickLookBridge.renderPreviewPDF(
                    path: fixture,
                    maxPages: 1,
                    dpi: 72,
                    maxPixels: 40_000_000
                )
                let pageCount2 = PDFDocument(data: pdf2)?.pageCount ?? -1
                check(
                    pdf2.starts(with: Data("%PDF".utf8)) && pageCount2 == 1,
                    "preview PDF \(name) (maxPages=1, dpi=72): \(pdf2.count) bytes, \(pageCount2) page(s)"
                )
            } catch {
                check(false, "preview PDF \(name) threw: \(error)")
            }

            // --- thumbnail PNG ---
            do {
                let png = try OFDQuickLookBridge.renderThumbnailPNG(
                    path: fixture,
                    maxDimension: 512
                )
                let pngMagic = Data([0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A])
                let hasMagic = png.starts(with: pngMagic)
                let size = pngDimensions(png) ?? CGSize(width: 0, height: 0)
                check(
                    hasMagic && png.count > 512,
                    "thumbnail PNG \(name): \(png.count) bytes, PNG magic"
                )
                check(
                    size.width > 0 && size.height > 0
                        && max(size.width, size.height) <= 512
                        && min(size.width, size.height) >= 32,
                    "thumbnail PNG \(name) dimensions: \(Int(size.width))x\(Int(size.height)) (longest edge <= 512)"
                )
            } catch {
                check(false, "thumbnail PNG \(name) threw: \(error)")
            }
        }

        // --- error paths: status codes must cross the ABI correctly ---
        do {
            _ = try OFDQuickLookBridge.renderThumbnailPNG(path: "/nonexistent/ofd-quicklook-smoke.ofd")
            check(false, "missing file: expected an error, got data")
        } catch let error as OFDQuickLookError {
            check(
                error.status == OFDQuickLookStatus.io.rawValue && !error.message.isEmpty,
                "missing file -> status 2 (i/o), message: \(error.message)"
            )
        } catch {
            check(false, "missing file: unexpected error type: \(error)")
        }

        do {
            _ = try OFDQuickLookBridge.renderPreviewPDF(path: "")
            check(false, "empty path: expected an error, got data")
        } catch let error as OFDQuickLookError {
            check(
                error.status == OFDQuickLookStatus.invalidArgument.rawValue,
                "empty path -> status 1 (invalid argument)"
            )
        } catch {
            check(false, "empty path: unexpected error type: \(error)")
        }

        let junkURL = FileManager.default.temporaryDirectory
            .appendingPathComponent("ofd-quicklook-smoke-junk-\(UUID().uuidString).ofd")
        guard (try? Data("this is not an OFD container".utf8).write(to: junkURL)) != nil else {
            check(false, "could not write junk fixture")
            finish(failures)
        }
        do {
            _ = try OFDQuickLookBridge.renderPreviewPDF(path: junkURL.path)
            check(false, "junk file: expected an error, got data")
        } catch let error as OFDQuickLookError {
            check(
                error.status == OFDQuickLookStatus.containerOrParse.rawValue,
                "junk file -> status 3 (container/parse), message: \(error.message)"
            )
        } catch {
            check(false, "junk file: unexpected error type: \(error)")
        }
        // `finish()` calls exit(), which skips `defer`, so clean up explicitly here.
        try? FileManager.default.removeItem(at: junkURL)

        finish(failures)
    }

    /// Parse width/height from a PNG IHDR chunk (bytes 16...23, big endian).
    private static func pngDimensions(_ data: Data) -> CGSize? {
        guard data.count >= 24,
              data[12] == 0x49, data[13] == 0x48, data[14] == 0x44, data[15] == 0x52 // "IHDR"
        else { return nil }
        let width = data[16..<20].reduce(UInt32(0)) { ($0 << 8) | UInt32($1) }
        let height = data[20..<24].reduce(UInt32(0)) { ($0 << 8) | UInt32($1) }
        return CGSize(width: CGFloat(width), height: CGFloat(height))
    }

    private static func finish(_ failures: Int) -> Never {
        if failures > 0 {
            print("FAIL bridge smoke: \(failures) failure(s)")
            exit(1)
        }
        print("PASS bridge smoke: Swift<->Rust ABI ok")
        exit(0)
    }
}
