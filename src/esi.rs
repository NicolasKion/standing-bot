//! The ESI routes the bot uses: character contacts, access lists and affiliations.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::anyhow;
use reqwest::header::RETRY_AFTER;
use reqwest::{Client, Method, RequestBuilder, Response};
use serde::Deserialize;

use crate::access::{AccessList, Affiliation};

const BASE_URL: &str = "https://esi.evetech.net";

/// Pins ESI's behavior. Check what changed before moving it forward.
const COMPATIBILITY_DATE: &str = "2026-08-18";

/// ESI takes at most this many ids in one contact add or edit call.
const WRITE_CHUNK: usize = 100;

/// ESI takes at most this many ids in one contact delete call.
const DELETE_CHUNK: usize = 20;

/// ESI takes at most this many ids in one name lookup.
const NAMES_CHUNK: usize = 1000;

/// ESI takes at most this many characters in one affiliation call.
const AFFILIATION_CHUNK: usize = 1000;

#[derive(Debug, thiserror::Error)]
pub enum EsiError {
    /// The token was refused: a scope is missing or the character has no
    /// right to the resource.
    #[error("ESI refused the access token")]
    Unauthorized,
    /// A rate or error limit was hit. Stop calling ESI for now.
    #[error("ESI rate limit hit")]
    Throttled { retry_after: Option<Duration> },
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl From<reqwest::Error> for EsiError {
    fn from(error: reqwest::Error) -> Self {
        Self::Other(error.into())
    }
}

#[derive(Deserialize)]
struct Contact {
    contact_id: i64,
    standing: f64,
}

#[derive(Deserialize)]
struct Name {
    id: i64,
    name: String,
}

#[derive(Deserialize)]
struct AccessListListing {
    #[serde(default)]
    access_lists: Vec<AccessListId>,
}

#[derive(Deserialize)]
struct AccessListId {
    id: i64,
}

pub struct Esi {
    http: Client,
}

impl Esi {
    pub fn new(http: Client) -> Self {
        Self { http }
    }

    /// All contacts of a character with their standing, keyed by contact id.
    pub async fn contacts(
        &self,
        character_id: i64,
        token: &str,
    ) -> Result<HashMap<i64, f64>, EsiError> {
        let mut contacts = HashMap::new();
        let mut page = 1_u32;
        loop {
            let request = self
                .contacts_request(Method::GET, character_id, token)
                .query(&[("page", page)]);
            let response = send(request).await?;
            let pages: u32 = response
                .headers()
                .get("x-pages")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse().ok())
                .unwrap_or(1);

            let batch: Vec<Contact> = response.json().await?;
            contacts.extend(batch.iter().map(|c| (c.contact_id, c.standing)));

            if page >= pages {
                return Ok(contacts);
            }
            page += 1;
        }
    }

    pub async fn add_contacts(
        &self,
        character_id: i64,
        token: &str,
        contact_ids: &[i64],
        standing: f64,
    ) -> Result<(), EsiError> {
        self.write_contacts(Method::POST, character_id, token, contact_ids, standing)
            .await
    }

    pub async fn edit_contacts(
        &self,
        character_id: i64,
        token: &str,
        contact_ids: &[i64],
        standing: f64,
    ) -> Result<(), EsiError> {
        self.write_contacts(Method::PUT, character_id, token, contact_ids, standing)
            .await
    }

    pub async fn delete_contacts(
        &self,
        character_id: i64,
        token: &str,
        contact_ids: &[i64],
    ) -> Result<(), EsiError> {
        for chunk in contact_ids.chunks(DELETE_CHUNK) {
            let ids: Vec<String> = chunk.iter().map(i64::to_string).collect();
            let request = self
                .contacts_request(Method::DELETE, character_id, token)
                .query(&[("contact_ids", ids.join(","))]);
            send(request).await?;
        }
        Ok(())
    }

    /// Ids of the access lists a character is manager or admin of.
    pub async fn access_list_ids(
        &self,
        character_id: i64,
        token: &str,
    ) -> Result<Vec<i64>, EsiError> {
        let path = format!("/characters/{character_id}/access-lists");
        let request = self.request(Method::GET, &path).bearer_auth(token);
        let listing: AccessListListing = send(request).await?.json().await?;
        Ok(listing.access_lists.iter().map(|list| list.id).collect())
    }

    /// One access list with its members, read as one of its managers.
    pub async fn access_list(
        &self,
        character_id: i64,
        token: &str,
        access_list_id: i64,
    ) -> Result<AccessList, EsiError> {
        let path = format!("/characters/{character_id}/access-lists/{access_list_id}");
        let request = self.request(Method::GET, &path).bearer_auth(token);
        Ok(send(request).await?.json().await?)
    }

    /// Corporation and alliance of each character. Public, needs no token.
    pub async fn affiliations(&self, character_ids: &[i64]) -> Result<Vec<Affiliation>, EsiError> {
        let mut affiliations = Vec::with_capacity(character_ids.len());
        for chunk in character_ids.chunks(AFFILIATION_CHUNK) {
            let request = self
                .request(Method::POST, "/characters/affiliation")
                .json(chunk);
            let batch: Vec<Affiliation> = send(request).await?.json().await?;
            affiliations.extend(batch);
        }
        Ok(affiliations)
    }

    /// Names of characters, corporations or alliances. Public, needs no token.
    pub async fn names(&self, ids: &[i64]) -> Result<HashMap<i64, String>, EsiError> {
        let mut names = HashMap::with_capacity(ids.len());
        for chunk in ids.chunks(NAMES_CHUNK) {
            let request = self.request(Method::POST, "/universe/names").json(chunk);
            let batch: Vec<Name> = send(request).await?.json().await?;
            names.extend(batch.into_iter().map(|entry| (entry.id, entry.name)));
        }
        Ok(names)
    }

    async fn write_contacts(
        &self,
        method: Method,
        character_id: i64,
        token: &str,
        contact_ids: &[i64],
        standing: f64,
    ) -> Result<(), EsiError> {
        for chunk in contact_ids.chunks(WRITE_CHUNK) {
            let request = self
                .contacts_request(method.clone(), character_id, token)
                .query(&[("standing", standing)])
                .json(chunk);
            send(request).await?;
        }
        Ok(())
    }

    fn contacts_request(&self, method: Method, character_id: i64, token: &str) -> RequestBuilder {
        self.request(method, &format!("/characters/{character_id}/contacts"))
            .bearer_auth(token)
    }

    fn request(&self, method: Method, path: &str) -> RequestBuilder {
        self.http
            .request(method, format!("{BASE_URL}{path}"))
            .header("X-Compatibility-Date", COMPATIBILITY_DATE)
    }
}

async fn send(request: RequestBuilder) -> Result<Response, EsiError> {
    let response = request.send().await?;
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }

    match status.as_u16() {
        401 | 403 => Err(EsiError::Unauthorized),
        // 420 is the legacy error limit, 429 the bucket rate limit.
        420 | 429 => {
            let retry_after = response
                .headers()
                .get(RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse().ok())
                .map(Duration::from_secs);
            Err(EsiError::Throttled { retry_after })
        }
        _ => {
            let body = response.text().await.unwrap_or_default();
            Err(anyhow!("ESI answered {status}: {body}").into())
        }
    }
}
