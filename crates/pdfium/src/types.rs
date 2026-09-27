#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct RectF {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct CharBox {
    pub left: f64,
    pub right: f64,
    pub bottom: f64,
    pub top: f64,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Matrix {
    pub a: f32,
    pub b: f32,
    pub c: f32,
    pub d: f32,
    pub e: f32,
    pub f: f32,
}

impl Matrix {
    /// Principal affine scales, ordered from largest to smallest.
    pub fn scale_factors(&self) -> (f32, f32) {
        let (a, b, c, d) = (
            f64::from(self.a),
            f64::from(self.b),
            f64::from(self.c),
            f64::from(self.d),
        );
        let mt_a = a * a + b * b;
        let mt_b = a * c + b * d;
        let mt_d = c * c + d * d;
        let first = (mt_a + mt_d) / 2.0;
        let disc = ((mt_a + mt_d).powi(2) - 4.0 * (mt_a * mt_d - mt_b * mt_b)).sqrt() / 2.0;
        let sx = (first + disc).sqrt();
        let sy = (first - disc).sqrt();
        let sx = if sx.is_nan() { 1.0 } else { sx };
        let sy = if sy.is_nan() { 1.0 } else { sy };
        (sx as f32, sy as f32)
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct TextRect {
    pub left: f64,
    pub top: f64,
    pub right: f64,
    pub bottom: f64,
}
