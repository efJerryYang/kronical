use anyhow::{Context, Result};
use base64::Engine;
use uuid::Uuid;

const RUN_ID_LEN_BYTES: usize = 16;

pub fn encode_uuid(uuid: Uuid) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(uuid.as_bytes())
}

pub fn parse_run_id(input: &str) -> Result<String> {
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(input)
        .with_context(|| format!("invalid base64 run id '{input}'"))?;
    if decoded.len() != RUN_ID_LEN_BYTES {
        return Err(anyhow::anyhow!(
            "invalid run id length: expected {} bytes, got {}",
            RUN_ID_LEN_BYTES,
            decoded.len()
        ));
    }
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(decoded))
}
