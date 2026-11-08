//! Where the remote image is drawn inside the remote-screen area, and the inverse
//! mapping from a mouse position to a remote pixel.

use crate::model::Scaling;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// Rectangle (relative to `area`'s origin) that the image occupies. It may extend
/// beyond the area for `Fill` and for a 100% image larger than the window.
pub fn image_rect(area_w: f32, area_h: f32, iw: f32, ih: f32, scaling: Scaling) -> Rect {
    if iw <= 0.0 || ih <= 0.0 {
        return Rect { x: 0.0, y: 0.0, w: 0.0, h: 0.0 };
    }
    let scale = match scaling {
        Scaling::Fit => (area_w / iw).min(area_h / ih),
        Scaling::Fill => (area_w / iw).max(area_h / ih),
        Scaling::Actual => 1.0,
    };
    let (w, h) = (iw * scale, ih * scale);
    Rect { x: (area_w - w) / 2.0, y: (area_h - h) / 2.0, w, h }
}

/// Map a position relative to the area's origin to a remote pixel. Positions on
/// the letterbox (outside the image) clamp to the nearest edge pixel; `None` only
/// if there is no image.
pub fn to_remote(area_w: f32, area_h: f32, iw: u32, ih: u32, scaling: Scaling, px: f32, py: f32) -> Option<(u16, u16)> {
    if iw == 0 || ih == 0 {
        return None;
    }
    let r = image_rect(area_w, area_h, iw as f32, ih as f32, scaling);
    if r.w <= 0.0 || r.h <= 0.0 {
        return None;
    }
    let fx = ((px - r.x) / r.w * iw as f32).floor();
    let fy = ((py - r.y) / r.h * ih as f32).floor();
    Some((fx.clamp(0.0, iw as f32 - 1.0) as u16, fy.clamp(0.0, ih as f32 - 1.0) as u16))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_letterboxes_wide_area() {
        // 1920x1080 image in a 2000x1000 area: height-limited, centred horizontally.
        let r = image_rect(2000.0, 1000.0, 1920.0, 1080.0, Scaling::Fit);
        assert!((r.h - 1000.0).abs() < 0.01);
        assert!((r.w - 1777.78).abs() < 0.1);
        assert!((r.x - (2000.0 - r.w) / 2.0).abs() < 0.01);
        assert_eq!(r.y, 0.0);
    }

    #[test]
    fn fill_covers_and_actual_is_one_to_one() {
        let f = image_rect(1000.0, 1000.0, 1600.0, 900.0, Scaling::Fill);
        assert!(f.w >= 1000.0 && (f.h - 1000.0).abs() < 0.01 && f.x < 0.0);
        let a = image_rect(1000.0, 800.0, 400.0, 200.0, Scaling::Actual);
        assert_eq!((a.w, a.h, a.x, a.y), (400.0, 200.0, 300.0, 300.0));
    }

    #[test]
    fn mouse_mapping_hits_corners_and_centre() {
        let m = |px, py| to_remote(1000.0, 500.0, 1000, 500, Scaling::Fit, px, py).unwrap();
        assert_eq!(m(0.0, 0.0), (0, 0));
        assert_eq!(m(500.0, 250.0), (500, 250));
        assert_eq!(m(999.9, 499.9), (999, 499));
    }

    #[test]
    fn letterbox_clicks_clamp_to_edges() {
        // 1:1 image centred in a larger area: area (500,300) is the image's centre.
        let m = |px, py| to_remote(1000.0, 600.0, 400, 200, Scaling::Actual, px, py).unwrap();
        assert_eq!(m(500.0, 300.0), (200, 100));
        assert_eq!(m(0.0, 0.0), (0, 0));
        assert_eq!(m(5000.0, 5000.0), (399, 199));
    }

    #[test]
    fn fill_mapping_accounts_for_cropping() {
        // 2:1 image filling a square area is cropped left/right; centre maps to centre.
        let (x, y) = to_remote(100.0, 100.0, 200, 100, Scaling::Fill, 50.0, 50.0).unwrap();
        assert_eq!((x, y), (100, 50));
        let (x0, _) = to_remote(100.0, 100.0, 200, 100, Scaling::Fill, 0.0, 50.0).unwrap();
        assert_eq!(x0, 50);
    }

    #[test]
    fn empty_image_maps_to_none() {
        assert!(to_remote(100.0, 100.0, 0, 0, Scaling::Fit, 1.0, 1.0).is_none());
    }
}
