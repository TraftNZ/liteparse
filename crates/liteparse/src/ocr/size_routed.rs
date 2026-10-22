use std::{pin::Pin, sync::Arc};

use crate::error::LiteParseError;
use crate::ocr::{OcrEngine, OcrOptions, OcrRecognitionResult};

/// Routes each raster to one of two engines by its long edge: rasters whose
/// long edge is below `small_max_long_edge_px` go to `small`, every other
/// raster to `large`. Lets a fast whole-image engine read small rasters while
/// a tiling engine keeps the small lettering of large sheets.
pub struct SizeRoutedOcrEngine {
    small: Arc<dyn OcrEngine>,
    large: Arc<dyn OcrEngine>,
    small_max_long_edge_px: u32,
}

impl SizeRoutedOcrEngine {
    /// Both engines receive the same raster buffer, so they must agree on its
    /// pixel format (`OcrEngine::prefers_grayscale`).
    pub fn new(
        small: Arc<dyn OcrEngine>,
        large: Arc<dyn OcrEngine>,
        small_max_long_edge_px: u32,
    ) -> Result<Self, LiteParseError> {
        if small_max_long_edge_px == 0 {
            return Err("small-image OCR long-edge threshold must be positive".into());
        }
        if small.prefers_grayscale() != large.prefers_grayscale() {
            return Err(format!(
                "OCR engines {:?} and {:?} expect different raster pixel formats",
                small.name(),
                large.name()
            )
            .into());
        }
        Ok(Self {
            small,
            large,
            small_max_long_edge_px,
        })
    }

    fn route(&self, width: u32, height: u32) -> &dyn OcrEngine {
        if width.max(height) < self.small_max_long_edge_px {
            self.small.as_ref()
        } else {
            self.large.as_ref()
        }
    }
}

impl OcrEngine for SizeRoutedOcrEngine {
    fn name(&self) -> &str {
        "size-routed"
    }

    fn prefers_grayscale(&self) -> bool {
        self.large.prefers_grayscale()
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn recognize<'a, 'b: 'a, 'c: 'a>(
        &'a self,
        image_data: &'c [u8],
        width: u32,
        height: u32,
        options: &'b OcrOptions,
    ) -> Pin<Box<dyn Future<Output = OcrRecognitionResult> + Send + '_>> {
        self.route(width, height)
            .recognize(image_data, width, height, options)
    }

    #[cfg(target_arch = "wasm32")]
    fn recognize<'a, 'b: 'a, 'c: 'a>(
        &'a self,
        image_data: &'c [u8],
        width: u32,
        height: u32,
        options: &'b OcrOptions,
    ) -> Pin<Box<dyn Future<Output = OcrRecognitionResult> + '_>> {
        self.route(width, height)
            .recognize(image_data, width, height, options)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ocr::OcrResult;

    const THRESHOLD_PX: u32 = 1700;

    struct NamedEngine {
        name: &'static str,
        grayscale: bool,
    }

    impl OcrEngine for NamedEngine {
        fn name(&self) -> &str {
            self.name
        }

        fn prefers_grayscale(&self) -> bool {
            self.grayscale
        }

        fn recognize<'a, 'b: 'a, 'c: 'a>(
            &'a self,
            _image_data: &'c [u8],
            _width: u32,
            _height: u32,
            _options: &'b OcrOptions,
        ) -> Pin<Box<dyn Future<Output = OcrRecognitionResult> + Send + '_>> {
            let text = self.name.to_string();
            Box::pin(async move {
                Ok(vec![OcrResult {
                    text,
                    bbox: [0.0, 0.0, 1.0, 1.0],
                    confidence: 1.0,
                    polygon: None,
                }])
            })
        }
    }

    fn engine(name: &'static str, grayscale: bool) -> Arc<dyn OcrEngine> {
        Arc::new(NamedEngine { name, grayscale })
    }

    async fn routed_to(router: &SizeRoutedOcrEngine, width: u32, height: u32) -> String {
        let options = OcrOptions {
            language: "en".into(),
            dpi: 200.0,
        };
        router
            .recognize(&[], width, height, &options)
            .await
            .unwrap()
            .remove(0)
            .text
    }

    #[tokio::test]
    async fn routes_by_long_edge() {
        let router =
            SizeRoutedOcrEngine::new(engine("small", false), engine("large", false), THRESHOLD_PX)
                .unwrap();
        assert_eq!(routed_to(&router, 1200, 1699).await, "small");
        assert_eq!(routed_to(&router, 1699, 900).await, "small");
        assert_eq!(routed_to(&router, 1200, 1700).await, "large");
        assert_eq!(routed_to(&router, 1654, 2339).await, "large");
        assert_eq!(routed_to(&router, 3308, 2339).await, "large");
    }

    #[test]
    fn rejects_mismatched_pixel_formats() {
        let err =
            SizeRoutedOcrEngine::new(engine("small", true), engine("large", false), THRESHOLD_PX)
                .err()
                .unwrap();
        assert!(err.to_string().contains("pixel formats"));
    }

    #[test]
    fn rejects_zero_threshold() {
        assert!(
            SizeRoutedOcrEngine::new(engine("small", false), engine("large", false), 0).is_err()
        );
    }
}
