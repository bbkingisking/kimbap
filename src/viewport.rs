//! Image-space geometry, independent of GTK and decoding.
#[derive(Debug, Clone)]
pub struct Viewport {
    pub image: [f64; 2],
    pub size: [f64; 2],
    pub zoom: f64,
    pub offset: [f64; 2],
}
impl Default for Viewport {
    fn default() -> Self {
        Self {
            image: [1., 1.],
            size: [1., 1.],
            zoom: 1.,
            offset: [0., 0.],
        }
    }
}
impl Viewport {
    pub fn fit(&self) -> f64 {
        (self.size[0] / self.image[0]).min(self.size[1] / self.image[1])
    }
    pub fn scale(&self) -> f64 {
        self.fit() * self.zoom
    }
    pub fn reset(&mut self) {
        self.zoom = 1.;
        self.offset = [0., 0.];
    }
    pub fn clamp(&mut self) {
        for i in 0..2 {
            let excess = ((self.image[i] * self.scale() - self.size[i]) / 2.).max(0.);
            self.offset[i] = self.offset[i].clamp(-excess, excess);
        }
    }
    pub fn resize(&mut self, width: f64, height: f64) {
        self.size = [width.max(1.), height.max(1.)];
        self.clamp();
    }
    pub fn zoom_to(&mut self, zoom: f64, anchor: [f64; 2]) {
        let zoom = zoom.clamp(0.1, 32.);
        let ratio = zoom / self.zoom;
        for (i, position) in anchor.into_iter().enumerate() {
            let relative = position - self.size[i] / 2.;
            self.offset[i] = relative - (relative - self.offset[i]) * ratio;
        }
        self.zoom = zoom;
        self.clamp();
    }
    pub fn pan_to(&mut self, offset: [f64; 2]) {
        self.offset = offset;
        self.clamp();
    }
    /// Pixel bounds of the displayed image, including zoom and device density.
    pub fn raster_size(&self, density: f64, max_edge: u32) -> (u32, u32) {
        let scale = self.scale() * density;
        let width = self.image[0] * scale;
        let height = self.image[1] * scale;
        let limit = (max_edge as f64 / width.max(height)).min(1.);
        (
            (width * limit).round().clamp(1., max_edge as f64) as u32,
            (height * limit).round().clamp(1., max_edge as f64) as u32,
        )
    }
    pub fn bounds(&self) -> [f32; 4] {
        let scale = self.scale();
        [
            (self.size[0] - self.image[0] * scale) as f32 / 2. + self.offset[0] as f32,
            (self.size[1] - self.image[1] * scale) as f32 / 2. + self.offset[1] as f32,
            (self.image[0] * scale) as f32,
            (self.image[1] * scale) as f32,
        ]
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn document_raster_tracks_fractional_density_and_caps_the_page() {
        let mut view = Viewport {
            image: [700., 990.],
            size: [1702., 990.],
            ..Default::default()
        };
        // 150% display scale, two samples per device pixel on each axis.
        assert_eq!(view.raster_size(1.5 * 2., 4096), (2100, 2970));
        view.zoom = 1.25;
        assert_eq!(view.raster_size(3., 4096), (2625, 3713));
        view.zoom = 2.;
        assert_eq!(view.raster_size(3., 4096), (2896, 4096));
        view.image = [2000., 1000.];
        view.size = [1000., 700.];
        view.zoom = 1.;
        assert_eq!(view.raster_size(3., 4096), (3000, 1500));
    }
    #[test]
    fn pinch_preserves_anchor() {
        let mut view = Viewport {
            image: [2000., 1000.],
            size: [800., 400.],
            ..Default::default()
        };
        let anchor = [600., 250.];
        let pixel = [
            (anchor[0] - 400.) / view.scale(),
            (anchor[1] - 200.) / view.scale(),
        ];
        view.zoom_to(2., anchor);
        assert!(((anchor[0] - 400. - view.offset[0]) / view.scale() - pixel[0]).abs() < 1e-9);
        assert!(((anchor[1] - 200. - view.offset[1]) / view.scale() - pixel[1]).abs() < 1e-9);
    }
    #[test]
    fn pan_never_exposes_empty_edges() {
        let mut v = Viewport {
            image: [2000., 1000.],
            size: [800., 400.],
            ..Default::default()
        };
        v.zoom_to(2., [400., 200.]);
        v.pan_to([10000., -10000.]);
        assert_eq!(v.offset, [400., -200.]);
        v.reset();
        assert_eq!(v.offset, [0., 0.]);
        assert_eq!(v.bounds(), [0., 0., 800., 400.]);
    }
}
