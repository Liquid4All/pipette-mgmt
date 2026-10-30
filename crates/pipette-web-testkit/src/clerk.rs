//! A Clerk instance a test issues tokens as: an RSA key, the `[clerk]` section
//! that trusts it, and the claims the JWT template that section names would carry.

use aws_lc_rs::encoding::AsDer;
use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::rsa::{KeyPair as RsaKeyPair, KeySize};
use aws_lc_rs::signature::{KeyPair, RSA_PKCS1_SHA256};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use serde_json::{Value, json};

use pipette_core::config::{ClerkConfig, ClerkProvisionRule, DEFAULT_CLERK_JWT_TEMPLATE};
use pipette_httpapi::state::AppState;

use crate::app::{make_state_from_config, setup_benchmarks, test_config};

/// The origin the console is served from, as a token's `azp` names it.
pub const CLERK_ORIGIN: &str = "https://console.example.test";

/// The Frontend API the publishable key names, and every token's `iss` host.
pub const CLERK_FRONTEND_API: &str = "clerk.example.test";

pub struct ClerkInstance {
    key: RsaKeyPair,
    jwt_template: String,
}

impl ClerkInstance {
    /// An instance whose `[clerk]` leaves `jwt_template` at its default.
    pub fn new() -> anyhow::Result<Self> {
        Self::with_jwt_template(DEFAULT_CLERK_JWT_TEMPLATE)
    }

    /// An instance whose `[clerk]` names `jwt_template`, and whose tokens come from it.
    pub fn with_jwt_template(jwt_template: &str) -> anyhow::Result<Self> {
        let key = RsaKeyPair::generate(KeySize::Rsa2048)
            .map_err(|e| anyhow::anyhow!("generating an RSA key: {e}"))?;
        Ok(Self {
            key,
            jwt_template: jwt_template.to_string(),
        })
    }

    /// The `[clerk]` section trusting this instance, with `provision` as its rules.
    pub fn config(&self, provision: &[(&str, &str)]) -> anyhow::Result<ClerkConfig> {
        let der = self
            .key
            .public_key()
            .as_der()
            .map_err(|e| anyhow::anyhow!("encoding the public key: {e}"))?;
        let body = STANDARD.encode(der.as_ref());
        let lines: Vec<&str> = body
            .as_bytes()
            .chunks(64)
            .map(std::str::from_utf8)
            .collect::<Result<_, _>>()?;
        Ok(ClerkConfig {
            publishable_key: format!(
                "pk_test_{}",
                STANDARD.encode(format!("{CLERK_FRONTEND_API}$"))
            ),
            public_key: format!(
                "-----BEGIN PUBLIC KEY-----\n{}\n-----END PUBLIC KEY-----\n",
                lines.join("\n")
            ),
            authorized_parties: vec![CLERK_ORIGIN.to_string()],
            jwt_template: self.jwt_template.clone(),
            allowed_domains: Vec::new(),
            provision: provision
                .iter()
                .map(|(domain, org)| ClerkProvisionRule {
                    domain: (*domain).to_string(),
                    org: (*org).to_string(),
                })
                .collect(),
        })
    }

    /// The claims the template issues for `email` now: verified, for this
    /// console, from this instance.
    pub fn claims(&self, email: &str) -> Value {
        let now = chrono::Utc::now().timestamp();
        json!({
            "azp": CLERK_ORIGIN,
            "aud": self.jwt_template,
            "email": email,
            "email_verified": true,
            "exp": now + 60,
            "iat": now,
            "iss": format!("https://{CLERK_FRONTEND_API}"),
            "jti": uuid::Uuid::now_v7().to_string(),
            "nbf": now - 10,
            "sub": "user_2clerk",
        })
    }

    /// `claims`, signed RS256 as the instance signs them.
    pub fn token(&self, claims: &Value) -> anyhow::Result<String> {
        self.token_with_header(&json!({ "alg": "RS256", "typ": "JWT" }), claims)
    }

    /// `claims` under `header`, signed RS256 whatever the header says.
    pub fn token_with_header(&self, header: &Value, claims: &Value) -> anyhow::Result<String> {
        let signed = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(header)?),
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims)?)
        );
        let mut signature = vec![0; self.key.public_modulus_len()];
        self.key
            .sign(
                &RSA_PKCS1_SHA256,
                &SystemRandom::new(),
                signed.as_bytes(),
                &mut signature,
            )
            .map_err(|e| anyhow::anyhow!("signing a token: {e}"))?;
        Ok(format!("{signed}.{}", URL_SAFE_NO_PAD.encode(signature)))
    }
}

/// A seeded state that signs readers in through `clerk`, with `provision` as
/// its rules.
pub async fn make_state_with_clerk(
    dir: &std::path::Path,
    clerk: &ClerkInstance,
    provision: &[(&str, &str)],
) -> anyhow::Result<AppState> {
    make_state_with_clerk_section(dir, clerk.config(provision)?).await
}

/// A seeded state whose `[clerk]` is `section`, for a test that edits it first.
pub async fn make_state_with_clerk_section(
    dir: &std::path::Path,
    section: ClerkConfig,
) -> anyhow::Result<AppState> {
    setup_benchmarks(dir)?;
    let mut config = test_config(dir);
    config.clerk = Some(section);
    make_state_from_config(config).await
}
