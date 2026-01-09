use anyhow::{Context, Result};
use base64::Engine;
use base64::alphabet::{Alphabet, STANDARD as ALPHABET_STANDARD};
use base64::engine::general_purpose::{
    GeneralPurpose, GeneralPurposeConfig, STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD,
};
use once_cell::sync::Lazy;
use uuid::Uuid;

const RUN_ID_LEN_BYTES: usize = 16;
const RUN_ID_ALPHABET_LEGACY: &str =
    "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789._";

static RUN_ID_ENGINE: Lazy<GeneralPurpose> = Lazy::new(|| {
    GeneralPurpose::new(
        &ALPHABET_STANDARD,
        GeneralPurposeConfig::new().with_encode_padding(false),
    )
});

static RUN_ID_LEGACY_ENGINE: Lazy<GeneralPurpose> = Lazy::new(|| {
    let alphabet = Alphabet::new(RUN_ID_ALPHABET_LEGACY).expect("valid legacy run id alphabet");
    GeneralPurpose::new(
        &alphabet,
        GeneralPurposeConfig::new().with_encode_padding(false),
    )
});

static RUN_ID_LEGACY_ENGINE_PADDED: Lazy<GeneralPurpose> = Lazy::new(|| {
    let alphabet = Alphabet::new(RUN_ID_ALPHABET_LEGACY).expect("valid legacy run id alphabet");
    GeneralPurpose::new(
        &alphabet,
        GeneralPurposeConfig::new().with_encode_padding(true),
    )
});

fn decode_run_id(input: &str) -> Result<Vec<u8>> {
    if let Ok(decoded) = RUN_ID_ENGINE.decode(input) {
        return Ok(decoded);
    }
    if let Ok(decoded) = RUN_ID_LEGACY_ENGINE.decode(input) {
        return Ok(decoded);
    }
    if let Ok(decoded) = STANDARD_NO_PAD.decode(input) {
        return Ok(decoded);
    }
    if let Ok(decoded) = URL_SAFE_NO_PAD.decode(input) {
        return Ok(decoded);
    }
    if let Ok(decoded) = RUN_ID_LEGACY_ENGINE_PADDED.decode(input) {
        return Ok(decoded);
    }
    if let Ok(decoded) = STANDARD.decode(input) {
        return Ok(decoded);
    }
    if let Ok(decoded) = URL_SAFE.decode(input) {
        return Ok(decoded);
    }
    let mut padded = input.to_string();
    let rem = padded.len() % 4;
    if rem != 0 {
        padded.extend(std::iter::repeat('=').take(4 - rem));
    }
    if let Ok(decoded) = RUN_ID_LEGACY_ENGINE_PADDED.decode(&padded) {
        return Ok(decoded);
    }
    if let Ok(decoded) = STANDARD.decode(&padded) {
        return Ok(decoded);
    }
    if let Ok(decoded) = URL_SAFE.decode(&padded) {
        return Ok(decoded);
    }
    Err(anyhow::anyhow!("invalid base64 run id '{input}'"))
}

pub fn encode_uuid(uuid: Uuid) -> String {
    RUN_ID_ENGINE.encode(uuid.as_bytes())
}

pub fn parse_run_id(input: &str) -> Result<String> {
    let decoded =
        decode_run_id(input).with_context(|| format!("invalid base64 run id '{input}'"))?;
    if decoded.len() != RUN_ID_LEN_BYTES {
        return Err(anyhow::anyhow!(
            "invalid run id length: expected {} bytes, got {}",
            RUN_ID_LEN_BYTES,
            decoded.len()
        ));
    }
    Ok(RUN_ID_ENGINE.encode(decoded))
}
