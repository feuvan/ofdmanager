//! Geometry primitives. OFD uses a top-left origin (+X right, +Y down) and
//! millimetre units. Conversion to device pixels: `px = mm / 25.4 * dpi`.

/// Millimetres per inch, for mm→px conversion.
pub const MM_PER_INCH: f32 = 25.4;

/// Convert a millimetre length to device pixels at the given DPI.
#[inline]
pub fn mm_to_px(mm: f32, dpi: f32) -> f32 {
    mm / MM_PER_INCH * dpi
}

/// A point in millimetre document space.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

impl Point {
    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }
}

/// An axis-aligned rectangle, typically an object `Boundary`, in mm.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }
}

/// A 2x3 affine transform `[a b c d e f]` matching OFD's `CTM`, mapping a point
/// `(x, y)` to `(a*x + c*y + e, b*x + d*y + f)`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Matrix {
    pub a: f32,
    pub b: f32,
    pub c: f32,
    pub d: f32,
    pub e: f32,
    pub f: f32,
}

impl Matrix {
    pub const IDENTITY: Matrix = Matrix {
        a: 1.0,
        b: 0.0,
        c: 0.0,
        d: 1.0,
        e: 0.0,
        f: 0.0,
    };

    pub const fn new(a: f32, b: f32, c: f32, d: f32, e: f32, f: f32) -> Self {
        Self { a, b, c, d, e, f }
    }

    /// Apply the transform to a point.
    #[inline]
    pub fn apply(&self, p: Point) -> Point {
        Point::new(
            self.a * p.x + self.c * p.y + self.e,
            self.b * p.x + self.d * p.y + self.f,
        )
    }

    /// Compose `self` followed by `other` (i.e. `other * self`).
    pub fn then(&self, other: &Matrix) -> Matrix {
        Matrix {
            a: self.a * other.a + self.b * other.c,
            b: self.a * other.b + self.b * other.d,
            c: self.c * other.a + self.d * other.c,
            d: self.c * other.b + self.d * other.d,
            e: self.e * other.a + self.f * other.c + other.e,
            f: self.e * other.b + self.f * other.d + other.f,
        }
    }
}

impl Default for Matrix {
    fn default() -> Self {
        Matrix::IDENTITY
    }
}
