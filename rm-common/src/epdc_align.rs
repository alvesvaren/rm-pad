//! 8×8 alignment for imx EPDC partial updates (host + tablet use the same geometry).

/// Expand a rectangle to 8-pixel boundaries, clamp to the framebuffer, force even width.
/// Returns `(left, top, width, height)`; `(0,0,0,0)` if nothing drawable remains.
pub fn expand_rect_to_epdc_grid(
    left: u32,
    top: u32,
    width: u32,
    height: u32,
    fb_w: u32,
    fb_h: u32,
) -> (u32, u32, u32, u32) {
    if width < 2 || height < 1 {
        return (0, 0, 0, 0);
    }
    let left_a = (left / 8) * 8;
    let top_a = (top / 8) * 8;
    let right = ((left + width + 7) / 8) * 8;
    let bottom = ((top + height + 7) / 8) * 8;
    let right = right.min(fb_w);
    let bottom = bottom.min(fb_h);
    let mut w = right.saturating_sub(left_a);
    let h = bottom.saturating_sub(top_a);
    if w < 2 || h < 1 {
        return (0, 0, 0, 0);
    }
    w &= !1;
    if w < 2 {
        return (0, 0, 0, 0);
    }
    (left_a, top_a, w, h)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_idempotent_for_grid_aligned_rect() {
        let fb_w = 1404u32;
        let fb_h = 1872u32;
        let (l, t, w, h) = expand_rect_to_epdc_grid(16, 24, 400, 304, fb_w, fb_h);
        let (l2, t2, w2, h2) = expand_rect_to_epdc_grid(l, t, w, h, fb_w, fb_h);
        assert_eq!((l, t, w, h), (l2, t2, w2, h2));
    }
}
