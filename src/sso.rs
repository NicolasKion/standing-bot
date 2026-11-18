//! EVE SSO: the login link, token exchange, refresh and access token checks.

use anyhow::{Context, Result, anyhow, bail};
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use reqwest::{Client, StatusCode, Url};
use serde::Deserialize;
use tokio::sync::RwLock;

use crate::config::Config;

const METADATA_URL: &str = "https://login.eveonline.com/.well-known/oauth-authorization-server";

/// Optional scope for characters that manage the tracked access list.
pub const ACCESS_LIST_SCOPE: &str = "esi-access.read_lists.v1";

/// Scopes every linked character has to grant.
pub const SCOPES: [&str; 2] = [
    "esi-characters.read_contacts.v1",
    "esi-characters.write_contacts.v1",
];

const ISSUERS: [&str; 3] = [
    "login.eveonline.com",
    "https://login.eveonline.com",
    "https://login.eveonline.com/",
];

#[derive(Deserialize)]
struct Metadata {
    authorization_endpoint: String,
    token_endpoint: String,
    jwks_uri: String,
}

#[derive(Deserialize)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
}

#[derive(Deserialize, Default)]
struct OAuthError {
    #[serde(default)]
    error: String,
}

#[derive(Deserialize)]
struct Claims {
    sub: String,
    name: String,
    #[serde(default)]
    scp: Scopes,
}

/// The `scp` claim is a plain string when only one scope was granted.
#[derive(Deserialize)]
#[serde(untagged)]
enum Scopes {
    One(String),
    Many(Vec<String>),
}

impl Default for Scopes {
    fn default() -> Self {
        Self::Many(Vec::new())
    }
}

/// The character an access token belongs to.
pub struct Identity {
    pub character_id: i64,
    pub name: String,
    pub scopes: Vec<String>,
}

impl Identity {
    pub fn has_scope(&self, scope: &str) -> bool {
        self.scopes.iter().any(|granted| granted == scope)
    }

    pub fn has_required_scopes(&self) -> bool {
        SCOPES
            .iter()
            .all(|required| self.scopes.iter().any(|granted| granted == required))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RefreshError {
    /// The refresh token no longer works: the player revoked the app, changed
    /// their password or moved the character to another account.
    #[error("refresh token was rejected")]
    Revoked,
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub struct Sso {
    http: Client,
    client_id: String,
    client_secret: String,
    callback_url: String,
    metadata: Metadata,
    jwks: RwLock<JwkSet>,
}

impl Sso {
    pub async fn connect(http: Client, config: &Config) -> Result<Self> {
        let metadata: Metadata = http
            .get(METADATA_URL)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .context("fetching EVE SSO metadata")?
            .json()
            .await
            .context("reading EVE SSO metadata")?;
        let jwks = fetch_jwks(&http, &metadata.jwks_uri).await?;

        Ok(Self {
            http,
            client_id: config.eve_client_id.clone(),
            client_secret: config.eve_client_secret.clone(),
            callback_url: config.callback_url(),
            metadata,
            jwks: RwLock::new(jwks),
        })
    }

    /// The URL a player opens to log in with a character. `extra_scopes` are
    /// asked for on top of the required ones.
    pub fn authorize_url(&self, state: &str, extra_scopes: &[&str]) -> Result<Url> {
        let scope = [&SCOPES[..], extra_scopes].concat().join(" ");
        Url::parse_with_params(
            &self.metadata.authorization_endpoint,
            [
                ("response_type", "code"),
                ("redirect_uri", self.callback_url.as_str()),
                ("client_id", self.client_id.as_str()),
                ("scope", scope.as_str()),
                ("state", state),
            ],
        )
        .context("building the EVE login URL")
    }

    /// Trades the code from the login callback for tokens.
    pub async fn exchange(&self, code: &str) -> Result<Tokens> {
        let response = self
            .token_request(&[("grant_type", "authorization_code"), ("code", code)])
            .await?;
        if !response.status().is_success() {
            bail!("EVE SSO rejected the login code: {}", response.status());
        }
        response.json().await.context("reading the token response")
    }

    /// Gets a fresh access token. The returned refresh token can differ from
    /// the one passed in and has to be stored.
    pub async fn refresh(&self, refresh_token: &str) -> Result<Tokens, RefreshError> {
        let response = self
            .token_request(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token),
            ])
            .await?;

        let status = response.status();
        if status == StatusCode::BAD_REQUEST {
            let body: OAuthError = response.json().await.unwrap_or_default();
            if body.error == "invalid_grant" {
                return Err(RefreshError::Revoked);
            }
            return Err(anyhow!("EVE SSO refused the refresh: {}", body.error).into());
        }
        if !status.is_success() {
            return Err(anyhow!("EVE SSO refresh failed: {status}").into());
        }
        let tokens = response
            .json()
            .await
            .context("reading the token response")?;
        Ok(tokens)
    }

    /// Checks an access token's signature and claims and returns its character.
    pub async fn verify(&self, access_token: &str) -> Result<Identity> {
        let header = jsonwebtoken::decode_header(access_token).context("reading the JWT header")?;
        let kid = header.kid.context("access token has no key id")?;
        let key = self.decoding_key(&kid).await?;

        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_audience(&[&self.client_id]);
        validation.set_issuer(&ISSUERS);
        let claims = jsonwebtoken::decode::<Claims>(access_token, &key, &validation)
            .context("validating the access token")?
            .claims;

        let character_id = claims
            .sub
            .strip_prefix("CHARACTER:EVE:")
            .and_then(|id| id.parse().ok())
            .with_context(|| format!("unexpected token subject: {}", claims.sub))?;
        let scopes = match claims.scp {
            Scopes::One(scope) => vec![scope],
            Scopes::Many(scopes) => scopes,
        };
        Ok(Identity {
            character_id,
            name: claims.name,
            scopes,
        })
    }

    async fn token_request(&self, form: &[(&str, &str)]) -> Result<reqwest::Response> {
        self.http
            .post(&self.metadata.token_endpoint)
            .basic_auth(&self.client_id, Some(&self.client_secret))
            .form(form)
            .send()
            .await
            .context("calling the EVE SSO token endpoint")
    }

    /// Finds the signing key, refetching the key set once in case CCP rotated it.
    async fn decoding_key(&self, kid: &str) -> Result<DecodingKey> {
        if let Some(jwk) = self.jwks.read().await.find(kid) {
            return DecodingKey::from_jwk(jwk).context("reading the EVE SSO signing key");
        }

        let fresh = fetch_jwks(&self.http, &self.metadata.jwks_uri).await?;
        let key = fresh
            .find(kid)
            .with_context(|| format!("EVE SSO has no signing key {kid}"))
            .and_then(|jwk| DecodingKey::from_jwk(jwk).context("reading the EVE SSO signing key"));
        *self.jwks.write().await = fresh;
        key
    }
}

async fn fetch_jwks(http: &Client, url: &str) -> Result<JwkSet> {
    http.get(url)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .context("fetching EVE SSO signing keys")?
        .json()
        .await
        .context("reading EVE SSO signing keys")
}
