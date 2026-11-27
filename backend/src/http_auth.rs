use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

use reqwest::{header, Client, RequestBuilder, Response, Url};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::data_connectors::{
    credential_setting, string_setting, ConnectionConfig, ConnectorError,
};

const MAX_TOKEN_RESPONSE_BYTES: usize = 64 * 1024;
const MAX_ACCESS_TOKEN_BYTES: usize = 16 * 1024;
const MAX_CACHED_TOKENS: usize = 256;
const MAX_TOKEN_LIFETIME_SECONDS: u64 = 86_400;

#[derive(Debug, thiserror::Error)]
#[error("Could not obtain an OAuth 2.0 access token")]
pub struct TokenError;

#[derive(Clone)]
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
    Basic,
    Body,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct TokenKey {
    endpoint: Url,
    client_id: String,
    client_secret: String,
    scopes: Option<String>,
    audience: Option<String>,
    client_authentication: ClientAuthentication,
}

#[derive(Clone)]
pub struct OAuth2ClientCredentials {
    key: TokenKey,
    client: Client,
}

struct CachedToken {
    value: String,
    expires: Instant,
}

pub fn connection_fields() -> Vec<Value> {
    vec![
        json!({"name":"auth_type","label":"Authentication","input_type":"select","required":false,"secret":false,"editable":true,"group":"authentication","placeholder":"","options":[{"value":"none","label":"None"},{"value":"basic","label":"Basic"},{"value":"bearer","label":"Bearer token"},{"value":"oauth2","label":"OAuth 2.0 client credentials"}]}),
        json!({"name":"username","label":"Username","input_type":"text","required":true,"secret":true,"editable":true,"group":"credentials","placeholder":"","visible_when":{"field":"auth_type","value":"basic"}}),
        json!({"name":"password","label":"Password","input_type":"password","required":false,"secret":true,"editable":true,"group":"credentials","placeholder":"","visible_when":{"field":"auth_type","value":"basic"}}),
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
                let client = crate::data_connectors::shared_http_client(Duration::from_secs(10))
                    .map_err(|_| invalid())?;
                Ok(Self::OAuth2(OAuth2ClientCredentials {
                    key: TokenKey {
                        endpoint,
                        client_id: credential_setting(config, "oauth_client_id")?
                            .ok_or_else(invalid)?,
                        client_secret: credential_setting(config, "oauth_client_secret")?
                            .ok_or_else(invalid)?,
                        scopes: optional("oauth_scopes")?,
                        audience: optional("oauth_audience")?,
                        client_authentication,
                    },
                    client,
                }))
            }
            _ => Err(invalid()),
        }
    }

    pub fn public_configuration(&self) -> ConnectionConfig {
        let mut configuration = ConnectionConfig::new();
        let mode = match self {
            Self::None => "none",
            Self::Basic { .. } => "basic",
            Self::Bearer(_) => "bearer",
            Self::OAuth2(flow) => {
                configuration.insert(
                    "oauth_token_url".to_owned(),
                    json!(flow.key.endpoint.as_str()),
                );
                configuration.insert(
                    "oauth_client_auth".to_owned(),
                    json!(match flow.key.client_authentication {
                        ClientAuthentication::Basic => "basic",
                        ClientAuthentication::Body => "body",
                    }),
                );
                for (name, value) in [
                    ("oauth_scopes", &flow.key.scopes),
                    ("oauth_audience", &flow.key.audience),
                ] {
                    if let Some(value) = value {
                        configuration.insert(name.to_owned(), json!(value));
                    }
                }
                "oauth2"
            }
        };
        configuration.insert("auth_type".to_owned(), json!(mode));
        configuration
    }

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

    pub fn invalidate(&self) {
        if let Self::OAuth2(flow) = self {
            if let Ok(mut tokens) = token_cache().lock() {
                tokens.remove(&flow.cache_key());
            }
        }
    }
}

fn token_endpoint(raw: &str) -> Option<Url> {
    let url = Url::parse(raw).ok()?;
    let loopback = match url.host()? {
        url::Host::Domain(domain) => domain.eq_ignore_ascii_case("localhost"),
        url::Host::Ipv4(address) => address.is_loopback(),
        url::Host::Ipv6(address) => address.is_loopback(),
    };
    (url.scheme() == "https" || (url.scheme() == "http" && loopback))
        .then_some(())
        .filter(|_| {
            url.username().is_empty() && url.password().is_none() && url.fragment().is_none()
        })
        .map(|_| url)
}

fn bearer_header(token: &str) -> Result<header::HeaderValue, TokenError> {
    let mut value =
        header::HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| TokenError)?;
    value.set_sensitive(true);
    Ok(value)
}

fn token_cache() -> &'static Mutex<HashMap<[u8; 32], CachedToken>> {
    static TOKENS: OnceLock<Mutex<HashMap<[u8; 32], CachedToken>>> = OnceLock::new();
    TOKENS.get_or_init(Default::default)
}

impl OAuth2ClientCredentials {
    fn cache_key(&self) -> [u8; 32] {
        let mut digest = Sha256::new();
        for value in [
            self.key.endpoint.as_str(),
            &self.key.client_id,
            &self.key.client_secret,
            self.key.scopes.as_deref().unwrap_or(""),
            self.key.audience.as_deref().unwrap_or(""),
            match self.key.client_authentication {
                ClientAuthentication::Basic => "basic",
                ClientAuthentication::Body => "body",
            },
        ] {
            digest.update((value.len() as u64).to_be_bytes());
            digest.update(value.as_bytes());
        }
        digest.finalize().into()
    }

    async fn access_token(&self) -> Result<String, TokenError> {
        let now = Instant::now();
        let cache_key = self.cache_key();
        if let Some(token) = token_cache()
            .lock()
            .map_err(|_| TokenError)?
            .get(&cache_key)
            .filter(|token| token.expires > now)
        {
            return Ok(token.value.clone());
        }
        let (value, lifetime) = self.request_token().await?;
        let mut tokens = token_cache().lock().map_err(|_| TokenError)?;
        tokens.retain(|_, token| token.expires > Instant::now());
        if tokens.len() >= MAX_CACHED_TOKENS {
            tokens.clear();
        }
        tokens.insert(
            cache_key,
            CachedToken {
                value: value.clone(),
                expires: Instant::now() + lifetime,
            },
        );
        Ok(value)
    }

    async fn request_token(&self) -> Result<(String, Duration), TokenError> {
        let mut form = vec![("grant_type", "client_credentials")];
        if let Some(scopes) = self.key.scopes.as_deref() {
            form.push(("scope", scopes));
        }
        if let Some(audience) = self.key.audience.as_deref() {
            form.push(("audience", audience));
        }
        let mut request = self
            .client
            .post(self.key.endpoint.clone())
            .header(header::ACCEPT, "application/json");
        request = match self.key.client_authentication {
            ClientAuthentication::Basic => request.basic_auth(
                form_encode(&self.key.client_id),
                Some(form_encode(&self.key.client_secret)),
            ),
            ClientAuthentication::Body => {
                form.push(("client_id", &self.key.client_id));
                form.push(("client_secret", &self.key.client_secret));
                request
            }
        };
        let response = request.form(&form).send().await.map_err(|_| TokenError)?;
        let body: Value = serde_json::from_slice(&read_token_response(response).await?)
            .map_err(|_| TokenError)?;
        let token = body
            .get("access_token")
            .and_then(Value::as_str)
            .filter(|token| {
                !token.is_empty()
                    && token.len() <= MAX_ACCESS_TOKEN_BYTES
                    && token.bytes().all(|byte| byte.is_ascii_graphic())
            })
            .ok_or(TokenError)?;
        if body.get("token_type").is_some_and(|kind| {
            !kind
                .as_str()
                .is_some_and(|kind| kind.eq_ignore_ascii_case("bearer"))
        }) {
            return Err(TokenError);
        }
        let expires_in = match body.get("expires_in") {
            Some(Value::Number(seconds)) => seconds.as_u64(),
            Some(Value::String(seconds)) => seconds.trim().parse::<u64>().ok(),
            _ => None,
        };
        let lifetime = match expires_in {
            None => Duration::from_secs(60),
            Some(seconds) => {
                let seconds = seconds.min(MAX_TOKEN_LIFETIME_SECONDS);
                Duration::from_secs(if seconds > 60 {
                    seconds - 30
                } else {
                    seconds / 2
                })
            }
        };
        Ok((token.to_owned(), lifetime))
    }
}

fn form_encode(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

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

    fn oauth(token_url: &str, secret: &str, client_auth: &str) -> HttpAuth {
        let value = json!({"auth_type":"oauth2","oauth_token_url":token_url,"oauth_client_id":"reader app","oauth_client_secret":secret,"oauth_scopes":"logs:read","oauth_client_auth":client_auth});
        HttpAuth::from_config(value.as_object().unwrap()).unwrap()
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
    fn token_endpoints_require_tls_except_on_loopback() {
        for accepted in [
            "https://idp.example/token",
            "http://localhost:8080/token",
            "http://127.0.0.1/token",
            "http://[::1]/token",
        ] {
            assert!(token_endpoint(accepted).is_some());
        }
        for rejected in [
            "http://idp.example/token",
            "http://10.0.0.5/token",
            "http://localhost.example/token",
            "https://user:pw@idp/token",
            "https://idp/token#fragment",
            "ftp://idp/token",
        ] {
            assert!(token_endpoint(rejected).is_none());
        }
    }

    #[tokio::test]
    async fn tokens_are_cached_and_invalidated_without_exposing_credentials() {
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
        let public = Value::Object(auth.public_configuration()).to_string();
        assert!(!public.contains("s&cret") && !public.contains("reader app"));
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
    async fn body_auth_and_invalid_token_responses() {
        let server = MockServer::start().await;
        Mock::given(matchers::method("POST"))
            .and(matchers::path("/token"))
            .and(matchers::body_string("grant_type=client_credentials&scope=logs%3Aread&client_id=reader+app&client_secret=secret"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"access_token":"body-token"})))
            .expect(1).mount(&server).await;
        assert_eq!(
            authorization(&oauth(&format!("{}/token", server.uri()), "secret", "body"))
                .await
                .as_deref(),
            Some("Bearer body-token")
        );

        for (path, response) in [
            (
                "/denied",
                ResponseTemplate::new(401).set_body_string("secret upstream detail"),
            ),
            (
                "/type",
                ResponseTemplate::new(200)
                    .set_body_json(json!({"access_token":"value","token_type":"mac"})),
            ),
            (
                "/invalid-type",
                ResponseTemplate::new(200)
                    .set_body_json(json!({"access_token":"value","token_type":42})),
            ),
            (
                "/missing",
                ResponseTemplate::new(200).set_body_json(json!({"token_type":"Bearer"})),
            ),
            (
                "/unsafe",
                ResponseTemplate::new(200).set_body_json(json!({"access_token":"bad\ntoken"})),
            ),
            (
                "/large",
                ResponseTemplate::new(200)
                    .set_body_string("x".repeat(MAX_TOKEN_RESPONSE_BYTES + 1)),
            ),
        ] {
            Mock::given(matchers::path(path))
                .respond_with(response)
                .mount(&server)
                .await;
            assert!(authorization(&oauth(
                &format!("{}{path}", server.uri()),
                "secret",
                "basic"
            ))
            .await
            .is_none());
        }
    }
}
