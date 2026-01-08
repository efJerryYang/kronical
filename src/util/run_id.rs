use anyhow::{Context, Result};
use base64::Engine;
use base64::alphabet::Alphabet;
use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig};
use once_cell::sync::Lazy;
use uuid::Uuid;

const RUN_ID_LEN_BYTES: usize = 16;
const RUN_ID_ALPHABET: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789._";

static RUN_ID_ENGINE: Lazy<GeneralPurpose> = Lazy::new(|| {
    let alphabet = Alphabet::new(RUN_ID_ALPHABET).expect("valid run id alphabet");
    GeneralPurpose::new(
        &alphabet,
        GeneralPurposeConfig::new().with_encode_padding(false),
    )
});

pub fn encode_uuid(uuid: Uuid) -> String {
    RUN_ID_ENGINE.encode(uuid.as_bytes())
}

pub fn parse_run_id(input: &str) -> Result<String> {
    let decoded = RUN_ID_ENGINE.decode(input).or_else(|_| {
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(input)
            .with_context(|| format!("invalid base64 run id '{input}'"))
    })?;
    if decoded.len() != RUN_ID_LEN_BYTES {
        return Err(anyhow::anyhow!(
            "invalid run id length: expected {} bytes, got {}",
            RUN_ID_LEN_BYTES,
            decoded.len()
        ));
    }
    Ok(RUN_ID_ENGINE.encode(decoded))
}
