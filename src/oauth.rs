use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::RngCore;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;

pub const GMAIL_SCOPE: &str = "https://www.googleapis.com/auth/gmail.modify";
const USERINFO_SCOPE: &str = "openid email profile";

#[derive(Clone, Debug, Deserialize)]
pub struct OAuthCredentials {
    pub client_id: String,
    pub client_secret: String,
    pub auth_uri: String,
    pub token_uri: String,
}

#[derive(Deserialize)]
struct CredentialsFile {
    installed: OAuthCredentials,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TokenSet {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: u64,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: u64,
}

impl OAuthCredentials {
    pub fn from_google_file(path: &Path) -> Result<Self> {
        let contents = fs::read_to_string(path)
            .with_context(|| format!("could not read {}", path.display()))?;
        let file: CredentialsFile = serde_json::from_str(&contents)
            .context("this is not a Google Desktop OAuth credentials file")?;
        Ok(file.installed)
    }

    pub fn authorize(&self) -> Result<TokenSet> {
        let listener = TcpListener::bind("127.0.0.1:0").context("could not open OAuth callback")?;
        listener
            .set_nonblocking(false)
            .context("could not configure OAuth callback")?;
        let port = listener.local_addr()?.port();
        let redirect_uri = format!("http://127.0.0.1:{port}");

        let state = random_urlsafe(24);
        let verifier = random_urlsafe(48);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));

        let mut authorization_url = Url::parse(&self.auth_uri)?;
        authorization_url
            .query_pairs_mut()
            .append_pair("client_id", &self.client_id)
            .append_pair("redirect_uri", &redirect_uri)
            .append_pair("response_type", "code")
            .append_pair("scope", &format!("{GMAIL_SCOPE} {USERINFO_SCOPE}"))
            .append_pair("access_type", "offline")
            .append_pair("prompt", "consent select_account")
            .append_pair("state", &state)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256");

        open::that(authorization_url.as_str()).context("could not open the system browser")?;
        let (mut stream, _) = listener.accept().context("OAuth callback failed")?;
        let callback = read_callback(&mut stream)?;
        let result = parse_callback(&callback, &state);
        write_browser_response(&mut stream, result.is_ok())?;
        let code = result?;

        let response = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?
            .post(&self.token_uri)
            .form(&[
                ("code", code.as_str()),
                ("client_id", self.client_id.as_str()),
                ("client_secret", self.client_secret.as_str()),
                ("redirect_uri", redirect_uri.as_str()),
                ("grant_type", "authorization_code"),
                ("code_verifier", verifier.as_str()),
            ])
            .send()?
            .error_for_status()
            .context("Google rejected the authorization code")?
            .json::<TokenResponse>()?;

        Ok(TokenSet {
            access_token: response.access_token,
            refresh_token: response.refresh_token,
            expires_at: unix_now() + response.expires_in,
        })
    }

    pub fn refresh(&self, refresh_token: &str) -> Result<TokenSet> {
        let response = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?
            .post(&self.token_uri)
            .form(&[
                ("client_id", self.client_id.as_str()),
                ("client_secret", self.client_secret.as_str()),
                ("refresh_token", refresh_token),
                ("grant_type", "refresh_token"),
            ])
            .send()?
            .error_for_status()
            .context("Google rejected the refresh token")?
            .json::<TokenResponse>()?;

        Ok(TokenSet {
            access_token: response.access_token,
            refresh_token: Some(refresh_token.to_owned()),
            expires_at: unix_now() + response.expires_in,
        })
    }
}

impl TokenSet {
    pub fn expires_soon(&self) -> bool {
        self.expires_at <= unix_now() + 60
    }
}

fn random_urlsafe(bytes: usize) -> String {
    let mut value = vec![0; bytes];
    rand::rng().fill_bytes(&mut value);
    URL_SAFE_NO_PAD.encode(value)
}

fn read_callback(stream: &mut TcpStream) -> Result<String> {
    stream.set_read_timeout(Some(Duration::from_secs(120)))?;
    let mut buffer = [0_u8; 8192];
    let length = stream.read(&mut buffer)?;
    if length == 0 {
        bail!("Google returned an empty OAuth callback");
    }
    Ok(String::from_utf8_lossy(&buffer[..length]).into_owned())
}

fn parse_callback(request: &str, expected_state: &str) -> Result<String> {
    let target = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .context("invalid OAuth callback")?;
    let url = Url::parse(&format!("http://127.0.0.1{target}"))?;
    let params: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
    if let Some(error) = params.get("error") {
        bail!("Google authorization failed: {error}");
    }
    if params.get("state").map(String::as_str) != Some(expected_state) {
        bail!("OAuth state did not match; authorization was cancelled for safety");
    }
    params
        .get("code")
        .cloned()
        .context("Google did not return an authorization code")
}

fn write_browser_response(stream: &mut TcpStream, success: bool) -> Result<()> {
    let (status, title, message) = if success {
        (
            "200 OK",
            "Postbird is connected",
            "You can close this tab and return to Postbird.",
        )
    } else {
        (
            "400 Bad Request",
            "Postbird could not connect",
            "Return to Postbird for more information.",
        )
    };
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>{title}</title><style>body{{font:18px system-ui;max-width:38rem;margin:12vh auto;padding:2rem}}h1{{font-size:2rem}}</style><h1>{title}</h1><p>{message}</p>"
    );
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    stream.flush()?;
    Ok(())
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_google_desktop_credentials() {
        let json = r#"{"installed":{"client_id":"id","client_secret":"secret","auth_uri":"https://accounts.example/auth","token_uri":"https://accounts.example/token"}}"#;
        let file: CredentialsFile = serde_json::from_str(json).unwrap();
        assert_eq!(file.installed.client_id, "id");
    }

    #[test]
    fn validates_callback_state() {
        let request = "GET /?state=right&code=abc HTTP/1.1\r\n\r\n";
        assert_eq!(parse_callback(request, "right").unwrap(), "abc");
        assert!(parse_callback(request, "wrong").is_err());
    }
}
