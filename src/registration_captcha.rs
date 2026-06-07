use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use base64::Engine as _;
use captcha::Captcha;
use captcha::filters::{Dots, Noise, Wave};
use sha2::{Digest as _, Sha256};
use tokio::sync::Mutex;
use uuid::Uuid;

const CAPTCHA_CHARS: &[u8] = b"123456789ABCDEFGHJKMNPQRSTUVWXYZ";
const CAPTCHA_LENGTH: usize = 5;
const CAPTCHA_TTL: Duration = Duration::from_secs(10 * 60);
#[cfg(debug_assertions)]
const CAPTCHA_E2E_ANSWER_ENV: &str = "RUSTPOST_E2E_CAPTCHA_ANSWER";

#[derive(Debug, Clone)]
pub struct RegistrationCaptchaChallenge {
    pub token: String,
    pub image_data_uri: String,
    pub expires_minutes: u64,
    #[cfg(test)]
    pub answer: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistrationCaptchaError {
    MissingChallenge,
    MissingAnswer,
    ExpiredOrUsed,
    Incorrect,
}

impl RegistrationCaptchaError {
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::MissingChallenge => "CAPTCHA challenge is missing. Please try again.",
            Self::MissingAnswer => "CAPTCHA answer is required.",
            Self::ExpiredOrUsed => {
                "CAPTCHA challenge expired or was already used. Please try again."
            }
            Self::Incorrect => "CAPTCHA answer was incorrect. Please try again.",
        }
    }
}

#[derive(Clone, Default)]
pub struct RegistrationCaptchaStore {
    challenges: Arc<Mutex<HashMap<String, StoredChallenge>>>,
}

#[derive(Debug, Clone)]
struct StoredChallenge {
    answer_digest: [u8; 32],
    expires_at: Instant,
}

impl RegistrationCaptchaStore {
    pub async fn create_challenge(&self) -> anyhow::Result<RegistrationCaptchaChallenge> {
        let token = Uuid::new_v4().to_string();
        let answer = challenge_answer()?;
        let image_data_uri = captcha_image_data_uri(&answer)?;
        let expires_at = Instant::now() + CAPTCHA_TTL;
        let answer_digest = answer_digest(&token, &answer);

        let mut challenges = self.challenges.lock().await;
        remove_expired(&mut challenges, Instant::now());
        challenges.insert(
            token.clone(),
            StoredChallenge {
                answer_digest,
                expires_at,
            },
        );
        drop(challenges);

        Ok(RegistrationCaptchaChallenge {
            token,
            image_data_uri,
            expires_minutes: CAPTCHA_TTL.as_secs() / 60,
            #[cfg(test)]
            answer,
        })
    }

    pub async fn validate(
        &self,
        token: Option<&str>,
        answer: Option<&str>,
    ) -> Result<(), RegistrationCaptchaError> {
        let token = token
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or(RegistrationCaptchaError::MissingChallenge)?;
        let answer = answer
            .map(normalize_answer)
            .filter(|value| !value.is_empty())
            .ok_or(RegistrationCaptchaError::MissingAnswer)?;

        let now = Instant::now();
        let Some(challenge) = ({
            let mut challenges = self.challenges.lock().await;
            remove_expired(&mut challenges, now);
            challenges.remove(token)
        }) else {
            return Err(RegistrationCaptchaError::ExpiredOrUsed);
        };
        if challenge.expires_at <= now {
            return Err(RegistrationCaptchaError::ExpiredOrUsed);
        }
        if answer_digest(token, &answer) == challenge.answer_digest {
            Ok(())
        } else {
            Err(RegistrationCaptchaError::Incorrect)
        }
    }

    #[cfg(test)]
    pub async fn expire_for_test(&self, token: &str) {
        let mut challenges = self.challenges.lock().await;
        if let Some(challenge) = challenges.get_mut(token) {
            let now = Instant::now();
            challenge.expires_at = now.checked_sub(Duration::from_secs(1)).unwrap_or(now);
        }
    }
}

fn challenge_answer() -> anyhow::Result<String> {
    #[cfg(debug_assertions)]
    if let Ok(answer) = std::env::var(CAPTCHA_E2E_ANSWER_ENV) {
        let normalized = normalize_answer(&answer);
        if normalized.len() == CAPTCHA_LENGTH
            && normalized.bytes().all(|byte| CAPTCHA_CHARS.contains(&byte))
        {
            return Ok(normalized);
        }
    }

    let mut bytes = [0_u8; CAPTCHA_LENGTH];
    getrandom::fill(&mut bytes).context("failed to generate CAPTCHA randomness")?;
    Ok(bytes
        .iter()
        .map(|byte| CAPTCHA_CHARS[usize::from(*byte) % CAPTCHA_CHARS.len()] as char)
        .collect())
}

fn captcha_image_data_uri(answer: &str) -> anyhow::Result<String> {
    let mut captcha = Captcha::new();
    for ch in answer.chars() {
        captcha.set_chars(&[ch]).add_char();
    }
    captcha
        .apply_filter(Noise::new(0.18))
        .apply_filter(Wave::new(1.6, 14.0).horizontal())
        .view(220, 90)
        .apply_filter(Dots::new(12));
    let png = captcha.as_png().context("failed to render CAPTCHA image")?;
    let encoded = base64::engine::general_purpose::STANDARD.encode(png);
    Ok(format!("data:image/png;base64,{encoded}"))
}

fn answer_digest(token: &str, answer: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hasher.update([0]);
    hasher.update(normalize_answer(answer).as_bytes());
    hasher.finalize().into()
}

fn normalize_answer(answer: &str) -> String {
    answer.trim().to_ascii_uppercase()
}

fn remove_expired(challenges: &mut HashMap<String, StoredChallenge>, now: Instant) {
    challenges.retain(|_, challenge| challenge.expires_at > now);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn challenge_validates_once_case_insensitively() {
        let store = RegistrationCaptchaStore::default();
        let challenge = store.create_challenge().await.expect("challenge");

        store
            .validate(
                Some(&challenge.token),
                Some(&challenge.answer.to_lowercase()),
            )
            .await
            .expect("valid answer");
        assert_eq!(
            store
                .validate(Some(&challenge.token), Some(&challenge.answer))
                .await,
            Err(RegistrationCaptchaError::ExpiredOrUsed)
        );
    }

    #[tokio::test]
    async fn wrong_answer_consumes_challenge() {
        let store = RegistrationCaptchaStore::default();
        let challenge = store.create_challenge().await.expect("challenge");

        assert_eq!(
            store.validate(Some(&challenge.token), Some("WRONG")).await,
            Err(RegistrationCaptchaError::Incorrect)
        );
        assert_eq!(
            store
                .validate(Some(&challenge.token), Some(&challenge.answer))
                .await,
            Err(RegistrationCaptchaError::ExpiredOrUsed)
        );
    }
}
