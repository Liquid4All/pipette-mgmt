//! Console sign-in through Clerk: the token a Clerk session issues from the JWT
//! template `[clerk]` names, verified offline.

use std::collections::HashSet;

use anyhow::Context;
use aws_lc_rs::signature::{ParsedPublicKey, RSA_PKCS1_2048_8192_SHA256};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD_INDIFFERENT, URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha512};

use pipette_core::config::ClerkConfig;
use pipette_pipeline::org::OrgRef;
use pipette_plan_types::ContactEmail;

use crate::auth::{ReplayCache, SignatureClaim, TIMESTAMP_TOLERANCE_SECS};
use crate::error::AppError;

/// Clock difference tolerated against Clerk's, the default of Clerk's own SDK.
const CLOCK_SKEW_SECS: i64 = 5;

/// The claims `verify` decides on, logged as Clerk rendered them, plus `sub`,
/// which finds the user in Clerk's dashboard. `email` is logged apart.
const LOGGED_CLAIMS: [&str; 9] = [
    "iss",
    "sub",
    "azp",
    "aud",
    "iat",
    "nbf",
    "exp",
    "act",
    "email_verified",
];

/// A `[clerk]` section, parsed once at startup so a bad key stops the server.
pub struct ClerkSignIn {
    publishable_key: String,
    frontend_api: String,
    key: ParsedPublicKey,
    authorized_parties: Vec<String>,
    jwt_template: String,
    allowed_domains: Vec<String>,
    provision: Vec<ProvisionRule>,
}

/// Addresses at `domain` that no account holds join `org` on first sign-in.
pub struct ProvisionRule {
    pub domain: String,
    pub org: OrgRef,
}

#[derive(Deserialize)]
struct Header {
    alg: String,
}

#[derive(Deserialize)]
struct Claims {
    iss: String,
    exp: i64,
    iat: i64,
    nbf: Option<i64>,
    azp: Option<String>,
    aud: Option<Audience>,
    /// Present when a Clerk admin impersonates the user.
    act: Option<serde_json::Value>,
    email: Option<String>,
    email_verified: Option<serde_json::Value>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Audience {
    One(String),
    Many(Vec<String>),
}

impl Audience {
    fn contains(&self, audience: &str) -> bool {
        match self {
            Self::One(one) => one == audience,
            Self::Many(many) => many.iter().any(|one| one == audience),
        }
    }
}

impl ClerkSignIn {
    pub fn new(config: &ClerkConfig) -> anyhow::Result<Self> {
        let frontend_api = frontend_api(&config.publishable_key)
            .context("[clerk] publishable_key is not a Clerk publishable key")?;
        let key = ParsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, pem_body(&config.public_key)?)
            .map_err(|e| anyhow::anyhow!("[clerk] public_key is not an RSA public key: {e}"))?;

        let authorized_parties: Vec<String> = config
            .authorized_parties
            .iter()
            .map(|origin| origin.trim().trim_end_matches('/').to_string())
            .collect();
        anyhow::ensure!(
            !authorized_parties.is_empty(),
            "[clerk] authorized_parties names no origin, so every token would be refused"
        );
        authorized_parties.iter().try_for_each(|origin| {
            let host = origin
                .strip_prefix("https://")
                .or_else(|| origin.strip_prefix("http://"));
            anyhow::ensure!(
                host.is_some_and(|host| !host.is_empty() && !host.contains('/')),
                "[clerk] authorized_parties entry {origin:?} is not an origin, such as \
                 https://console.example.test"
            );
            Ok(())
        })?;

        let jwt_template = config.jwt_template.trim().to_string();
        anyhow::ensure!(
            !jwt_template.is_empty(),
            "[clerk] jwt_template names no template, so every token would be refused"
        );

        let allowed_domains = config
            .allowed_domains
            .iter()
            .map(|domain| {
                normalize_domain(domain)
                    .with_context(|| format!("[clerk] allowed_domains entry {domain:?}"))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;

        let provision = config
            .provision
            .iter()
            .map(|rule| {
                let domain = normalize_domain(&rule.domain)
                    .with_context(|| format!("[clerk] provision domain {:?}", rule.domain))?;
                // A rule outside the allowlist would create accounts that cannot sign in.
                anyhow::ensure!(
                    allowed_domains.is_empty() || allowed_domains.contains(&domain),
                    "[clerk] provision domain {domain:?} is not in allowed_domains"
                );
                let org = rule
                    .org
                    .parse()
                    .with_context(|| format!("[clerk] provision org {:?}", rule.org))?;
                Ok(ProvisionRule { domain, org })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        let mut domains = HashSet::new();
        if let Some(twice) = provision.iter().find(|rule| !domains.insert(&rule.domain)) {
            anyhow::bail!("[clerk] provision names {} twice", twice.domain);
        }

        Ok(Self {
            publishable_key: config.publishable_key.trim().to_string(),
            frontend_api,
            key,
            authorized_parties,
            jwt_template,
            allowed_domains,
            provision,
        })
    }

    pub fn publishable_key(&self) -> &str {
        &self.publishable_key
    }

    /// The host ClerkJS is loaded from, and the issuer its tokens name.
    pub fn frontend_api(&self) -> &str {
        &self.frontend_api
    }

    /// The JWT template the sign-in page asks Clerk for, and the `aud` a token
    /// must carry.
    pub fn jwt_template(&self) -> &str {
        &self.jwt_template
    }

    /// Whether `host`, a request's `Host`, is one of `authorized_parties`: a
    /// page served at any other gets tokens `verify` refuses.
    pub fn authorizes_host(&self, host: &str) -> bool {
        self.authorized_parties.iter().any(|origin| {
            origin
                .split_once("://")
                .is_some_and(|(_, authority)| authority.eq_ignore_ascii_case(host))
        })
    }

    /// Whether `email` may sign in through Clerk at all: its domain is in
    /// `allowed_domains`, or the list is empty.
    pub fn admits(&self, email: &ContactEmail) -> bool {
        self.allowed_domains.is_empty()
            || email.as_str().rsplit_once('@').is_some_and(|(_, domain)| {
                self.allowed_domains
                    .iter()
                    .any(|allowed| allowed.eq_ignore_ascii_case(domain))
            })
    }

    /// The domains `admits` accepts; empty accepts any.
    pub fn allowed_domains(&self) -> &[String] {
        &self.allowed_domains
    }

    /// The rule for `email`'s domain, if an account is created for it.
    pub fn provision_rule(&self, email: &ContactEmail) -> Option<&ProvisionRule> {
        let (_, domain) = email.as_str().rsplit_once('@')?;
        self.provision
            .iter()
            .find(|rule| rule.domain.eq_ignore_ascii_case(domain))
    }

    /// The verified address `jwt` signs in as. Spends the token, so it signs in once.
    pub fn verify(
        &self,
        jwt: &str,
        spent: &ReplayCache,
        now: DateTime<Utc>,
    ) -> Result<ContactEmail, AppError> {
        let malformed = || refuse("the token is not a JWT");
        let (signed, signature) = jwt.rsplit_once('.').ok_or_else(malformed)?;
        let (header, payload) = signed.split_once('.').ok_or_else(malformed)?;
        let header: Header = decode(header).ok_or_else(malformed)?;
        // Pinned rather than taken from the token, which could name `none`.
        if header.alg != "RS256" {
            return Err(refuse("the token is not signed with RS256"));
        }
        let signature = URL_SAFE_NO_PAD.decode(signature).map_err(|_| malformed())?;
        self.key
            .verify_sig(signed.as_bytes(), &signature)
            .map_err(|_| refuse("the token's signature does not verify"))?;
        // Logged only now, so the line holds what Clerk signed, not a caller's text.
        let claims: serde_json::Map<String, serde_json::Value> =
            decode(payload).ok_or_else(malformed)?;
        log_claims(&claims);
        let claims: Claims =
            serde_json::from_value(serde_json::Value::Object(claims)).map_err(|_| malformed())?;

        if claims.iss != format!("https://{}", self.frontend_api) {
            return Err(refuse("the token is from another Clerk instance"));
        }
        let now_secs = now.timestamp();
        if now_secs >= claims.exp.saturating_add(CLOCK_SKEW_SECS) {
            return Err(refuse("the token has expired"));
        }
        let skewed = now_secs.saturating_add(CLOCK_SKEW_SECS);
        if claims.iat > skewed || claims.nbf.is_some_and(|nbf| nbf > skewed) {
            return Err(refuse("the token is not valid yet"));
        }
        // Whatever lifetime the template sets, so the replay record outlives the token.
        if now_secs.saturating_sub(claims.iat) > TIMESTAMP_TOLERANCE_SECS {
            return Err(refuse("the token was issued more than five minutes ago"));
        }
        if !claims
            .azp
            .is_some_and(|azp| self.authorized_parties.contains(&azp))
        {
            return Err(refuse("the token was issued to another origin"));
        }
        if !claims
            .aud
            .is_some_and(|aud| aud.contains(&self.jwt_template))
        {
            return Err(refuse(&format!(
                "the token is not from the {} JWT template",
                self.jwt_template
            )));
        }
        if claims.act.is_some() {
            return Err(refuse("an impersonated Clerk session signs nobody in"));
        }
        // Clerk renders the shortcode as a boolean; the string "true" is taken too.
        let verified = match claims.email_verified {
            Some(serde_json::Value::Bool(verified)) => verified,
            Some(serde_json::Value::String(verified)) => verified == "true",
            _ => false,
        };
        let Some(email) = claims.email.filter(|_| verified) else {
            return Err(refuse("the token carries no verified email address"));
        };
        // Folded, as the account index keys it.
        let email = ContactEmail::try_new(email.to_lowercase())
            .map_err(|_| refuse("the token's email address is not one an account can hold"))?;

        let issued = DateTime::from_timestamp(claims.iat, 0).ok_or_else(malformed)?;
        match spent.claim(&Sha512::digest(&signature).into(), issued, now) {
            SignatureClaim::Fresh => Ok(email),
            SignatureClaim::Replayed => Err(refuse("the token was already used")),
            SignatureClaim::Expired => {
                Err(refuse("the token was issued more than five minutes ago"))
            }
        }
    }
}

fn refuse(reason: &str) -> AppError {
    tracing::warn!("refused a Clerk sign-in: {reason}");
    AppError::Unauthorized(reason.to_string())
}

/// The address is cut to its domain, since no log names one.
fn log_claims(claims: &serde_json::Map<String, serde_json::Value>) {
    let mut logged: serde_json::Map<String, serde_json::Value> = LOGGED_CLAIMS
        .iter()
        .filter_map(|name| Some(((*name).to_string(), claims.get(*name)?.clone())))
        .collect();
    if let Some(email) = claims.get("email") {
        let shown = match email.as_str().and_then(|email| email.rsplit_once('@')) {
            Some((_, domain)) => serde_json::Value::String(format!("*@{domain}")),
            None => email.clone(),
        };
        logged.insert("email".to_string(), shown);
    }
    tracing::info!(claims = %serde_json::Value::Object(logged), "checking a Clerk token's claims");
}

/// `domain` folded, without a leading `@`, or an error if it is not a bare domain.
fn normalize_domain(domain: &str) -> anyhow::Result<String> {
    let domain = domain.trim().trim_start_matches('@').to_ascii_lowercase();
    anyhow::ensure!(
        !domain.is_empty() && !domain.contains(['@', '/', ' ']),
        "is not a domain"
    );
    Ok(domain)
}

fn decode<T: DeserializeOwned>(part: &str) -> Option<T> {
    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(part).ok()?).ok()
}

/// The host a publishable key names: `pk_live_` or `pk_test_`, then the host
/// and a `$`, in base64.
fn frontend_api(publishable_key: &str) -> Option<String> {
    let key = publishable_key.trim();
    let encoded = key
        .strip_prefix("pk_live_")
        .or_else(|| key.strip_prefix("pk_test_"))?;
    let decoded = String::from_utf8(STANDARD_NO_PAD_INDIFFERENT.decode(encoded).ok()?).ok()?;
    let host = decoded.strip_suffix('$')?;
    let valid = !host.is_empty()
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-');
    valid.then(|| host.to_string())
}

fn pem_body(pem: &str) -> anyhow::Result<Vec<u8>> {
    let body: String = pem
        .lines()
        .map(str::trim)
        .filter(|line| !line.starts_with("-----"))
        .collect();
    STANDARD
        .decode(body)
        .context("[clerk] public_key is not a PEM public key")
}

#[cfg(test)]
mod tests {
    use base64::Engine;
    use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
    use rstest::rstest;

    use super::frontend_api;

    /// Clerk's own keys come padded or not, so both decode.
    #[rstest]
    #[case::test_instance("pk_test_", "clerk.example.test$", Some("clerk.example.test"))]
    #[case::live_instance("pk_live_", "clerk.example.test$", Some("clerk.example.test"))]
    #[case::no_terminator("pk_test_", "clerk.example.test", None)]
    #[case::not_a_host("pk_test_", "clerk.example.test/npm$", None)]
    #[case::a_secret_key("sk_live_", "clerk.example.test$", None)]
    fn a_publishable_key_names_its_frontend_api(
        #[case] prefix: &str,
        #[case] encoded: &str,
        #[case] host: Option<&str>,
    ) {
        [STANDARD, STANDARD_NO_PAD].iter().for_each(|engine| {
            let key = format!("{prefix}{}", engine.encode(encoded));
            assert_eq!(frontend_api(&key).as_deref(), host, "{key}");
        });
    }
}
