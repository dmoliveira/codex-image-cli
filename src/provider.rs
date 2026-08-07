use serde::Serialize;
use std::env;

use crate::{
    cli::{Background, GenerateArgs, Moderation, OutputFormat, Provider},
    report::AppError,
    MODEL,
};

#[derive(Debug, Serialize)]
pub struct Capability {
    pub provider: &'static str,
    pub supported: Vec<&'static str>,
    pub best_effort: Vec<&'static str>,
    pub unsupported: Vec<&'static str>,
}

pub fn capabilities() -> Vec<Capability> {
    vec![
        Capability {
            provider: Provider::Codex.as_str(),
            supported: vec!["count=1", "format=png"],
            best_effort: vec!["size", "quality"],
            unsupported: vec!["compression", "background!=auto", "moderation!=auto"],
        },
        Capability {
            provider: Provider::Api.as_str(),
            supported: vec![
                "count=1..4",
                "format=png|jpeg|webp",
                "size",
                "quality",
                "background",
                "compression",
                "moderation",
            ],
            best_effort: Vec::new(),
            unsupported: Vec::new(),
        },
    ]
}

pub fn model(provider: Provider) -> Option<&'static str> {
    match provider {
        Provider::Api => Some(MODEL),
        Provider::Codex => None,
    }
}

pub fn executable() -> String {
    env::var("CODEX_CLI_PATH").unwrap_or_else(|_| "codex".to_owned())
}

pub fn validate(args: &GenerateArgs) -> Result<(), AppError> {
    if args.provider == Provider::Codex
        && (args.n != 1
            || args.format != OutputFormat::Png
            || args.compression.is_some()
            || args.background != Background::Auto
            || args.moderation != Moderation::Auto)
    {
        return Err(AppError::usage(
            "codex_provider_constraints",
            "The Codex subscription provider supports one PNG per command; size and quality are best-effort. Use --provider api for other formats, counts, compression, background, or moderation settings.",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_match_provider_models() {
        assert_eq!(model(Provider::Api), Some(MODEL));
        assert_eq!(model(Provider::Codex), None);
        assert_eq!(capabilities().len(), 2);
    }
}
