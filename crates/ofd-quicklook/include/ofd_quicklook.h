/*
 * OFD Quick Look — frozen C ABI v1.
 *
 * Linked into the macOS Quick Look preview / thumbnail appexes as a Rust
 * static library (libofd_quicklook.a). Owned buffers returned here are released
 * with ofd_quicklook_result_free(); no other allocator may free them.
 *
 * Status codes: 1 invalid argument, 2 I/O, 3 container/parse, 4 no pages,
 * 5 render, 6 PDF encoding, 7 PNG encoding, 8 allocation/other.
 */
#ifndef OFD_QUICKLOOK_H
#define OFD_QUICKLOOK_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct OfdQuickLookResult {
  int32_t status;        /* 0 = ok */
  uint8_t *data;         /* owned; may be NULL */
  size_t data_len;
  char *error_message;   /* owned; UTF-8, NUL terminated; may be NULL */
} OfdQuickLookResult;

OfdQuickLookResult *ofd_quicklook_render_preview_pdf(const char *path_utf8,
                                                     uint32_t max_pages,
                                                     float dpi,
                                                     uint64_t max_pixels);
OfdQuickLookResult *ofd_quicklook_render_thumbnail_png(const char *path_utf8,
                                                       uint32_t max_dimension);
void ofd_quicklook_result_free(OfdQuickLookResult *result);

#ifdef __cplusplus
}
#endif

#endif /* OFD_QUICKLOOK_H */
