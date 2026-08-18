mod leb;
mod view;

pub use leb::{Cursor, sleb128, uleb128};
pub use view::View;

pub type Word = u64;
