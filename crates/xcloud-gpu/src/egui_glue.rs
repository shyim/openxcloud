//! Showing the renderer's output in egui. Replaces the `QSGSimpleTextureNode` /
//! `QNativeInterface::QSGMetalTexture::fromNative` part of `MetalVideoItem::updatePaintNode`
//! and its letterboxing (`QSizeF::scaled(..., Qt::KeepAspectRatio)` centred in the item).
//!
//! The output view is registered once per output texture with
//! `egui_wgpu::Renderer::register_native_texture` and re-pointed with
//! `update_egui_texture_from_wgpu_texture` whenever the renderer recreates the texture
//! (size change), tracked through [`crate::VideoRenderer::output_generation`].

use egui::{Color32, Rect, TextureId, Vec2, pos2};

/// An egui texture id bound to the video output view.
#[derive(Debug, Default)]
pub struct VideoTexture {
    id: Option<TextureId>,
    generation: Option<u64>,
}

impl VideoTexture {
    /// Nothing registered yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// The registered id, if any.
    pub fn id(&self) -> Option<TextureId> {
        self.id
    }

    /// Registers `view` on first use and re-binds it when `generation` changed. Returns the
    /// id to draw with. Linear filtering, like `QSGTexture::Linear` in the Qt item.
    pub fn sync(
        &mut self,
        renderer: &mut egui_wgpu::Renderer,
        device: &wgpu::Device,
        view: &wgpu::TextureView,
        generation: u64,
    ) -> TextureId {
        match self.id {
            Some(id) if self.generation == Some(generation) => id,
            Some(id) => {
                renderer.update_egui_texture_from_wgpu_texture(
                    device,
                    view,
                    wgpu::FilterMode::Linear,
                    id,
                );
                self.generation = Some(generation);
                id
            }
            None => {
                let id = renderer.register_native_texture(device, view, wgpu::FilterMode::Linear);
                self.id = Some(id);
                self.generation = Some(generation);
                id
            }
        }
    }

    /// Frees the egui-side binding (the wgpu texture itself belongs to the renderer).
    pub fn free(&mut self, renderer: &mut egui_wgpu::Renderer) {
        if let Some(id) = self.id.take() {
            renderer.free_texture(&id);
        }
        self.generation = None;
    }

    /// Draws the video into `available`, letterboxed to `(width, height)`'s aspect ratio.
    /// Returns the rectangle actually covered. No-op (returns an empty rect at the centre)
    /// until something is registered.
    pub fn draw(&self, painter: &egui::Painter, available: Rect, width: u32, height: u32) -> Rect {
        let rect = letterbox(available, width, height);
        if let Some(id) = self.id {
            painter.image(
                id,
                rect,
                Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                Color32::WHITE,
            );
        }
        rect
    }
}

/// Fits a `width`×`height` image into `available` keeping its aspect ratio, centred:
/// `QSizeF(outputSize).scaled(boundingRect().size(), Qt::KeepAspectRatio)` placed at
/// `((w - fitted.w) / 2, (h - fitted.h) / 2)`.
pub fn letterbox(available: Rect, width: u32, height: u32) -> Rect {
    if width == 0 || height == 0 || available.width() <= 0.0 || available.height() <= 0.0 {
        return Rect::from_center_size(available.center(), Vec2::ZERO);
    }
    let (w, h) = (width as f32, height as f32);
    let scale = (available.width() / w).min(available.height() / h);
    let size = Vec2::new(w * scale, h * scale);
    Rect::from_center_size(available.center(), size)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: Rect, b: Rect) -> bool {
        (a.min - b.min).length() < 1e-3 && (a.max - b.max).length() < 1e-3
    }

    #[test]
    fn letterbox_pillarboxes_a_wide_video_in_a_tall_area() {
        let area = Rect::from_min_max(pos2(0.0, 0.0), pos2(1000.0, 1000.0));
        let fitted = letterbox(area, 3840, 2160);
        assert!(
            approx(
                fitted,
                Rect::from_min_max(pos2(0.0, 218.75), pos2(1000.0, 781.25))
            ),
            "{fitted:?}"
        );
    }

    #[test]
    fn letterbox_fills_an_area_with_the_same_aspect() {
        let area = Rect::from_min_max(pos2(10.0, 20.0), pos2(1930.0, 1100.0));
        let fitted = letterbox(area, 1920, 1080);
        assert!(approx(fitted, area), "{fitted:?}");
    }

    #[test]
    fn letterbox_handles_degenerate_sizes() {
        let area = Rect::from_min_max(pos2(0.0, 0.0), pos2(100.0, 50.0));
        assert_eq!(letterbox(area, 0, 1080).size(), Vec2::ZERO);
        assert_eq!(letterbox(area, 1920, 0).size(), Vec2::ZERO);
        let empty = Rect::from_min_max(pos2(5.0, 5.0), pos2(5.0, 5.0));
        assert_eq!(letterbox(empty, 1920, 1080).size(), Vec2::ZERO);
    }

    #[test]
    fn video_texture_starts_unregistered() {
        let t = VideoTexture::new();
        assert!(t.id().is_none());
    }
}
