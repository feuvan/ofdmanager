/*
 * Bridging header shared by the OFD Quick Look extensions.
 *
 * The C ABI is frozen in crates/ofd-quicklook/include/ofd_quicklook.h and
 * implemented by the Rust staticlib (libofd_quicklook.a). swiftc is invoked
 * with `-import-objc-header <this file> -I crates/ofd-quicklook/include`, so
 * every Swift source in the compilation can call the three exported functions
 * directly:
 *
 *   ofd_quicklook_render_preview_pdf(path, max_pages, dpi, max_pixels)
 *   ofd_quicklook_render_thumbnail_png(path, max_dimension)
 *   ofd_quicklook_result_free(result)
 *
 * Nothing else is exposed to Swift: the shared bridge in Sources/OFDQuickLook.swift
 * owns allocation/freeing and converts the result to Foundation `Data`/`Error`.
 */
#ifndef OFD_QUICKLOOK_BRIDGE_H
#define OFD_QUICKLOOK_BRIDGE_H

#include <stddef.h>
#include <stdint.h>

#include <ofd_quicklook.h>

#endif /* OFD_QUICKLOOK_BRIDGE_H */
