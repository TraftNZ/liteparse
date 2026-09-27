//! Preserve Go geometry rounding at thresholds and omitted-zero fields.
//!
//! Adapted from Go src/math/{hypot,atan,atan2,sin}.go
//! (Copyright 2009, 2010, 2011 The Go Authors).
//! See ../liteparse/licenses/Go-JPEG-LICENSE for the Go BSD license.

/// Go's Hypot rounds through max * sqrt(1 + (min/max)^2).
pub(crate) fn hypot(x: f64, y: f64) -> f64 {
    let maximum = x.abs().max(y.abs());
    let minimum = x.abs().min(y.abs());
    if maximum == 0.0 {
        return 0.0;
    }
    let ratio = minimum / maximum;
    maximum * (1.0 + ratio * ratio).sqrt()
}

// Go's arctangent uses the Cephes rational approximation and interval reduction.
// Original approximation: Stephen L. Moshier, Cephes Math Library Release 2.8
// (June 2000), copyright 1984, 1987, 1989, 1992, 2000; freely usable without
// support or guarantee. The Go adaptation is covered by the BSD license above.
fn atan_series(x: f64) -> f64 {
    const P: [f64; 5] = [
        -0.8750608600031904,
        -16.157537187333652,
        -75.00855792314705,
        -122.88666844901361,
        -64.85021904942025,
    ];
    const Q: [f64; 5] = [
        24.858464901423062,
        165.02700983169885,
        432.88106049129027,
        485.3903996359137,
        194.5506571482614,
    ];
    let squared = x * x;
    let numerator = (((P[0] * squared + P[1]) * squared + P[2]) * squared + P[3]) * squared + P[4];
    let denominator =
        ((((squared + Q[0]) * squared + Q[1]) * squared + Q[2]) * squared + Q[3]) * squared + Q[4];
    let ratio = squared * numerator / denominator;
    x * ratio + x
}

fn atan_positive(x: f64) -> f64 {
    const SERIES_LIMIT: f64 = 0.66;
    const TAN_THREE_PI_OVER_EIGHT: f64 = 2.414213562373095;
    const HALF_PI_LOW_BITS: f64 = 6.123233995736766e-17;
    use std::f64::consts::{FRAC_PI_2, FRAC_PI_4};
    if x <= SERIES_LIMIT {
        atan_series(x)
    } else if x > TAN_THREE_PI_OVER_EIGHT {
        FRAC_PI_2 - atan_series(1.0 / x) + HALF_PI_LOW_BITS
    } else {
        FRAC_PI_4 + atan_series((x - 1.0) / (x + 1.0)) + 0.5 * HALF_PI_LOW_BITS
    }
}

/// Match Go's direction rounding, including a nonzero delta between lines.
pub(crate) fn atan2(y: f64, x: f64) -> f64 {
    use std::f64::consts::{FRAC_PI_2, FRAC_PI_4, PI};
    if y.is_nan() || x.is_nan() {
        return f64::NAN;
    }
    if y == 0.0 {
        return if x.is_sign_negative() {
            PI.copysign(y)
        } else {
            0.0_f64.copysign(y)
        };
    }
    if x == 0.0 {
        return FRAC_PI_2.copysign(y);
    }
    if x.is_infinite() {
        return if y.is_infinite() {
            if x.is_sign_positive() {
                FRAC_PI_4.copysign(y)
            } else {
                (3.0 * FRAC_PI_4).copysign(y)
            }
        } else if x.is_sign_positive() {
            0.0_f64.copysign(y)
        } else {
            PI.copysign(y)
        };
    }
    if y.is_infinite() {
        return FRAC_PI_2.copysign(y);
    }
    let ratio = y / x;
    let angle = if ratio == 0.0 {
        ratio
    } else if ratio > 0.0 {
        atan_positive(ratio)
    } else {
        -atan_positive(-ratio)
    };
    if x < 0.0 {
        if angle <= 0.0 { angle + PI } else { angle - PI }
    } else {
        angle
    }
}

/// Go-compatible sine/cosine for directions produced by atan2 ([-pi, pi]).
pub(crate) fn direction_sin_cos(angle: f64) -> (f64, f64) {
    use std::f64::consts::PI;
    const QUARTER_PI_PARTS: [f64; 3] = [
        0.7853981256484985,
        3.774894707930798e-8,
        2.6951514290790595e-15,
    ];
    const SIN_COEFFICIENTS: [f64; 6] = [
        1.5896230157654656e-10,
        -2.5050747762857807e-08,
        2.7557313621385722e-06,
        -0.0001984126982958954,
        0.008333333333322118,
        -0.1666666666666663,
    ];
    const COS_COEFFICIENTS: [f64; 6] = [
        -1.1358536521387682e-11,
        2.087570084197473e-09,
        -2.755731417929674e-07,
        2.4801587288851704e-05,
        -0.0013888888888873056,
        0.041666666666666595,
    ];
    const OCTANTS_PER_TURN: u64 = 8;
    const HALF_TURN_OCTANTS: u64 = 4;
    if !angle.is_finite() {
        return (f64::NAN, f64::NAN);
    }
    debug_assert!(angle.abs() <= PI);
    let x = angle.abs();
    let mut octant = (x * (4.0 / PI)) as u64;
    let mut turns = octant as f64;
    if octant & 1 == 1 {
        octant += 1;
        turns += 1.0;
    }
    octant &= OCTANTS_PER_TURN - 1;
    let z = ((x - turns * QUARTER_PI_PARTS[0]) - turns * QUARTER_PI_PARTS[1])
        - turns * QUARTER_PI_PARTS[2];
    let mut sine_negative = angle.is_sign_negative();
    let mut cosine_negative = false;
    if octant > HALF_TURN_OCTANTS - 1 {
        octant -= HALF_TURN_OCTANTS;
        sine_negative = !sine_negative;
        cosine_negative = !cosine_negative;
    }
    if octant > 1 {
        cosine_negative = !cosine_negative;
    }
    let squared = z * z;
    let evaluate = |coefficients: &[f64; 6]| {
        coefficients[1..]
            .iter()
            .fold(coefficients[0], |value, coefficient| {
                value * squared + coefficient
            })
    };
    let sine_series = z + z * squared * evaluate(&SIN_COEFFICIENTS);
    let cosine_series = 1.0 - 0.5 * squared + squared * squared * evaluate(&COS_COEFFICIENTS);
    let (mut sine, mut cosine) = if octant == 1 || octant == 2 {
        (cosine_series, sine_series)
    } else {
        (sine_series, cosine_series)
    };
    if sine_negative {
        sine = -sine;
    }
    if cosine_negative {
        cosine = -cosine;
    }
    (sine, cosine)
}
