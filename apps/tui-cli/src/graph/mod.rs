//! The code graph: what is defined where, and what refers to it.
//!
//! Two layers. `extract` turns source text into definitions and references with
//! tree-sitter, and knows nothing about storage. `store` keeps them in SQLite
//! and answers the questions a reviewer has — what encloses this line, what does
//! this name refer to, and above all **what calls this**, which the regex index
//! it replaces could never answer at all.

pub mod extract;
pub mod link;
pub mod store;

pub use link::link_related;
pub use store::{Def, Graph};
