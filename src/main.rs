mod captcha;
mod config;
mod error;
mod signer;
mod utils;
mod verification_provider;

use axum::{extract::State, routing::post, Json, Router};
use base64::{engine::general_purpose, Engine};
use captcha::CaptchaClient;
use chrono::Utc;
use error::AppError;
use near_crypto::Signature;
use near_sdk::borsh::{self, BorshDeserialize, BorshSerialize};
use near_sdk::serde::{Deserialize, Serialize};
use near_sdk::AccountId;
use tower_http::cors::CorsLayer;
use uuid::Uuid;

use crate::config::AppConfig;
use utils::{enable_logging, is_allowed_named_sub_account, set_heavy_panic};
use verification_provider::{FractalClient, FractalTokenKind, KycStatus, VerifiedUser};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Exit on any panic in any async task
    set_heavy_panic();

    // Try load environment variables from `.env` if provided
    dotenv::dotenv().ok();

    enable_logging();
    let config = config::load_config()?;

    // Log a base64 encoded ed25519 public key to be used in smart contract for signature verification
    tracing::info!(
        "ED25519 public key (base64 encoded): {}",
        general_purpose::STANDARD.encode(
            config
                .signer
                .credentials
                .signing_key
                .public_key()
                .unwrap_as_ed25519()
                .as_ref()
        )
    );

    let addr = config
        .listen_address
        .parse()
        .expect("Can't parse socket address");

    let state = AppState::new(config.clone())?;

    let app = Router::new()
        .route("/verify", post(verify))
        .layer(CorsLayer::permissive())
        .with_state(state);

    tracing::debug!("Server listening on {}", addr);

    axum::Server::bind(&addr)
        .serve(app.into_make_service())
        .await?;

    Ok(())
}

#[derive(Clone)]
pub struct AppState {
    pub config: AppConfig,
    pub client: FractalClient,
    pub captcha: CaptchaClient,
}

impl AppState {
    pub fn new(config: AppConfig) -> Result<Self, AppError> {
        Ok(Self {
            captcha: CaptchaClient::new(config.captcha.clone())?,
            client: FractalClient::create(config.verification_provider.clone())?,
            config,
        })
    }
}

#[derive(Deserialize, Debug)]
#[serde(crate = "near_sdk::serde")]
pub struct VerificationReq {
    pub claimer: AccountId,
    #[serde(flatten)]
    pub fractal_token: FractalTokenKind,
}

#[derive(BorshSerialize, BorshDeserialize, Debug)]
pub struct VerifiedAccountToken {
    pub claimer: AccountId,
    pub ext_account: ExternalAccountId,
    pub timestamp: u64,
    pub verified_kyc: bool,
}

/// External account id represented as hexadecimal string
#[derive(BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq)]
pub struct ExternalAccountId(String);

impl std::fmt::Display for ExternalAccountId {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl AsRef<String> for ExternalAccountId {
    fn as_ref(&self) -> &String {
        &self.0
    }
}

impl From<Uuid> for ExternalAccountId {
    fn from(value: Uuid) -> Self {
        let mut buf = [0u8; uuid::fmt::Simple::LENGTH];
        Self(value.as_simple().encode_lower(&mut buf).to_owned())
    }
}

#[derive(Serialize, Debug)]
#[serde(crate = "near_sdk::serde")]
pub struct SignedResponse {
    #[serde(rename = "m")]
    pub message: String,
    #[serde(rename = "sig")]
    pub signature_ed25519: String,
    #[serde(rename = "kyc")]
    pub kyc_status: KycStatus,
}

pub async fn verify(
    State(state): State<AppState>,
    Json(req): Json<VerificationReq>,
) -> Result<Json<SignedResponse>, AppError> {
    tracing::debug!("Req: {:?}", req);

    if !state.config.allow_named_sub_accounts && !is_allowed_named_sub_account(&req.claimer) {
        return Err(AppError::NotAllowedNamedSubAccount(req.claimer));
    }

    if let Some(captcha_token) = req.fractal_token.captcha() {
        match state.captcha.verify(captcha_token).await {
            Ok(true) => (),
            Ok(false) => return Err(AppError::SuspiciousUser),
            Err(e) => {
                tracing::error!(
                    "Captcha verification failure for account `{:?}`. Error: {e:?}",
                    req.claimer
                );
                return Err(AppError::from(e));
            }
        };
    }

    let verified_user = state.client.verify(req.fractal_token).await?;

    create_verified_account_response(&state.config, req.claimer, verified_user)
}

/// Creates signed json response with verified account
fn create_verified_account_response(
    config: &AppConfig,
    claimer: AccountId,
    verified_user: VerifiedUser,
) -> Result<Json<SignedResponse>, AppError> {
    let credentials = &config.signer.credentials;
    let raw_message = VerifiedAccountToken {
        claimer,
        ext_account: verified_user.user_id.clone(),
        timestamp: Utc::now().timestamp() as u64,
        verified_kyc: verified_user.kyc_status == KycStatus::Approved,
    }
    .try_to_vec()
    .map_err(|_| AppError::SigningError)?;
    let signature = credentials.signing_key.sign(&raw_message);

    if !signature.verify(&raw_message, &credentials.signing_key.public_key()) {
        return Err(AppError::SigningError);
    }

    let raw_signature_ed25519 = match signature {
        Signature::ED25519(signature) => signature.to_bytes(),
        _ => return Err(AppError::SigningError),
    };

    let message = general_purpose::STANDARD.encode(&raw_message);
    let signature_ed25519 = general_purpose::STANDARD.encode(raw_signature_ed25519);

    tracing::debug!("Verification passed for {verified_user:?}");

    Ok(Json(SignedResponse {
        message,
        signature_ed25519,
        kyc_status: verified_user.kyc_status,
    }))
}

#[cfg(test)]
mod tests {
    use crate::signer::{SignerConfig, SignerCredentials};
    use crate::verification_provider::{KycStatus, VerifiedUser};
    use crate::{create_verified_account_response, AppConfig, VerifiedAccountToken};
    use assert_matches::assert_matches;
    use base64::{engine::general_purpose, Engine};
    use chrono::Utc;
    use near_crypto::{KeyType, Signature};
    use near_sdk::borsh::{BorshDeserialize, BorshSerialize};
    use near_sdk::AccountId;
    use std::str::FromStr;
    use uuid::Uuid;

    #[test]
    fn test_create_verified_account_response_no_kyc() {
        let signing_key = near_crypto::SecretKey::from_random(near_crypto::KeyType::ED25519);
        let config = AppConfig {
            signer: SignerConfig {
                credentials: SignerCredentials { signing_key },
            },
            listen_address: "0.0.0.0:8080".to_owned(),
            verification_provider: Default::default(),
            captcha: Default::default(),
            allow_named_sub_accounts: true,
        };

        let claimer = AccountId::new_unchecked("test.near".to_owned());
        let verified_user = VerifiedUser {
            user_id: Uuid::default().into(),
            kyc_status: KycStatus::Unavailable,
        };
        let res = create_verified_account_response(&config, claimer.clone(), verified_user.clone())
            .unwrap();

        let credentials = &config.signer.credentials;

        let decoded_bytes = general_purpose::STANDARD.decode(&res.message).unwrap();

        assert!(Signature::from_parts(
            KeyType::ED25519,
            &general_purpose::STANDARD
                .decode(&res.signature_ed25519)
                .unwrap()
        )
        .unwrap()
        .verify(&decoded_bytes, &credentials.signing_key.public_key()));

        let decoded_msg = VerifiedAccountToken::try_from_slice(&decoded_bytes).unwrap();

        assert_matches!(decoded_msg, VerifiedAccountToken {
            claimer: claimer_res,
            ext_account: ext_account_res,
            timestamp: _,
            verified_kyc: false,
        } if claimer_res == claimer && ext_account_res == verified_user.user_id);
    }

    #[test]
    fn test_create_verified_account_response_with_kyc() {
        let signing_key = near_crypto::SecretKey::from_random(near_crypto::KeyType::ED25519);
        let config = AppConfig {
            signer: SignerConfig {
                credentials: SignerCredentials { signing_key },
            },
            listen_address: "0.0.0.0:8080".to_owned(),
            verification_provider: Default::default(),
            captcha: Default::default(),
            allow_named_sub_accounts: true,
        };

        let claimer = AccountId::new_unchecked("test.near".to_owned());
        let verified_user = VerifiedUser {
            user_id: Uuid::default().into(),
            kyc_status: KycStatus::Approved,
        };
        let res = create_verified_account_response(&config, claimer.clone(), verified_user.clone())
            .unwrap();

        let credentials = &config.signer.credentials;

        let decoded_bytes = general_purpose::STANDARD.decode(&res.message).unwrap();

        assert!(Signature::from_parts(
            KeyType::ED25519,
            &general_purpose::STANDARD
                .decode(&res.signature_ed25519)
                .unwrap()
        )
        .unwrap()
        .verify(&decoded_bytes, &credentials.signing_key.public_key()));

        let decoded_msg = VerifiedAccountToken::try_from_slice(&decoded_bytes).unwrap();

        assert_matches!(decoded_msg, VerifiedAccountToken {
            claimer: claimer_res,
            ext_account: ext_account_res,
            timestamp: _,
            verified_kyc: true,
        } if claimer_res == claimer && ext_account_res == verified_user.user_id);
    }

    #[test]
    fn test_account_id_uuid_borsh_serde() {
        let serialized = VerifiedAccountToken {
            claimer: AccountId::new_unchecked("test.near".to_owned()),
            ext_account: Uuid::from_str("f20181ba-fc0c-11ed-be56-0242ac120002")
                .unwrap()
                .into(),
            timestamp: Utc::now().timestamp() as u64,
            verified_kyc: true,
        }
        .try_to_vec()
        .unwrap();

        assert_eq!(
            VerifiedAccountToken::try_from_slice(serialized.as_slice())
                .unwrap()
                .ext_account
                .0,
            "f20181bafc0c11edbe560242ac120002"
        );
    }
}
