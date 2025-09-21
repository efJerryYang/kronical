pub mod compressor;
pub mod coordinator;
pub mod pipeline;
// Re-export core modules to maintain stable paths
pub use kronical_core::events;
pub use kronical_core::records;
pub use kronical_core::snapshot;

// Grouped subsystems
pub mod server; // gRPC + HTTP
pub mod tracker; // focus + system tracker

// Unified API facade around available transports (gRPC, HTTP/SSE)
pub mod api;
