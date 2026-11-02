use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header, jwk::JwkSet};
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use url::Url;

use crate::app::AppIdentity;

const METADATA_URL: &str = "https://login.eveonline.com/.well-known/oauth-authorization-server";
const EXPECTED_ISSUERS: [&str; 3] = [
    "https://login.eveonline.com/",
    "https://login.eveonline.com",
    "login.eveonline.com",
];

#[derive(Debug, Clone, Deserialize)]
pub struct SsoMetadata {
    pub issuer: String,
    pub authorization_endpoint: Url,
    pub token_endpoint: Url,
    pub jwks_uri: Url,
}

#[derive(Debug, Clone, Eq, PartialEq, serde::Serialize)]
pub struct CharacterIdentity {
    pub id: u64,
    pub name: String,
    pub scopes: Vec<String>,
}

pub struct ValidatedAccessToken {
    pub secret: SecretString,
    pub expires_at: DateTime<Utc>,
    pub character: CharacterIdentity,
}

pub struct TokenBundle {
    pub access: ValidatedAccessToken,
    pub refresh: SecretString,
}

pub struct AuthorizationRequest {
    pub url: Url,
    pub state: SecretString,
    pub verifier: SecretString,
}

#[derive(Clone)]
pub struct SsoClient {
    http: reqwest::Client,
    metadata_url: Url,
    /// Shared by every clone, so one character finding EVE SSO down spares
    /// the others the same timeout. The daemon awaits ESI on its main loop,
    /// and six characters each waiting out a gateway timeout froze the
    /// mining meter and the previews for minutes at a time.
    outage: Arc<Mutex<Outage>>,
    /// Scopes every token this client accepts must carry.
    required_scopes: &'static [&'static str],
}

#[derive(Default)]
struct Outage {
    until: Option<Instant>,
    backoff: Duration,
}

const OUTAGE_FIRST_BACKOFF: Duration = Duration::from_secs(30);
const OUTAGE_MAX_BACKOFF: Duration = Duration::from_secs(600);

#[derive(Deserialize)]
struct RawTokenResponse {
    access_token: String,
    token_type: String,
    refresh_token: Option<String>,
}

#[derive(Deserialize)]
struct TokenClaims {
    aud: Vec<String>,
    exp: i64,
    iss: String,
    name: String,
    scp: Vec<String>,
    sub: String,
}

#[derive(Deserialize)]
struct OAuthError {
    error: Option<String>,
    error_description: Option<String>,
}

impl SsoClient {
    pub fn new(app: AppIdentity) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(app.user_agent())
            // A token answer is small and quick when SSO is well; anything
            // slower than this is an outage, and the wait blocks the daemon.
            .timeout(Duration::from_secs(10))
            .build()
            .context("could not build the EVE SSO HTTP client")?;
        Ok(Self {
            http,
            metadata_url: Url::parse(METADATA_URL).expect("the SSO metadata URL must be valid"),
            outage: Arc::default(),
            required_scopes: app.required_scopes,
        })
    }

    #[cfg(test)]
    fn with_metadata_url(metadata_url: Url) -> Result<Self> {
        let mut client = Self::new(crate::app::TEST_APP)?;
        client.metadata_url = metadata_url;
        Ok(client)
    }

    pub async fn metadata(&self) -> Result<SsoMetadata> {
        let metadata = self
            .http
            .get(self.metadata_url.clone())
            .send()
            .await
            .context("could not reach EVE SSO metadata")?
            .error_for_status()
            .context("EVE SSO metadata returned an error")?
            .json::<SsoMetadata>()
            .await
            .context("could not parse EVE SSO metadata")?;
        metadata.validate()?;
        Ok(metadata)
    }

    /// Builds the browser authorization URL.
    ///
    /// `extra_scopes` carries anything beyond the always-required set, so a
    /// character that will read corporation industry jobs asks for that scope
    /// while every other character stays on the minimum.
    pub fn authorization_request(
        &self,
        metadata: &SsoMetadata,
        client_id: &str,
        redirect_uri: &str,
        extra_scopes: &[&str],
    ) -> Result<AuthorizationRequest> {
        let state = random_urlsafe(32)?;
        let verifier = random_urlsafe(32)?;
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let mut url = metadata.authorization_endpoint.clone();
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", client_id)
            .append_pair("redirect_uri", redirect_uri)
            .append_pair(
                "scope",
                &requested_scopes(self.required_scopes, extra_scopes).join(" "),
            )
            .append_pair("state", &state)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256");

        Ok(AuthorizationRequest {
            url,
            state: SecretString::from(state),
            verifier: SecretString::from(verifier),
        })
    }

    pub async fn exchange_code(
        &self,
        metadata: &SsoMetadata,
        client_id: &str,
        code: &SecretString,
        verifier: &SecretString,
    ) -> Result<TokenBundle> {
        let response = self
            .http
            .post(metadata.token_endpoint.clone())
            .form(&[
                ("grant_type", "authorization_code"),
                ("code", code.expose_secret()),
                ("client_id", client_id),
                ("code_verifier", verifier.expose_secret()),
            ])
            .send()
            .await
            .context("could not exchange the EVE authorization code")?;
        let raw = parse_token_response(response).await?;
        let refresh = raw
            .refresh_token
            .context("EVE SSO did not return a refresh token")?;
        let access = self
            .validate_access_token(metadata, client_id, SecretString::from(raw.access_token))
            .await?;
        validate_required_scopes(self.required_scopes, &access.character.scopes)?;

        Ok(TokenBundle {
            access,
            refresh: SecretString::from(refresh),
        })
    }

    pub async fn refresh(
        &self,
        metadata: &SsoMetadata,
        client_id: &str,
        refresh_token: &SecretString,
    ) -> Result<(ValidatedAccessToken, Option<SecretString>)> {
        if let Some(until) = self.outage.lock().expect("outage lock").until {
            let left = until.saturating_duration_since(Instant::now());
            if !left.is_zero() {
                bail!(
                    "EVE SSO is unavailable; trying again in {} seconds",
                    left.as_secs().max(1)
                );
            }
        }
        let response = self
            .http
            .post(metadata.token_endpoint.clone())
            .form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token.expose_secret()),
                ("client_id", client_id),
            ])
            .send()
            .await;
        let server_side = match &response {
            Ok(response) => response.status().is_server_error(),
            Err(_) => true,
        };
        self.record_outage(server_side);
        let response = response.context("could not refresh the EVE access token")?;
        let raw = parse_token_response(response).await?;
        let rotated = raw.refresh_token.map(SecretString::from);
        let access = self
            .validate_access_token(metadata, client_id, SecretString::from(raw.access_token))
            .await?;
        validate_required_scopes(self.required_scopes, &access.character.scopes)?;
        Ok((access, rotated))
    }

    /// Opens or widens the outage window after a failure that was SSO's, and
    /// closes it after any answer that was not.
    fn record_outage(&self, server_side: bool) {
        let mut outage = self.outage.lock().expect("outage lock");
        if server_side {
            outage.backoff = if outage.backoff.is_zero() {
                OUTAGE_FIRST_BACKOFF
            } else {
                (outage.backoff * 2).min(OUTAGE_MAX_BACKOFF)
            };
            outage.until = Some(Instant::now() + outage.backoff);
        } else {
            *outage = Outage::default();
        }
    }

    async fn validate_access_token(
        &self,
        metadata: &SsoMetadata,
        client_id: &str,
        token: SecretString,
    ) -> Result<ValidatedAccessToken> {
        let header = decode_header(token.expose_secret())
            .context("EVE SSO returned an invalid JWT header")?;
        if header.alg != Algorithm::RS256 {
            bail!("EVE SSO access token uses an unexpected signing algorithm");
        }
        let key_id = header
            .kid
            .as_deref()
            .context("EVE SSO access token has no signing key ID")?;
        let keys = self
            .http
            .get(metadata.jwks_uri.clone())
            .send()
            .await
            .context("could not fetch EVE SSO signing keys")?
            .error_for_status()
            .context("EVE SSO signing keys returned an error")?
            .json::<JwkSet>()
            .await
            .context("could not parse EVE SSO signing keys")?;
        let key = keys
            .find(key_id)
            .context("EVE SSO signing key was not found")?;

        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
        validation.set_issuer(&EXPECTED_ISSUERS);
        validation.set_audience(&["EVE Online", client_id]);
        let decoded = decode::<TokenClaims>(
            token.expose_secret(),
            &DecodingKey::from_jwk(key).context("EVE SSO signing key is invalid")?,
            &validation,
        )
        .context("EVE SSO access token failed validation")?;
        let claims = decoded.claims;

        if !EXPECTED_ISSUERS.contains(&claims.iss.as_str()) {
            bail!("EVE SSO access token has an unexpected issuer");
        }
        if !claims.aud.iter().any(|audience| audience == "EVE Online")
            || !claims.aud.iter().any(|audience| audience == client_id)
        {
            bail!("EVE SSO access token is not intended for this application");
        }
        let id = parse_character_subject(&claims.sub)?;
        if claims.name.trim().is_empty() {
            bail!("EVE SSO access token has a blank character name");
        }
        let expires_at = DateTime::from_timestamp(claims.exp, 0)
            .context("EVE SSO access token has an invalid expiry")?;

        Ok(ValidatedAccessToken {
            secret: token,
            expires_at,
            character: CharacterIdentity {
                id,
                name: claims.name,
                scopes: claims.scp,
            },
        })
    }
}

impl SsoMetadata {
    fn validate(&self) -> Result<()> {
        if !EXPECTED_ISSUERS.contains(&self.issuer.as_str()) {
            bail!("EVE SSO metadata has an unexpected issuer");
        }
        for (label, endpoint) in [
            ("authorization", &self.authorization_endpoint),
            ("token", &self.token_endpoint),
            ("signing-key", &self.jwks_uri),
        ] {
            if endpoint.scheme() != "https" || endpoint.host_str() != Some("login.eveonline.com") {
                bail!("EVE SSO {label} endpoint is not on login.eveonline.com");
            }
        }
        Ok(())
    }
}

async fn parse_token_response(response: reqwest::Response) -> Result<RawTokenResponse> {
    let status = response.status();
    if !status.is_success() {
        let error = response.json::<OAuthError>().await.ok();
        let summary = error
            .and_then(|error| error.error_description.or(error.error))
            .unwrap_or_else(|| "no safe error detail was returned".to_owned());
        bail!("EVE SSO rejected the token request ({status}): {summary}");
    }
    let token = response
        .json::<RawTokenResponse>()
        .await
        .context("could not parse the EVE SSO token response")?;
    if !token.token_type.eq_ignore_ascii_case("bearer") {
        bail!("EVE SSO returned an unexpected token type");
    }
    Ok(token)
}

fn random_urlsafe(byte_count: usize) -> Result<String> {
    let mut bytes = vec![0_u8; byte_count];
    getrandom::fill(&mut bytes).context("could not obtain secure randomness")?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn parse_character_subject(subject: &str) -> Result<u64> {
    subject
        .strip_prefix("CHARACTER:EVE:")
        .context("EVE SSO access token has an unexpected subject")?
        .parse()
        .context("EVE SSO access token has an invalid character ID")
}

/// The required scopes plus any extras, without duplicates or ordering churn.
fn requested_scopes(required: &[&str], extra: &[&str]) -> Vec<String> {
    let mut scopes = required
        .iter()
        .map(|scope| (*scope).to_owned())
        .collect::<Vec<_>>();
    for scope in extra {
        if !scopes.iter().any(|existing| existing == scope) {
            scopes.push((*scope).to_owned());
        }
    }
    scopes
}

fn validate_required_scopes(required: &[&str], scopes: &[String]) -> Result<()> {
    let missing = required
        .iter()
        .filter(|required| !scopes.iter().any(|scope| scope == **required))
        .copied()
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        bail!(
            "EVE authorization is missing required scopes: {}",
            missing.join(", ")
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::TEST_APP;

    const REQUIRED_SCOPES: &[&str] = TEST_APP.required_scopes;

    fn metadata() -> SsoMetadata {
        SsoMetadata {
            issuer: "https://login.eveonline.com/".to_owned(),
            authorization_endpoint: "https://login.eveonline.com/v2/oauth/authorize"
                .parse()
                .unwrap(),
            token_endpoint: "https://login.eveonline.com/v2/oauth/token"
                .parse()
                .unwrap(),
            jwks_uri: "https://login.eveonline.com/oauth/jwks".parse().unwrap(),
        }
    }

    #[test]
    fn authorization_request_uses_pkce_and_all_required_scopes() {
        let client = SsoClient::with_metadata_url(METADATA_URL.parse().unwrap()).unwrap();
        let request = client
            .authorization_request(
                &metadata(),
                "public-client-id",
                "http://127.0.0.1:17632/callback",
                &[],
            )
            .unwrap();
        let parameters = request
            .url
            .query_pairs()
            .collect::<std::collections::HashMap<_, _>>();

        assert_eq!(parameters.get("response_type").unwrap(), "code");
        assert_eq!(parameters.get("code_challenge_method").unwrap(), "S256");
        assert_eq!(parameters.get("client_id").unwrap(), "public-client-id");
        assert_eq!(parameters.get("scope").unwrap(), &REQUIRED_SCOPES.join(" "));
        assert!(!parameters.contains_key("client_secret"));
        assert!(
            !request
                .url
                .as_str()
                .contains(request.verifier.expose_secret())
        );
    }

    #[test]
    fn character_subject_requires_the_eve_namespace() {
        assert_eq!(
            parse_character_subject("CHARACTER:EVE:2112345678").unwrap(),
            2_112_345_678
        );
        assert!(parse_character_subject("USER:2112345678").is_err());
    }

    #[test]
    fn an_extra_scope_is_appended_without_duplicating_the_required_set() {
        let client = SsoClient::with_metadata_url(METADATA_URL.parse().unwrap()).unwrap();
        let request = client
            .authorization_request(
                &metadata(),
                "public-client-id",
                "http://127.0.0.1:17632/callback",
                &[
                    crate::esi::CORPORATION_JOBS_SCOPE,
                    // Asking for a scope that is already required must not
                    // repeat it: EVE shows the raw list on the consent page.
                    REQUIRED_SCOPES[0],
                ],
            )
            .unwrap();
        let parameters = request
            .url
            .query_pairs()
            .collect::<std::collections::HashMap<_, _>>();
        let scopes = parameters.get("scope").unwrap();

        assert!(scopes.contains(crate::esi::CORPORATION_JOBS_SCOPE));
        assert_eq!(
            scopes.split(' ').count(),
            REQUIRED_SCOPES.len() + 1,
            "{scopes}"
        );
    }

    #[test]
    fn required_scopes_are_enforced() {
        let all = REQUIRED_SCOPES
            .iter()
            .map(|scope| (*scope).to_owned())
            .collect::<Vec<_>>();
        assert!(validate_required_scopes(REQUIRED_SCOPES, &all).is_ok());
        assert!(
            validate_required_scopes(REQUIRED_SCOPES, &[REQUIRED_SCOPES[0].to_owned()]).is_err()
        );
    }

    #[test]
    fn an_sso_outage_is_shared_by_every_clone_and_backs_off() {
        let client = SsoClient::new(TEST_APP).unwrap();
        let other = client.clone();
        client.record_outage(true);
        client.record_outage(true);
        let outage = other.outage.lock().unwrap();
        assert_eq!(outage.backoff, OUTAGE_FIRST_BACKOFF * 2);
        assert!(outage.until.is_some_and(|until| until > Instant::now()));
        drop(outage);
        other.record_outage(false);
        assert!(client.outage.lock().unwrap().until.is_none());
    }
}
