use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

use super::{
    AliasBinding, AliasScheme, AuthorityError, NormalizedAlias, ProviderAccountAlias,
    ProviderAccountId, NORMALIZATION_VERSION,
};

pub fn normalize_alias(alias: ProviderAccountAlias) -> Result<NormalizedAlias, AuthorityError> {
    let (scheme, raw) = match alias {
        ProviderAccountAlias::OpenAiAccountUuid(v) => (AliasScheme::OpenAiAccountUuid, v),
        ProviderAccountAlias::OpenAiOrgId(v) => (AliasScheme::OpenAiOrgId, v),
        ProviderAccountAlias::CodexBarAccountKey(v) => (AliasScheme::CodexBarAccountKey, v),
        ProviderAccountAlias::CodexMultiAuthAccountId(v) => {
            (AliasScheme::CodexMultiAuthAccountId, v)
        }
        ProviderAccountAlias::Email(v) => (AliasScheme::Email, v),
        ProviderAccountAlias::Label(v) => (AliasScheme::Label, v),
    };
    if raw.len() > 1024 || raw.contains('\0') {
        return Err(AuthorityError::InvalidAlias("alias exceeds bounds".into()));
    }
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(AuthorityError::InvalidAlias("alias is empty".into()));
    }
    let (value, display) = match scheme {
        AliasScheme::OpenAiAccountUuid => {
            let value = normalize_uuid(trimmed)?;
            let display = format!("{}…{}", &value[..4], &value[value.len() - 4..]);
            (value, display)
        }
        AliasScheme::OpenAiOrgId => {
            if !(trimmed.starts_with("org-") || trimmed.starts_with("org_")) || trimmed.len() < 5 {
                return Err(AuthorityError::InvalidAlias("invalid OpenAI org id".into()));
            }
            (trimmed.into(), mask(trimmed))
        }
        AliasScheme::CodexBarAccountKey => {
            let parts: Vec<_> = trimmed.split(':').collect();
            if parts.len() < 4
                || parts[0] != "codex"
                || parts[1] != "v1"
                || parts.iter().any(|p| p.is_empty())
            {
                return Err(AuthorityError::InvalidAlias(
                    "invalid CodexBar account key".into(),
                ));
            }
            (
                trimmed.into(),
                format!("codex:v1:***:{}", mask(parts[parts.len() - 1])),
            )
        }
        AliasScheme::CodexMultiAuthAccountId => (trimmed.into(), mask(trimmed)),
        AliasScheme::Email => {
            if trimmed.len() > 320 || !trimmed.contains('@') {
                return Err(AuthorityError::InvalidAlias("invalid email hint".into()));
            }
            let normalized: String = trimmed.nfc().collect::<String>().to_lowercase();
            let digest = hex_digest(normalized.as_bytes());
            let local = normalized.split('@').next().unwrap_or("");
            (
                format!("email_sha256:{}", digest),
                format!("{}***@***", local.chars().next().unwrap_or('*')),
            )
        }
        AliasScheme::Label => {
            if trimmed.len() > 128
                || trimmed == "."
                || trimmed == ".."
                || trimmed.contains('/')
                || trimmed.contains('\\')
            {
                return Err(AuthorityError::InvalidAlias("invalid label".into()));
            }
            let nfc: String = trimmed.nfc().collect();
            (nfc.clone(), nfc)
        }
    };
    Ok(NormalizedAlias {
        scheme,
        value,
        display,
        rank: scheme.rank(),
    })
}

pub fn binding_id(binding: &AliasBinding) -> String {
    let material = format!(
        "{}\0{}\0{}\0{}",
        binding.source.as_str(),
        binding.source_record_key,
        binding.scheme.as_str(),
        binding.normalized_value
    );
    format!("pab_{}", &hex_digest(material.as_bytes())[..24])
}

pub fn normalize_uuid(value: &str) -> Result<String, AuthorityError> {
    let value = value.to_ascii_lowercase();
    let bytes = value.as_bytes();
    if bytes.len() != 36
        || bytes[8] != b'-'
        || bytes[13] != b'-'
        || bytes[18] != b'-'
        || bytes[23] != b'-'
        || bytes
            .iter()
            .enumerate()
            .any(|(i, b)| !matches!(i, 8 | 13 | 18 | 23) && !b.is_ascii_hexdigit())
    {
        return Err(AuthorityError::InvalidAlias("invalid UUID".into()));
    }
    Ok(value)
}

pub fn deterministic_id(provider: &str, anchor: &str) -> ProviderAccountId {
    let mut hasher = Sha256::new();
    hasher.update(b"switchback-provider-account\0");
    hasher.update(provider.as_bytes());
    hasher.update(b"\0");
    hasher.update(NORMALIZATION_VERSION.as_bytes());
    hasher.update(b"\0");
    hasher.update(anchor.as_bytes());
    let encoded = base32_lower(&hasher.finalize());
    ProviderAccountId(format!("pa_{}", &encoded[..26]))
}

pub fn hex_digest(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

fn base32_lower(bytes: &[u8]) -> String {
    const A: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut out = String::new();
    let mut buffer = 0u32;
    let mut bits = 0u8;
    for byte in bytes {
        buffer = (buffer << 8) | u32::from(*byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(A[((buffer >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(A[((buffer << (5 - bits)) & 31) as usize] as char);
    }
    out
}
fn mask(value: &str) -> String {
    let head: String = value.chars().take(4).collect();
    format!("{head}…")
}
