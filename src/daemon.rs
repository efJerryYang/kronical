pub mod compressor;
pub mod coordinator;
pub mod events;
pub mod records;
pub mod snapshot;

// Grouped subsystems
pub mod server; // gRPC + HTTP
pub mod tracker; // focus + system tracker

// Unified API facade around available transports (gRPC, HTTP/SSE)
pub mod api;
