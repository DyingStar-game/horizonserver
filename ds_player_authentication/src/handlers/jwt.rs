//! Checks the Keycloak access token a player presents in `player/init`.
//!
//! Keycloak signs the token (RS256 by default) with a key of the realm, published
//! as a JWKS at `<issuer>/protocol/openid-connect/certs`. The keys are fetched
//! once, cached, and fetched again when a token names a key we do not hold (a key
//! rotation) or when the cache is older than an hour (a key removed from the
//! realm must stop being accepted) — never more than once every
//! `JWKS_MIN_REFETCH`, so a stream of forged tokens with random `kid`s cannot
//! hammer Keycloak.
//!
//! Configuration (environment, read once when the plugin starts):
//! * `VERIFY_JWT_SIGNATURE` — `false` (or `0`, `no`, `off`) keeps the old
//!   decode-only behaviour, for local development: anyone can then forge an
//!   identity and roles. Verification is on when unset.
//! * `OIDC_ISSUER` — the realm's issuer, exactly the `iss` of its tokens, e.g.
//!   `https://auth-preprod.dyingstar-game.com/realms/dyingstar`. The same variable
//!   the other services of the cluster read. Required when verifying: without it
//!   every login is rejected (fail closed).
//! * `OIDC_JWKS_URL` — optional, where to fetch the keys from (an in-cluster
//!   Keycloak URL, for instance). Defaults to the issuer's certs endpoint.
//!
//! No audience check: the launcher is a public client and its access tokens have
//! no stable `aud` (`account`, or none).

use async_trait::async_trait;
use jsonwebtoken::jwk::{AlgorithmParameters, Jwk, PublicKeyUse};
use jsonwebtoken::errors::ErrorKind;
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use tracing::{error, info, warn};

use super::authentication::JwtClaims;

/// Why a player token was refused. An expired one is a real session that only needs a
/// fresh token (the launcher refreshes it at every game launch): the client is told so.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenRejection {
    Expired,
    Invalid(String),
}

impl std::fmt::Display for TokenRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TokenRejection::Expired => write!(f, "token expired"),
            TokenRejection::Invalid(reason) => write!(f, "{}", reason),
        }
    }
}

/// Least time between two JWKS fetches.
pub const JWKS_MIN_REFETCH: Duration = Duration::from_secs(10);
/// Age past which the cached keys are fetched again before use.
pub const JWKS_MAX_AGE: Duration = Duration::from_secs(3600);
/// Timeout of one JWKS request.
const JWKS_HTTP_TIMEOUT: Duration = Duration::from_secs(5);

/// The signature algorithms accepted: the RSA family Keycloak signs with. Never
/// HS*: a public RSA key used as an HMAC secret would let anyone sign.
const ACCEPTED_ALGORITHMS: [Algorithm; 6] = [
    Algorithm::RS256,
    Algorithm::RS384,
    Algorithm::RS512,
    Algorithm::PS256,
    Algorithm::PS384,
    Algorithm::PS512,
];

/// Where the JWKS comes from: Keycloak over HTTP, or a fixed set in the tests.
#[async_trait]
pub trait JwksSource: Send + Sync {
    /// The JWKS document (`{"keys": [...]}`).
    async fn fetch(&self) -> Result<serde_json::Value, String>;
    /// For the logs.
    fn describe(&self) -> String;
}

/// The realm's certs endpoint.
pub struct HttpJwks {
    url: String,
    client: reqwest::Client,
}

impl HttpJwks {
    pub fn new(url: String) -> Result<Self, String> {
        // rustls with the bundled Mozilla roots: the runtime image has no
        // ca-certificates, and native-tls would find no root to trust.
        let client = reqwest::Client::builder()
            .use_rustls_tls()
            .timeout(JWKS_HTTP_TIMEOUT)
            .build()
            .map_err(|e| format!("cannot build the JWKS HTTP client: {}", e))?;
        Ok(Self { url, client })
    }
}

#[async_trait]
impl JwksSource for HttpJwks {
    async fn fetch(&self) -> Result<serde_json::Value, String> {
        let response = self.client.get(&self.url).send().await.map_err(|e| e.to_string())?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!("HTTP {}", status));
        }
        let body = response.bytes().await.map_err(|e| e.to_string())?;
        serde_json::from_slice(&body).map_err(|e| format!("not a JSON document: {}", e))
    }

    fn describe(&self) -> String {
        self.url.clone()
    }
}

/// Keeps the signing keys of the realm: `kid` -> key.
struct KeyCache {
    keys: HashMap<String, DecodingKey>,
    /// Last successful fetch.
    fetched_at: Option<Instant>,
    /// Last fetch attempt, successful or not (the rate limit).
    attempted_at: Option<Instant>,
}

/// Verifies the signature, `exp` and `iss` of a token against the realm's keys.
pub struct JwtVerifier {
    issuer: String,
    source: Box<dyn JwksSource>,
    cache: RwLock<KeyCache>,
    /// Serialises the fetches: the inits waiting for one find its keys after it.
    fetch_lock: tokio::sync::Mutex<()>,
    min_refetch: Duration,
    max_age: Duration,
}

impl JwtVerifier {
    pub fn new(issuer: String, source: Box<dyn JwksSource>) -> Self {
        Self::with_timings(issuer, source, JWKS_MIN_REFETCH, JWKS_MAX_AGE)
    }

    pub fn with_timings(issuer: String, source: Box<dyn JwksSource>, min_refetch: Duration, max_age: Duration) -> Self {
        Self {
            issuer,
            source,
            cache: RwLock::new(KeyCache { keys: HashMap::new(), fetched_at: None, attempted_at: None }),
            fetch_lock: tokio::sync::Mutex::new(()),
            min_refetch,
            max_age,
        }
    }

    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    /// The claims of `token` once its signature, `exp` and `iss` are checked.
    pub async fn verify(&self, token: &str) -> Result<JwtClaims, TokenRejection> {
        self.verify_inner(token).await.map_err(|(expired, reason)| {
            if expired { TokenRejection::Expired } else { TokenRejection::Invalid(reason) }
        })
    }

    /// [`Self::verify`], with the rejection as (expired?, reason).
    async fn verify_inner(&self, token: &str) -> Result<JwtClaims, (bool, String)> {
        let invalid = |reason: String| (false, reason);
        let header = decode_header(token).map_err(|e| invalid(format!("malformed token header: {}", e)))?;
        if !ACCEPTED_ALGORITHMS.contains(&header.alg) {
            return Err(invalid(format!("signature algorithm {:?} not accepted", header.alg)));
        }
        let kid = header.kid.ok_or_else(|| invalid("token header has no kid".to_string()))?;

        let key = match self.key(&kid) {
            Some((key, fresh)) if fresh => key,
            _ => {
                self.refresh(&kid).await;
                self.key(&kid)
                    .map(|(key, _)| key)
                    .ok_or_else(|| invalid(format!("unknown signing key kid={} (not in the JWKS of {})", kid, self.source.describe())))?
            }
        };

        let mut validation = Validation::new(header.alg);
        validation.algorithms = ACCEPTED_ALGORITHMS.to_vec();
        validation.set_issuer(&[self.issuer.as_str()]);
        validation.set_required_spec_claims(&["exp", "iss"]);
        validation.validate_exp = true;
        validation.validate_aud = false;
        decode::<JwtClaims>(token, &key, &validation)
            .map(|data| data.claims)
            .map_err(|e| (*e.kind() == ErrorKind::ExpiredSignature, format!("token rejected: {}", e)))
    }

    /// The cached key `kid`, and whether the cache is still fresh.
    fn key(&self, kid: &str) -> Option<(DecodingKey, bool)> {
        let cache = self.cache.read().unwrap();
        let fresh = cache.fetched_at.map_or(false, |at| at.elapsed() < self.max_age);
        cache.keys.get(kid).map(|key| (key.clone(), fresh))
    }

    /// Fetches the JWKS again, unless another init just did (it may have brought
    /// `kid`) or the last attempt is more recent than `min_refetch`. A failed fetch
    /// keeps the keys already held.
    async fn refresh(&self, kid: &str) {
        let _guard = self.fetch_lock.lock().await;
        {
            let cache = self.cache.read().unwrap();
            let fresh = cache.fetched_at.map_or(false, |at| at.elapsed() < self.max_age);
            if fresh && cache.keys.contains_key(kid) {
                return;
            }
            if let Some(at) = cache.attempted_at {
                if at.elapsed() < self.min_refetch {
                    warn!("auth (jwks): kid={} not held, JWKS fetched {:?} ago, not fetched again yet", kid, at.elapsed());
                    return;
                }
            }
        }
        self.cache.write().unwrap().attempted_at = Some(Instant::now());
        match self.source.fetch().await {
            Ok(document) => {
                let keys = signing_keys(&document);
                if keys.is_empty() {
                    error!("auth (jwks): no RSA signing key in the JWKS of {}", self.source.describe());
                    return;
                }
                info!("auth (jwks): {} signing key(s) fetched from {}: {:?}", keys.len(), self.source.describe(), keys.keys().collect::<Vec<_>>());
                let mut cache = self.cache.write().unwrap();
                cache.keys = keys;
                cache.fetched_at = Some(Instant::now());
            }
            Err(e) => error!("auth (jwks): fetching {} failed: {}", self.source.describe(), e),
        }
    }
}

/// The RSA signature keys of a JWKS, by `kid`. Keycloak also publishes its
/// encryption keys (`use: enc`, `RSA-OAEP`): skipped, like anything unreadable.
fn signing_keys(document: &serde_json::Value) -> HashMap<String, DecodingKey> {
    let mut keys = HashMap::new();
    for value in document["keys"].as_array().into_iter().flatten() {
        let Ok(jwk) = serde_json::from_value::<Jwk>(value.clone()) else {
            continue;
        };
        if matches!(jwk.common.public_key_use, Some(PublicKeyUse::Encryption)) {
            continue;
        }
        if !matches!(jwk.algorithm, AlgorithmParameters::RSA(_)) {
            continue;
        }
        let Some(kid) = jwk.common.key_id.clone() else {
            continue;
        };
        if let Ok(key) = DecodingKey::from_jwk(&jwk) {
            keys.insert(kid, key);
        }
    }
    keys
}

/// How the init checks the token, from the environment.
pub enum TokenCheck {
    /// Signature, `exp` and `iss` verified.
    Verify(Arc<JwtVerifier>),
    /// `VERIFY_JWT_SIGNATURE=false`: payload decoded, nothing verified.
    DecodeOnly,
    /// Verification on but not configured: every login is rejected with this reason.
    Misconfigured(String),
}

/// Whether `VERIFY_JWT_SIGNATURE` turns the verification off.
fn verification_disabled(value: Option<String>) -> bool {
    value.map_or(false, |v| matches!(v.trim().to_ascii_lowercase().as_str(), "false" | "0" | "no" | "off"))
}

impl TokenCheck {
    pub fn from_env() -> Self {
        let env = |name: &str| std::env::var(name).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        Self::from_values(env("VERIFY_JWT_SIGNATURE"), env("OIDC_ISSUER"), env("OIDC_JWKS_URL"))
    }

    fn from_values(verify: Option<String>, issuer: Option<String>, jwks_url: Option<String>) -> Self {
        if verification_disabled(verify) {
            return TokenCheck::DecodeOnly;
        }
        let Some(issuer) = issuer.map(|i| i.trim_end_matches('/').to_string()) else {
            return TokenCheck::Misconfigured("OIDC_ISSUER is not set (or set VERIFY_JWT_SIGNATURE=false for development)".to_string());
        };
        let url = jwks_url.unwrap_or_else(|| format!("{}/protocol/openid-connect/certs", issuer));
        match HttpJwks::new(url) {
            Ok(source) => TokenCheck::Verify(Arc::new(JwtVerifier::new(issuer, Box::new(source)))),
            Err(e) => TokenCheck::Misconfigured(e),
        }
    }

    /// Says at startup how tokens are checked (called from `on_init`, once the
    /// plugin's logging is set up: `register_handlers` runs before it).
    pub fn log_startup(&self) {
        match self {
            TokenCheck::Verify(verifier) => info!(
                "auth: player tokens verified against issuer {} (keys from {})",
                verifier.issuer, verifier.source.describe()
            ),
            TokenCheck::DecodeOnly => warn!(
                "⚠️ VERIFY_JWT_SIGNATURE=false: player tokens are NOT verified, anyone can forge an identity and roles. Development only."
            ),
            TokenCheck::Misconfigured(reason) => error!(
                "❌ auth: JWT verification is on but {}: every player login will be rejected", reason
            ),
        }
    }
}

/// The realm roles of the token (`realm_access.roles`); none when absent.
pub fn realm_roles(claims: &JwtClaims) -> Vec<String> {
    claims
        .extra
        .get("realm_access")
        .and_then(|access| access.get("roles"))
        .and_then(|roles| roles.as_array())
        .map(|roles| roles.iter().filter_map(|r| r.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{encode, EncodingKey, Header};
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    const ISSUER: &str = "https://auth.test/realms/dyingstar";
    const KEY_A_PEM: &str = include_str!("testdata/rsa_a.pem");
    const KEY_A_N: &str = "k9EIuXrUg1ai9s83waJ74WqJDPhu61JVZvlw5QHRhnX8wK1_7V8aY4BaG3uPYlM3sGJrMc3HtuCeHIEtvaRSy4GYh3KyhnyPr3Sxj9KchCG651iceeMjMqsN0ZXRAnS9E-pmcruLBtLLP1ezqf_vqZDUwC0n3bsC2zvnnUCJSgN03jIgsm30pZxd0W5upvFRXvygDySDytkdy1AIPs-xElf2c7U-xB3qkU3tGiPZJV7jUkk6V-MUT3xpdAxofDg5Wv4ivg6nRdfL0x4zO87zZPHhVLfr152k6_wLb45FGUTo_BHTGZMaSRASkR93boYnuxbfrpmLh5Ui2qG3B1tLfw";
    const KEY_B_PEM: &str = include_str!("testdata/rsa_b.pem");
    const KEY_B_N: &str = "omLTyaLbpbdkhnCVT7LFFbvFR7aIT6isUW1yRbJsyDEm-1m_eOvBv3rdaEIuHB4pknOi3W4YUql-dKlzp4qRTzxdr9efCRH0ePjHFlTefR6lnDnN8-nFPVURXMUJO4t7zkdaz5jlwE7joKVaxQF0Dv9Y0lvEJxlHRwInuLR7YmuZjUx6nwzxYxMlVw8ZoguCdxKDV-ozRp3fosdQIQ01FvjzX_LsOVN1NDtdNx1S2qjbkmm0yN4_2tEhR4rFUuxuR1V8Fo3tBEr7OG857B05koMkZfW-LSgRH1J5-_JtAOPz1CGhJdepowcOx3Ae9294VcdwFW35FjF4GxHD9-JzsQ";

    fn jwk(kid: &str, n: &str) -> serde_json::Value {
        json!({"kid": kid, "kty": "RSA", "alg": "RS256", "use": "sig", "n": n, "e": "AQAB"})
    }

    /// Keycloak's JWKS also carries an encryption key, which must be skipped.
    fn enc_jwk() -> serde_json::Value {
        json!({"kid": "enc-key", "kty": "RSA", "alg": "RSA-OAEP", "use": "enc", "n": KEY_B_N, "e": "AQAB"})
    }

    struct FixedJwks {
        document: Mutex<serde_json::Value>,
        fetches: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl JwksSource for Arc<FixedJwks> {
        async fn fetch(&self) -> Result<serde_json::Value, String> {
            self.fetches.fetch_add(1, Ordering::SeqCst);
            Ok(self.document.lock().unwrap().clone())
        }
        fn describe(&self) -> String {
            "test JWKS".to_string()
        }
    }

    fn verifier(keys: Vec<serde_json::Value>, min_refetch: Duration) -> (JwtVerifier, Arc<FixedJwks>) {
        let source = Arc::new(FixedJwks { document: Mutex::new(json!({ "keys": keys })), fetches: Arc::new(AtomicUsize::new(0)) });
        let verifier = JwtVerifier::with_timings(ISSUER.to_string(), Box::new(Arc::clone(&source)), min_refetch, JWKS_MAX_AGE);
        (verifier, source)
    }

    fn now() -> i64 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
    }

    fn claims(iss: &str, exp: i64) -> serde_json::Value {
        json!({
            "sub": "35941259-8205-4d46-b8a1-8837306d1587",
            "preferred_username": "ann",
            "iss": iss,
            "aud": "account",
            "exp": exp,
            "iat": now() - 10,
            "realm_access": {"roles": ["default-roles-dyingstar", "admin"]},
        })
    }

    fn sign(pem: &str, kid: &str, payload: &serde_json::Value) -> String {
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some(kid.to_string());
        encode(&header, payload, &EncodingKey::from_rsa_pem(pem.as_bytes()).unwrap()).unwrap()
    }

    #[tokio::test]
    async fn a_valid_token_is_accepted_with_its_roles() {
        let (verifier, source) = verifier(vec![jwk("a", KEY_A_N), enc_jwk()], JWKS_MIN_REFETCH);
        let token = sign(KEY_A_PEM, "a", &claims(ISSUER, now() + 300));
        let claims = verifier.verify(&token).await.expect("valid token");
        assert_eq!(claims.sub.as_deref(), Some("35941259-8205-4d46-b8a1-8837306d1587"));
        assert_eq!(claims.extra["preferred_username"], json!("ann"));
        assert_eq!(realm_roles(&claims), vec!["default-roles-dyingstar", "admin"]);
        // The second token is checked against the cached keys.
        verifier.verify(&token).await.expect("valid token");
        assert_eq!(source.fetches.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_token_signed_by_another_key_is_rejected() {
        let (verifier, _) = verifier(vec![jwk("a", KEY_A_N)], JWKS_MIN_REFETCH);
        // Claims the kid of the realm's key, signed with a key of the attacker's.
        let token = sign(KEY_B_PEM, "a", &claims(ISSUER, now() + 300));
        assert!(verifier.verify(&token).await.is_err());
    }

    #[tokio::test]
    async fn a_tampered_payload_is_rejected() {
        let (verifier, _) = verifier(vec![jwk("a", KEY_A_N)], JWKS_MIN_REFETCH);
        let token = sign(KEY_A_PEM, "a", &claims(ISSUER, now() + 300));
        let mut forged = claims(ISSUER, now() + 300);
        forged["realm_access"]["roles"] = json!(["supervisor"]);
        let forged_payload = base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, forged.to_string());
        let parts: Vec<&str> = token.split('.').collect();
        let tampered = format!("{}.{}.{}", parts[0], forged_payload, parts[2]);
        assert!(verifier.verify(&tampered).await.is_err());
    }

    #[tokio::test]
    async fn an_expired_token_is_rejected() {
        let (verifier, _) = verifier(vec![jwk("a", KEY_A_N)], JWKS_MIN_REFETCH);
        // Past the 60 s leeway.
        let token = sign(KEY_A_PEM, "a", &claims(ISSUER, now() - 600));
        assert_eq!(verifier.verify(&token).await.unwrap_err(), TokenRejection::Expired);
    }

    #[tokio::test]
    async fn a_token_of_another_issuer_is_rejected() {
        let (verifier, _) = verifier(vec![jwk("a", KEY_A_N)], JWKS_MIN_REFETCH);
        let token = sign(KEY_A_PEM, "a", &claims("https://auth.test/realms/other", now() + 300));
        let err = verifier.verify(&token).await.unwrap_err();
        assert!(err.to_string().contains("InvalidIssuer"), "{}", err);
    }

    #[tokio::test]
    async fn an_unsigned_or_hmac_token_is_rejected() {
        let (verifier, _) = verifier(vec![jwk("a", KEY_A_N)], JWKS_MIN_REFETCH);
        let mut header = Header::new(Algorithm::HS256);
        header.kid = Some("a".to_string());
        let token = encode(&header, &claims(ISSUER, now() + 300), &EncodingKey::from_secret(b"secret")).unwrap();
        assert!(verifier.verify(&token).await.is_err());
        assert!(verifier.verify("not-a-jwt").await.is_err());
    }

    #[tokio::test]
    async fn an_unknown_kid_fetches_the_keys_again() {
        let (verifier, source) = verifier(vec![jwk("a", KEY_A_N)], Duration::ZERO);
        verifier.verify(&sign(KEY_A_PEM, "a", &claims(ISSUER, now() + 300))).await.expect("key a");
        assert_eq!(source.fetches.load(Ordering::SeqCst), 1);

        // Keycloak rotated its key: the new kid is fetched.
        *source.document.lock().unwrap() = json!({ "keys": [jwk("a", KEY_A_N), jwk("b", KEY_B_N)] });
        verifier.verify(&sign(KEY_B_PEM, "b", &claims(ISSUER, now() + 300))).await.expect("key b after a refetch");
        assert_eq!(source.fetches.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn unknown_kids_are_fetched_at_most_once_per_interval() {
        let (verifier, source) = verifier(vec![jwk("a", KEY_A_N)], Duration::from_secs(3600));
        verifier.verify(&sign(KEY_A_PEM, "a", &claims(ISSUER, now() + 300))).await.expect("key a");
        for kid in ["x", "y", "z"] {
            assert!(verifier.verify(&sign(KEY_B_PEM, kid, &claims(ISSUER, now() + 300))).await.is_err());
        }
        assert_eq!(source.fetches.load(Ordering::SeqCst), 1, "rate limited");
    }

    #[test]
    fn roles_absent_are_empty() {
        let claims: JwtClaims = serde_json::from_value(json!({"sub": "x"})).unwrap();
        assert!(realm_roles(&claims).is_empty());
        let claims: JwtClaims = serde_json::from_value(json!({"realm_access": {"roles": ["admin", 3]}})).unwrap();
        assert_eq!(realm_roles(&claims), vec!["admin"]);
    }

    #[test]
    fn configuration_from_the_environment() {
        assert!(matches!(TokenCheck::from_values(Some("false".into()), None, None), TokenCheck::DecodeOnly));
        assert!(matches!(TokenCheck::from_values(Some("OFF".into()), Some(ISSUER.into()), None), TokenCheck::DecodeOnly));
        assert!(matches!(TokenCheck::from_values(None, None, None), TokenCheck::Misconfigured(_)), "fails closed");
        assert!(matches!(TokenCheck::from_values(Some("true".into()), None, None), TokenCheck::Misconfigured(_)));
        match TokenCheck::from_values(None, Some(format!("{}/", ISSUER)), None) {
            TokenCheck::Verify(v) => assert_eq!(v.issuer(), ISSUER),
            _ => panic!("verification expected"),
        }
    }
}
