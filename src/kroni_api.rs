#[cfg(feature = "kroni-api")]
pub mod kroni {
    pub mod v1 {
        tonic::include_proto!("kroni.v1");
    }
}
