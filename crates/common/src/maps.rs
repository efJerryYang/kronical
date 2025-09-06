#[cfg(feature = "fast-hash")]
pub use hashbrown::{HashMap, HashSet};

#[cfg(not(feature = "fast-hash"))]
pub use std::collections::{HashMap, HashSet};
