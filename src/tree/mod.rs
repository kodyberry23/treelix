//! The file-tree model and operations.

pub mod model;
pub mod node;
pub mod ops;

pub use model::{Row, SortMode, Tree, ViewOptions};
pub use node::NodeKind;
