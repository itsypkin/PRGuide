//! Diagram rendering behind a private trait (§2.8).

pub mod limits;
pub mod mmdflux;

pub use self::limits::Limit;
pub use self::mmdflux::{MmdfluxRenderer, rewrite_root_direction};

/// The renderer version mixed into cache keys. Must match the exact mmdflux pin.
pub const RENDERER_VERSION: &str = "mmdflux-2.6.1";

/// Which glyph set to draw with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Charset {
    Unicode,
    Ascii,
}

/// The only root-direction override v1 needs: LR/RL → TD (§2.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RootDirection {
    TopDown,
}

/// Options for one render call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderOpts {
    pub charset: Charset,
    pub root_direction: Option<RootDirection>,
}

/// A rendered diagram as lines of terminal text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextDiagram {
    pub lines: Vec<String>,
    /// Width in terminal cells.
    pub width: usize,
    /// Always `lines.len()`.
    pub height: usize,
    /// True when the root direction was rewritten.
    pub direction_overridden: bool,
}

/// Turns Mermaid source into terminal text.
pub trait Renderer {
    fn render(&self, src: &str, opts: &RenderOpts) -> Result<TextDiagram, RenderError>;
}

/// Why a diagram could not be rendered.
#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    #[error("unsupported diagram type: {0}")]
    Unsupported(String),
    #[error("invalid diagram: {0}")]
    Invalid(String),
    #[error("diagram too large: {actual} {} (max {max})", limit.as_str())]
    TooLarge {
        limit: Limit,
        actual: usize,
        max: usize,
    },
    #[error("renderer panicked: {0}")]
    Panic(String),
}

impl RenderError {
    /// Stable machine-readable code; never rename.
    pub fn code(&self) -> &'static str {
        match self {
            RenderError::Unsupported(_) => "render_unsupported",
            RenderError::Invalid(_) => "render_invalid",
            RenderError::TooLarge { .. } => "render_too_large",
            RenderError::Panic(_) => "render_panic",
        }
    }
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

fn fnv(hash: &mut u64, bytes: &[u8]) {
    for b in bytes {
        *hash ^= u64::from(*b);
        *hash = hash.wrapping_mul(FNV_PRIME);
    }
}

/// FNV-1a 64 key for a viewport's final result.
///
/// It deliberately has no root direction: the viewer caches the outcome after
/// its fit/flip decision, so the key identifies that final result.
pub fn render_cache_key(src: &str, width: u16, height: u16, charset: Charset) -> u64 {
    let mut h = FNV_OFFSET;
    fnv(&mut h, RENDERER_VERSION.as_bytes());
    fnv(&mut h, &[0]);
    fnv(&mut h, &(src.len() as u64).to_le_bytes());
    fnv(&mut h, src.as_bytes());
    fnv(&mut h, &width.to_le_bytes());
    fnv(&mut h, &height.to_le_bytes());
    fnv(
        &mut h,
        &[match charset {
            Charset::Unicode => 0,
            Charset::Ascii => 1,
        }],
    );
    h
}
