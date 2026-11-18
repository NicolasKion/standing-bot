//! Settings read from environment variables.

use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::str::FromStr;

use anyhow::{Context, Result, bail};

pub struct Config {
    pub discord_token: String,
    pub eve_client_id: String,
    pub eve_client_secret: String,
    /// Public base URL of the web server, without a trailing slash.
    pub public_url: String,
    pub bind_addr: SocketAddr,
    pub database_path: PathBuf,
    /// Standing the characters give each other.
    pub standing: f64,
    /// Contact shown to CCP in the ESI user agent.
    pub contact: String,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let standing: f64 = parsed_or("STANDING", 6.7)?;
        if !(-10.0..=10.0).contains(&standing) {
            bail!("STANDING must be between -10 and 10, got {standing}");
        }

        Ok(Self {
            discord_token: required("DISCORD_TOKEN")?,
            eve_client_id: required("EVE_CLIENT_ID")?,
            eve_client_secret: required("EVE_CLIENT_SECRET")?,
            public_url: required("PUBLIC_URL")?.trim_end_matches('/').to_owned(),
            bind_addr: parsed_or("BIND_ADDR", SocketAddr::from(([0, 0, 0, 0], 8080)))?,
            database_path: parsed_or("DATABASE_PATH", PathBuf::from("standing-bot.db"))?,
            standing,
            contact: required("CONTACT")?,
        })
    }

    pub fn callback_url(&self) -> String {
        format!("{}/callback", self.public_url)
    }
}

fn optional(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.trim().is_empty())
}

fn required(name: &str) -> Result<String> {
    optional(name).with_context(|| format!("environment variable {name} is not set"))
}

fn parsed<T>(name: &str) -> Result<T>
where
    T: FromStr,
    T::Err: std::error::Error + Send + Sync + 'static,
{
    let value = required(name)?;
    value
        .trim()
        .parse()
        .with_context(|| format!("environment variable {name} has an invalid value: {value}"))
}

fn parsed_or<T>(name: &str, default: T) -> Result<T>
where
    T: FromStr,
    T::Err: std::error::Error + Send + Sync + 'static,
{
    if optional(name).is_some() {
        parsed(name)
    } else {
        Ok(default)
    }
}
