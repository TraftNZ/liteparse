//! Deterministic page classification shared by the full geometry extractor.

const VECTOR_PATH_OPS_THRESHOLD: usize = 40;
const RASTER_PATH_OPS_THRESHOLD: usize = 10;

pub fn classify(path_ops: usize, image_ops: usize, text_span_count: usize) -> &'static str {
    if path_ops >= VECTOR_PATH_OPS_THRESHOLD {
        if image_ops > 0 { "mixed" } else { "vector" }
    } else if path_ops < RASTER_PATH_OPS_THRESHOLD && image_ops > 0 {
        "raster"
    } else if text_span_count > 0 {
        "text"
    } else {
        "mixed"
    }
}

pub fn image_coverage(image_area: f64, width_pts: f64, height_pts: f64) -> f64 {
    let page_area = width_pts * height_pts;
    if page_area <= 0.0 || image_area <= 0.0 {
        return 0.0;
    }
    (image_area / page_area).min(1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_baseline_class_boundaries() {
        assert_eq!(classify(39, 1, 0), "mixed");
        assert_eq!(classify(40, 1, 0), "mixed");
        assert_eq!(classify(40, 0, 0), "vector");
        assert_eq!(classify(9, 1, 0), "raster");
        assert_eq!(classify(10, 1, 1), "text");
        assert_eq!(classify(0, 0, 1), "text");
        assert_eq!(classify(0, 0, 0), "mixed");
    }

    #[test]
    fn overlap_is_summed_then_capped() {
        assert_eq!(image_coverage(2500.0, 100.0, 100.0), 0.25);
        assert_eq!(image_coverage(15000.0, 100.0, 100.0), 1.0);
        assert_eq!(image_coverage(1.0, 0.0, 100.0), 0.0);
    }
}
