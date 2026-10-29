//! Error type for `xcloud-gpu`. The Qt `MetalVideoItem` reported failures with a bare
//! `return false` and fell back to CPU images; here every failure says what broke so the
//! renderer can decide between "fall back to the CPU upload path" and "give up".

/// Everything that can go wrong on the GPU video path.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The zero-copy import cannot work on this device/backend (not Metal, no hal access,
    /// texture cache creation failed). The caller should use the CPU upload path.
    #[error("zero-copy frame import unavailable: {0}")]
    ZeroCopyUnavailable(&'static str),

    /// A CoreVideo call failed with a `CVReturn` we do not map more precisely.
    #[error("{what} failed with CVReturn {status}")]
    CoreVideo { what: &'static str, status: i32 },

    /// The frame's pixel buffer is not something the shaders understand (not NV12, or
    /// a plane is shorter than its stride and height imply).
    #[error("unsupported frame buffer: {0}")]
    UnsupportedBuffer(&'static str),

    /// Reading the pixels back to CPU memory failed (`Frame::to_nv12_cpu`).
    #[error(transparent)]
    Media(#[from] xcloud_media::Error),

    /// A frame with zero width or height; nothing can be rendered.
    #[error("frame has an empty size {width}x{height}")]
    EmptyFrame { width: u32, height: u32 },

    /// The MetalFX spatial scaler could not be created or encoded. The renderer never
    /// surfaces this itself: it logs the reason and falls back to FSR 1.0.
    #[error("MetalFX: {0}")]
    MetalFx(#[from] MetalFxError),
}

/// Why a MetalFX scaler could not be created or encoded (`crate::metalfx`). Kept as its
/// own enum so the scaler's `Result`s stay precise and comparable in tests; it wraps into
/// [`Error::MetalFx`].
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum MetalFxError {
    /// Built without `--features metalfx`, or not on macOS.
    #[error("MetalFX is not compiled in (build xcloud-gpu with `--features metalfx` on macOS)")]
    NotCompiled,
    /// wgpu is not running on the Metal backend, so there is no `MTLDevice`.
    #[error("wgpu is not running on the Metal backend")]
    NotMetal,
    /// `[MTLFXSpatialScalerDescriptor supportsDevice:]` returned `NO`.
    #[error("MTLFXSpatialScalerDescriptor does not support this MTLDevice")]
    Unsupported,
    /// `newSpatialScalerWithDevice:` returned `nil` for these sizes/formats.
    #[error(
        "MTLFXSpatialScaler creation failed for {input_width}x{input_height} → {output_width}x{output_height}"
    )]
    CreateFailed {
        input_width: u32,
        input_height: u32,
        output_width: u32,
        output_height: u32,
    },
    /// A texture does not carry the usage bits MetalFX requires of it.
    #[error("{texture} texture usage {have:#x} lacks MetalFX's required {need:#x}")]
    Usage {
        texture: &'static str,
        have: usize,
        need: usize,
    },
    /// A texture is not in the format the scaler was created for.
    #[error("{texture} texture is MTLPixelFormat {have}, scaler expects {need}")]
    Format {
        texture: &'static str,
        have: usize,
        need: usize,
    },
    /// `[MTLCommandQueue commandBuffer]` returned `nil`.
    #[error("wgpu's MTLCommandQueue returned no command buffer")]
    NoCommandBuffer,
}

/// Crate-wide result alias.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metalfx_error_wraps_into_the_crate_error() {
        let e: Error = MetalFxError::NotCompiled.into();
        assert!(matches!(e, Error::MetalFx(MetalFxError::NotCompiled)));
        assert!(e.to_string().starts_with("MetalFX: "));
        assert!(e.to_string().contains("--features metalfx"));
    }
}
