//! `ofd-core` — portable OFD (GB/T 33190-2016) parsing and rendering.
//!
//! Design rule: **bytes in, model/bitmap out.** This crate performs no
//! filesystem, network, threading, or UI work, so the same code targets native
//! (Tauri desktop/mobile) and, later, `wasm32` for the web build. Hosts read the
//! file and inject the bytes; the core returns parsed metadata and rendered
//! page bitmaps.
//!
//! ```no_run
//! let bytes = std::fs::read("invoice.ofd").unwrap();
//! let pkg = ofd_core::open(bytes).unwrap();
//! let doc = &pkg.documents[0];
//! let bmp = ofd_core::render::render_page(doc, 0, 144.0).unwrap();
//! assert_eq!(bmp.rgba.len(), (bmp.width * bmp.height * 4) as usize);
//! ```

pub mod cff;
pub mod container;
pub mod error;
pub mod fonts;
pub mod geom;
pub mod model;
pub mod parser;
pub mod render;
pub mod ses;
pub mod sign;

pub use error::{OfdError, Result};
pub use model::{Document, OfdPackage};
pub use render::Bitmap;

/// Parse an OFD package from its full file bytes.
pub fn open(bytes: Vec<u8>) -> Result<OfdPackage> {
    parser::parse(bytes)
}

/// Parse an OFD package with caller-selected ZIP resource limits.
pub fn open_with_limits(bytes: Vec<u8>, limits: container::ContainerLimits) -> Result<OfdPackage> {
    parser::parse_with_limits(bytes, limits)
}
