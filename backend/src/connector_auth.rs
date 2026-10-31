//! HTTP authentication shared by data connectors: none, Basic, bearer token, and OAuth 2.0
//! client credentials (RFC 6749 §4.4).
//!
//! Connectors are rebuilt from stored settings on every request, so OAuth access tokens are
//! cached process-wide, keyed by every setting that identifies the client, until shortly before
//! they expire.

use std::{
    collections::{hash_map::DefaultHasher, HashMap},
    hash::{Hash, Hasher},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

use reqwest::{header, Client, RequestBuilder, Response, Url};
use serde_json::{json, Value};

use crate::data_connectors::{
    credential_setting, shared_http_client, string_setting, ConnectionConfig, ConnectorError,
};

const TOKEN_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_TOKEN_RESPONSE_BYTES: usize = 64 * 1024;
const MAX_ACCESS_TOKEN_BYTES: usize = 16 * 1024;
const MAX_CACHED_TOKENS: usize = 256;
/// Tokens without `expires_in` are reused only briefly, since their lifetime is unknown.
const UNKNOWN_TOKEN_LIFETIME: Duration = Duration::from_secs(60);
const EXPIRY_MARGIN: Duration = Duration::from_secs(30);

#[derive(Debug, thiserror::Error)]
#[error("Could not obtain an OAuth 2.0 access token")]
pub struct TokenError;

pub enum HttpAuth {
    None,
    Basic {
        username: String,
        password: Option<String>,
    },
    Bearer(String),
    OAuth2(OAuth2ClientCredentials),
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum ClientAuthentication {
    /// `client_secret_basic`: the default every authorization server must support.
    Basic,
    /// `client_secret_post`, for servers that only accept credentials in the request body.
    Body,
}

pub struct OAuth2ClientCredentials {
    /// HTTPS, or HTTP on a loopback host only; see `token_endpoint`.
    endpoint: Url,
    client_id: String,
    client_secret: String,
    scopes: Option<String>,
    audience: Option<String>,
    client_authentication: ClientAuthentication,
    client: Client,
}

struct CachedToken {
    value: String,
    expires: Instant,
}

/// Connection-form fields for every supported mode; `auth_type` selects which apply.
pub fn connection_fields() -> Vec<Value> {
    vec![
        json!({"name":"auth_type","label":"Authentication","input_type":"select","required":false,"secret":false,"editable":true,"group":"authentication","placeholder":"","options":[{"value":"none","label":"None"},{"value":"basic","label":"Basic"},{"value":"bearer","label":"Bearer token"},{"value":"oauth2","label":"OAuth 2.0 client credentials"}]}),
        json!({"name":"username","label":"Username","input_type":"text","required":true,"secret":true,"editable":true,"group":"credentials","placeholder":"","visible_when":{"field":"auth_type","value":"basic"}}),
        json!({"name":"password","label":"Password","input_type":"password","required":true,"secret":true,"editable":true,"group":"credentials","placeholder":"","visible_when":{"field":"auth_type","value":"basic"}}),
        json!({"name":"bearer_token","label":"Bearer token","input_type":"password","required":true,"secret":true,"editable":true,"group":"credentials","placeholder":"","visible_when":{"field":"auth_type","value":"bearer"}}),
        json!({"name":"oauth_token_url","label":"Token URL","input_type":"url","required":true,"secret":false,"editable":true,"group":"authentication","placeholder":"https://login.example.com/oauth2/token","visible_when":{"field":"auth_type","value":"oauth2"}}),
        json!({"name":"oauth_client_auth","label":"Client authentication","input_type":"select","required":false,"secret":false,"editable":true,"group":"authentication","placeholder":"","options":[{"value":"basic","label":"HTTP Basic header"},{"value":"body","label":"Request body"}],"visible_when":{"field":"auth_type","value":"oauth2"}}),
        json!({"name":"oauth_scopes","label":"Scopes (optional)","input_type":"text","required":false,"secret":false,"editable":true,"group":"authentication","placeholder":"space-separated","visible_when":{"field":"auth_type","value":"oauth2"}}),
        json!({"name":"oauth_audience","label":"Audience (optional)","input_type":"text","required":false,"secret":false,"editable":true,"group":"authentication","placeholder":"","visible_when":{"field":"auth_type","value":"oauth2"}}),
        json!({"name":"oauth_client_id","label":"Client ID","input_type":"text","required":true,"secret":true,"editable":true,"group":"credentials","placeholder":"","visible_when":{"field":"auth_type","value":"oauth2"}}),
        json!({"name":"oauth_client_secret","label":"Client secret","input_type":"password","required":true,"secret":true,"editable":true,"group":"credentials","placeholder":"","visible_when":{"field":"auth_type","value":"oauth2"}}),
    ]
}

impl HttpAuth {
    /// Reads the selected mode. Forms submit hidden inputs for other modes as empty strings, so only
    /// the selected mode's values are read; a missing `auth_type` is inferred from the credentials.
    pub fn from_config(config: &ConnectionConfig) -> Result<Self, ConnectorError> {
        let invalid = ConnectorError::invalid_connection;
        let mode = match string_setting(config, "auth_type")? {
            Some(mode) => mode,
            None if credential_setting(config, "oauth_client_id")?.is_some() => "oauth2".to_owned(),
            None if credential_setting(config, "bearer_token")?.is_some() => "bearer".to_owned(),
            None if credential_setting(config, "username")?.is_some()
                || string_setting(config, "password")?.is_some_and(|value| !value.is_empty()) =>
            {
                "basic".to_owned()
            }
            None => "none".to_owned(),
        };
        match mode.as_str() {
            "none" => Ok(Self::None),
            "basic" => Ok(Self::Basic {
                username: credential_setting(config, "username")?.ok_or_else(invalid)?,
                password: string_setting(config, "password")?.filter(|value| !value.is_empty()),
            }),
            "bearer" => Ok(Self::Bearer(
                credential_setting(config, "bearer_token")?.ok_or_else(invalid)?,
            )),
            "oauth2" => {
                let raw_url = string_setting(config, "oauth_token_url")?.ok_or_else(invalid)?;
                let endpoint = token_endpoint(raw_url.trim()).ok_or_else(invalid)?;
                let optional = |name| -> Result<Option<String>, ConnectorError> {
                    let value =
                        credential_setting(config, name)?.map(|value| value.trim().to_owned());
                    if value.as_deref().is_some_and(|value| {
                        value.len() > 1024 || value.chars().any(char::is_control)
                    }) {
                        return Err(invalid());
                    }
                    Ok(value)
                };
                let client_authentication =
                    match string_setting(config, "oauth_client_auth")?.as_deref() {
                        None | Some("" | "basic") => ClientAuthentication::Basic,
                        Some("body") => ClientAuthentication::Body,
                        Some(_) => return Err(invalid()),
                    };
                let client = shared_http_client(TOKEN_TIMEOUT).map_err(|_| invalid())?;
                Ok(Self::OAuth2(OAuth2ClientCredentials {
                    endpoint,
                    client_id: credential_setting(config, "oauth_client_id")?
                        .ok_or_else(invalid)?,
                    client_secret: credential_setting(config, "oauth_client_secret")?
                        .ok_or_else(invalid)?,
                    scopes: optional("oauth_scopes")?,
                    audience: optional("oauth_audience")?,
                    client_authentication,
                    client,
                }))
            }
            _ => Err(invalid()),
        }
    }

    pub fn mode(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Basic { .. } => "basic",
            Self::Bearer(_) => "bearer",
            Self::OAuth2(_) => "oauth2",
        }
    }

    /// Settings that are safe to show again: the mode and non-secret OAuth endpoint details.
    pub fn public_configuration(&self) -> ConnectionConfig {
        let mut configuration = ConnectionConfig::new();
        configuration.insert("auth_type".to_owned(), json!(self.mode()));
        if let Self::OAuth2(flow) = self {
            configuration.insert("oauth_token_url".to_owned(), json!(flow.endpoint.as_str()));
            configuration.insert(
                "oauth_client_auth".to_owned(),
                json!(match flow.client_authentication {
                    ClientAuthentication::Basic => "basic",
                    ClientAuthentication::Body => "body",
                }),
            );
            for (name, value) in [
                ("oauth_scopes", &flow.scopes),
                ("oauth_audience", &flow.audience),
            ] {
                if let Some(value) = value {
                    configuration.insert(name.to_owned(), json!(value));
                }
            }
        }
        configuration
    }

    /// Adds credentials to a source request, fetching or reusing an OAuth access token.
    pub async fn authorize(&self, request: RequestBuilder) -> Result<RequestBuilder, TokenError> {
        Ok(match self {
            Self::None => request,
            Self::Basic { username, password } => request.basic_auth(username, password.as_deref()),
            Self::Bearer(token) => request.header(header::AUTHORIZATION, bearer_header(token)?),
            Self::OAuth2(flow) => request.header(
                header::AUTHORIZATION,
                bearer_header(&flow.access_token().await?)?,
            ),
        })
    }

    /// Drops a cached OAuth token after the source rejects it, so the next request fetches a new one.
    pub fn invalidate(&self) {
        if let Self::OAuth2(flow) = self {
            if let Ok(mut tokens) = token_cache().lock() {
                tokens.remove(&flow.cache_key());
            }
        }
    }
}

/// RFC 6749 §3.2 requires TLS for the token endpoint, since requests carry the client secret.
/// Plain HTTP is accepted only for loopback hosts, such as a local identity provider.
fn token_endpoint(raw: &str) -> Option<Url> {
    let url = Url::parse(raw).ok()?;
    let loopback = match url.host()? {
        url::Host::Domain(domain) => domain.eq_ignore_ascii_case("localhost"),
        url::Host::Ipv4(address) => address.is_loopback(),
        url::Host::Ipv6(address) => address.is_loopback(),
    };
    let secure = url.scheme() == "https" || (url.scheme() == "http" && loopback);
    (secure && url.username().is_empty() && url.password().is_none() && url.fragment().is_none())
        .then_some(url)
}

fn bearer_header(token: &str) -> Result<header::HeaderValue, TokenError> {
    let mut value =
        header::HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| TokenError)?;
    value.set_sensitive(true);
    Ok(value)
}

fn token_cache() -> &'static Mutex<HashMap<u64, CachedToken>> {
    static TOKENS: OnceLock<Mutex<HashMap<u64, CachedToken>>> = OnceLock::new();
    TOKENS.get_or_init(Default::default)
}

impl OAuth2ClientCredentials {
    /// Includes the secret, so a changed secret is verified instead of reusing an older token.
    fn cache_key(&self) -> u64 {
        let mut hasher = DefaultHasher::new();
        (
            self.endpoint.as_str(),
            &self.client_id,
            &self.client_secret,
            &self.scopes,
            &self.audience,
            self.client_authentication,
        )
            .hash(&mut hasher);
        hasher.finish()
    }

    async fn access_token(&self) -> Result<String, TokenError> {
        let key = self.cache_key();
        let now = Instant::now();
        if let Some(token) = token_cache()
            .lock()
            .map_err(|_| TokenError)?
            .get(&key)
            .filter(|token| token.expires > now)
        {
            return Ok(token.value.clone());
        }
        let (value, lifetime) = self.request_token().await?;
        let mut tokens = token_cache().lock().map_err(|_| TokenError)?;
        if tokens.len() >= MAX_CACHED_TOKENS {
            tokens.retain(|_, token| token.expires > now);
            if tokens.len() >= MAX_CACHED_TOKENS {
                tokens.clear();
            }
        }
        tokens.insert(
            key,
            CachedToken {
                value: value.clone(),
                expires: now + lifetime,
            },
        );
        Ok(value)
    }

    async fn request_token(&self) -> Result<(String, Duration), TokenError> {
        let mut form = vec![("grant_type", "client_credentials")];
        if let Some(scopes) = self.scopes.as_deref() {
            form.push(("scope", scopes));
        }
        if let Some(audience) = self.audience.as_deref() {
            form.push(("audience", audience));
        }
        let mut request = self
            .client
            .post(self.endpoint.clone())
            .header(header::ACCEPT, "application/json");
        request = match self.client_authentication {
            // RFC 6749 §2.3.1 form-encodes the identifier and secret before Basic encoding.
            ClientAuthentication::Basic => request.basic_auth(
                form_encode(&self.client_id),
                Some(form_encode(&self.client_secret)),
            ),
            ClientAuthentication::Body => {
                form.push(("client_id", &self.client_id));
                form.push(("client_secret", &self.client_secret));
                request
            }
        };
        let response = request.form(&form).send().await.map_err(|_| TokenError)?;
        let body = read_token_response(response).await?;
        let body: Value = serde_json::from_slice(&body).map_err(|_| TokenError)?;
        let token = body
            .get("access_token")
            .and_then(Value::as_str)
            .filter(|token| {
                !token.is_empty()
                    && token.len() <= MAX_ACCESS_TOKEN_BYTES
                    && token.bytes().all(|byte| byte.is_ascii_graphic())
            })
            .ok_or(TokenError)?;
        if body
            .get("token_type")
            .and_then(Value::as_str)
            .is_some_and(|kind| !kind.eq_ignore_ascii_case("bearer"))
        {
            return Err(TokenError);
        }
        let expires_in = match body.get("expires_in") {
            Some(Value::Number(seconds)) => seconds.as_u64(),
            Some(Value::String(seconds)) => seconds.trim().parse::<u64>().ok(),
            _ => None,
        };
        let lifetime = match expires_in.map(Duration::from_secs) {
            Some(lifetime) if lifetime > 2 * EXPIRY_MARGIN => lifetime - EXPIRY_MARGIN,
            Some(lifetime) => lifetime / 2,
            None => UNKNOWN_TOKEN_LIFETIME,
        };
        Ok((token.to_owned(), lifetime))
    }
}

fn form_encode(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

/// Token responses never reach users or logs; failures carry no upstream detail.
async fn read_token_response(mut response: Response) -> Result<Vec<u8>, TokenError> {
    if !response.status().is_success()
        || response
            .content_length()
            .is_some_and(|length| length > MAX_TOKEN_RESPONSE_BYTES as u64)
    {
        return Err(TokenError);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| TokenError)? {
        if body.len().saturating_add(chunk.len()) > MAX_TOKEN_RESPONSE_BYTES {
            return Err(TokenError);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{matchers, Mock, MockServer, ResponseTemplate};

    fn config(value: Value) -> ConnectionConfig {
        value.as_object().unwrap().clone()
    }

    fn oauth(token_url: &str, secret: &str, client_auth: &str) -> HttpAuth {
        HttpAuth::from_config(&config(json!({
            "auth_type":"oauth2","oauth_token_url":token_url,"oauth_client_id":"reader app",
            "oauth_client_secret":secret,"oauth_scopes":" logs:read ","oauth_audience":"",
            "oauth_client_auth":client_auth,"username":"","bearer_token":""
        })))
        .unwrap()
    }

    async fn authorization(auth: &HttpAuth) -> Option<String> {
        let request = auth
            .authorize(Client::new().get("http://localhost/"))
            .await
            .ok()?
            .build()
            .unwrap();
        request
            .headers()
            .get(header::AUTHORIZATION)
            .map(|value| value.to_str().unwrap().to_owned())
    }

    #[test]
    fn modes_read_only_their_own_settings_and_reject_partial_credentials() {
        let blank = json!({"username":"","password":"","bearer_token":"","oauth_client_id":""});
        assert_eq!(
            HttpAuth::from_config(&config(blank)).unwrap().mode(),
            "none"
        );
        let hidden = json!({"auth_type":"bearer","bearer_token":"token","username":"stale"});
        assert_eq!(
            HttpAuth::from_config(&config(hidden)).unwrap().mode(),
            "bearer"
        );
        for rejected in [
            json!({"auth_type":"basic","username":" "}),
            json!({"auth_type":"bearer","bearer_token":"   "}),
            json!({"auth_type":"oauth2","oauth_token_url":"https://idp/token","oauth_client_id":"id"}),
            json!({"auth_type":"oauth2","oauth_token_url":"https://user:pw@idp/token","oauth_client_id":"id","oauth_client_secret":"s"}),
            json!({"auth_type":"oauth2","oauth_token_url":"ftp://idp/token","oauth_client_id":"id","oauth_client_secret":"s"}),
            json!({"auth_type":"oauth2","oauth_token_url":"http://idp.example.com/token","oauth_client_id":"id","oauth_client_secret":"s"}),
            json!({"auth_type":"oauth2","oauth_token_url":"https://idp/token","oauth_client_id":"id","oauth_client_secret":"s","oauth_client_auth":"jwt"}),
            json!({"auth_type":"kerberos"}),
        ] {
            assert!(
                HttpAuth::from_config(&config(rejected.clone())).is_err(),
                "{rejected}"
            );
        }
    }

    #[test]
    fn token_endpoints_require_tls_except_on_loopback() {
        for accepted in [
            "https://idp.example.com/oauth2/token?tenant=a",
            "http://localhost:8080/token",
            "http://127.0.0.1:9000/token",
            "http://[::1]/token",
        ] {
            assert!(token_endpoint(accepted).is_some(), "{accepted}");
        }
        for rejected in [
            "http://idp.example.com/token",
            "http://10.0.0.5/token",
            "http://localhost.example.com/token",
            "https://user:pw@idp/token",
            "https://idp/token#fragment",
            "ftp://idp/token",
            "not a url",
        ] {
            assert!(token_endpoint(rejected).is_none(), "{rejected}");
        }
    }

    #[test]
    fn public_configuration_never_includes_client_credentials() {
        let auth = oauth("https://idp.example/token", "top-secret", "body");
        let public = Value::Object(auth.public_configuration()).to_string();
        assert!(public.contains("https://idp.example/token"));
        assert!(public.contains("logs:read"));
        assert!(!public.contains("top-secret"));
        assert!(!public.contains("reader app"));
    }

    #[tokio::test]
    async fn client_credentials_tokens_are_fetched_once_and_reused_until_invalidated() {
        let server = MockServer::start().await;
        Mock::given(matchers::method("POST"))
            .and(matchers::path("/token"))
            .and(matchers::header(
                "authorization",
                "Basic cmVhZGVyK2FwcDpzJTI2Y3JldA==",
            ))
            .and(matchers::body_string(
                "grant_type=client_credentials&scope=logs%3Aread",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({"access_token":"token-1","token_type":"Bearer","expires_in":3600}),
            ))
            .expect(2)
            .mount(&server)
            .await;
        let auth = oauth(&format!("{}/token", server.uri()), "s&cret", "basic");
        assert_eq!(
            authorization(&auth).await.as_deref(),
            Some("Bearer token-1")
        );
        let rebuilt = oauth(&format!("{}/token", server.uri()), "s&cret", "basic");
        assert_eq!(
            authorization(&rebuilt).await.as_deref(),
            Some("Bearer token-1")
        );
        rebuilt.invalidate();
        assert_eq!(
            authorization(&auth).await.as_deref(),
            Some("Bearer token-1")
        );
    }

    #[tokio::test]
    async fn body_client_authentication_and_unusable_token_responses() {
        let server = MockServer::start().await;
        Mock::given(matchers::path("/token"))
            .and(matchers::body_string(
                "grant_type=client_credentials&scope=logs%3Aread&client_id=reader+app&client_secret=other",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"access_token":"token-2"})))
            .expect(1)
            .mount(&server)
            .await;
        let auth = oauth(&format!("{}/token", server.uri()), "other", "body");
        assert_eq!(
            authorization(&auth).await.as_deref(),
            Some("Bearer token-2")
        );

        for (path, response) in [
            (
                "/denied",
                ResponseTemplate::new(401).set_body_string("invalid_client secret=leak"),
            ),
            (
                "/mac",
                ResponseTemplate::new(200)
                    .set_body_json(json!({"access_token":"t","token_type":"mac"})),
            ),
            (
                "/empty",
                ResponseTemplate::new(200).set_body_json(json!({"token_type":"Bearer"})),
            ),
            (
                "/header",
                ResponseTemplate::new(200).set_body_json(json!({"access_token":"bad\ntoken"})),
            ),
        ] {
            Mock::given(matchers::path(path))
                .respond_with(response)
                .mount(&server)
                .await;
            let auth = oauth(&format!("{}{path}", server.uri()), "x", "basic");
            assert!(authorization(&auth).await.is_none(), "{path}");
        }
    }
}
