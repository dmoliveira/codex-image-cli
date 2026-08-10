use std::{net::IpAddr, str::FromStr};

use url::{Origin, Url};

use crate::report::AppError;

#[derive(Debug, Clone)]
pub struct Endpoint {
    base: Url,
    url: Url,
}

impl Endpoint {
    pub fn authorize(
        input: &str,
        dangerous_origin: Option<&str>,
        allow_insecure_localhost: bool,
    ) -> Result<Self, AppError> {
        let mut base = Url::parse(input).map_err(|_| {
            AppError::usage(
                "invalid_api_base_url",
                "--api-base-url must be an absolute URL without embedded credentials.",
            )
        })?;
        validate_base_url(&base)?;

        let is_default_openai = is_default_openai(&base);
        let is_loopback = is_loopback(&base);

        match base.scheme() {
            "http" if is_loopback && allow_insecure_localhost => {}
            "http" if is_loopback => {
                return Err(AppError::usage(
                    "insecure_localhost_not_confirmed",
                    "Loopback HTTP requires --allow-insecure-localhost. This override is intended only for local testing.",
                ));
            }
            "http" => {
                return Err(AppError::usage(
                    "insecure_non_loopback_endpoint",
                    "HTTP endpoints are allowed only for explicit loopback tests; use HTTPS for all other endpoints.",
                ));
            }
            "https" if is_default_openai || is_loopback => {}
            "https" => {
                let actual_origin = origin_string(&base)?;
                let approved_origin = dangerous_origin.ok_or_else(|| {
                    AppError::usage(
                        "custom_endpoint_not_approved",
                        "A custom HTTPS endpoint needs --dangerously-allow-api-key-to with its exact origin before OPENAI_API_KEY can be sent.",
                    )
                })?;
                if normalize_approved_origin(approved_origin)? != actual_origin {
                    return Err(AppError::usage(
                        "custom_endpoint_origin_mismatch",
                        "--dangerously-allow-api-key-to must exactly match the custom endpoint origin, including its effective port.",
                    ));
                }
            }
            _ => {
                return Err(AppError::usage(
                    "unsupported_endpoint_scheme",
                    "--api-base-url must use HTTPS, or explicit loopback HTTP for tests.",
                ));
            }
        }

        let normalized_path = if base.path().ends_with('/') {
            base.path().to_owned()
        } else {
            format!("{}/", base.path())
        };
        base.set_path(&normalized_path);
        let url = base.join("images/generations").map_err(|_| {
            AppError::usage(
                "invalid_api_base_url",
                "--api-base-url could not be joined with the image generation endpoint.",
            )
        })?;
        Ok(Self { base, url })
    }

    pub fn url(&self) -> &Url {
        &self.url
    }

    /// OpenAI's published pricing is only applied to the canonical API
    /// origin. Compatible loopback/custom endpoints are still recorded but
    /// remain explicitly unpriced.
    pub fn is_canonical_openai(&self) -> bool {
        is_default_openai(&self.base)
    }

    pub fn files_url(&self) -> Result<Url, AppError> {
        self.url_for("files")
    }

    pub fn batches_url(&self) -> Result<Url, AppError> {
        self.url_for("batches")
    }

    pub fn batch_url(&self, batch_id: &str) -> Result<Url, AppError> {
        validate_remote_id(batch_id, "batch_id")?;
        self.url_for(&format!("batches/{batch_id}"))
    }

    pub fn batch_cancel_url(&self, batch_id: &str) -> Result<Url, AppError> {
        validate_remote_id(batch_id, "batch_id")?;
        self.url_for(&format!("batches/{batch_id}/cancel"))
    }

    pub fn file_content_url(&self, file_id: &str) -> Result<Url, AppError> {
        validate_remote_id(file_id, "file_id")?;
        self.url_for(&format!("files/{file_id}/content"))
    }

    pub fn file_url(&self, file_id: &str) -> Result<Url, AppError> {
        validate_remote_id(file_id, "file_id")?;
        self.url_for(&format!("files/{file_id}"))
    }

    fn url_for(&self, path: &str) -> Result<Url, AppError> {
        self.base.join(path).map_err(|_| {
            AppError::usage(
                "invalid_api_base_url",
                "The approved API base URL could not be joined with the requested endpoint.",
            )
        })
    }
}

pub fn validate_remote_id(value: &str, name: &str) -> Result<(), AppError> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
    {
        return Err(AppError::usage(
            "invalid_remote_id",
            format!("{name} must be 1-128 ASCII letters, digits, '_' or '-'."),
        ));
    }
    Ok(())
}

fn validate_base_url(url: &Url) -> Result<(), AppError> {
    if url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(AppError::usage(
            "unsafe_api_base_url",
            "--api-base-url cannot contain credentials, a query, or a fragment and must include a host.",
        ));
    }
    Ok(())
}

fn is_default_openai(url: &Url) -> bool {
    url.scheme() == "https"
        && url.host_str() == Some("api.openai.com")
        && url.port_or_known_default() == Some(443)
        && matches!(url.path(), "/v1" | "/v1/")
}

fn is_loopback(url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    IpAddr::from_str(host).is_ok_and(|address| address.is_loopback())
}

fn origin_string(url: &Url) -> Result<String, AppError> {
    match url.origin() {
        Origin::Tuple(_, _, _) => Ok(url.origin().ascii_serialization()),
        Origin::Opaque(_) => Err(AppError::usage(
            "opaque_api_origin",
            "--api-base-url must have a normal HTTP(S) origin.",
        )),
    }
}

fn normalize_approved_origin(input: &str) -> Result<String, AppError> {
    let url = Url::parse(input).map_err(|_| {
        AppError::usage(
            "invalid_approved_origin",
            "--dangerously-allow-api-key-to must be an HTTPS origin URL.",
        )
    })?;
    validate_base_url(&url)?;
    if url.scheme() != "https" || !matches!(url.path(), "" | "/") {
        return Err(AppError::usage(
            "invalid_approved_origin",
            "--dangerously-allow-api-key-to must be an HTTPS origin without a path, query, fragment, or credentials.",
        ));
    }
    origin_string(&url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_default_openai_endpoint() {
        let endpoint = Endpoint::authorize("https://api.openai.com/v1", None, false).unwrap();
        assert_eq!(
            endpoint.url().as_str(),
            "https://api.openai.com/v1/images/generations"
        );
        assert_eq!(
            endpoint.files_url().unwrap().as_str(),
            "https://api.openai.com/v1/files"
        );
        assert_eq!(
            endpoint.batch_url("batch_123").unwrap().as_str(),
            "https://api.openai.com/v1/batches/batch_123"
        );
    }

    #[test]
    fn requires_explicit_loopback_http_confirmation() {
        assert!(Endpoint::authorize("http://127.0.0.1:8080/v1", None, false).is_err());
        assert!(Endpoint::authorize("http://127.0.0.1:8080/v1", None, true).is_ok());
    }

    #[test]
    fn custom_https_requires_exact_origin_approval() {
        assert!(Endpoint::authorize("https://gateway.example/v1", None, false).is_err());
        assert!(Endpoint::authorize(
            "https://gateway.example/v1",
            Some("https://gateway.example"),
            false
        )
        .is_ok());
        assert!(Endpoint::authorize(
            "https://gateway.example/v1",
            Some("https://other.example"),
            false
        )
        .is_err());
    }

    #[test]
    fn rejects_unsafe_or_unapproved_urls_before_any_request() {
        for url in [
            "http://images.example/v1",
            "https://user:pass@api.openai.com/v1",
            "https://api.openai.com/v1?redirect=true",
            "https://api.openai.com/v1#fragment",
        ] {
            assert!(Endpoint::authorize(url, None, false).is_err(), "{url}");
        }
    }

    #[test]
    fn rejects_remote_ids_that_could_escape_the_allowlisted_path() {
        assert!(
            Endpoint::authorize("https://api.openai.com/v1", None, false)
                .unwrap()
                .batch_url("../files")
                .is_err()
        );
        assert!(
            Endpoint::authorize("https://api.openai.com/v1", None, false)
                .unwrap()
                .file_content_url("file/id")
                .is_err()
        );
    }
}
