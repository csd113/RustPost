use crate::config::Settings;

const RESERVED_USERNAMES: &[&str] = &[
    "admin", "api", "assets", "auth", "login", "logout", "register", "search", "settings",
    "static", "uploads",
];

pub fn normalize_username(username: &str, max_len: usize) -> anyhow::Result<String> {
    let trimmed = username.trim();
    if trimmed.is_empty() {
        anyhow::bail!("username cannot be empty");
    }
    if trimmed.chars().count() > max_len {
        anyhow::bail!("username is too long");
    }
    if !trimmed
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
    {
        anyhow::bail!("username may only contain letters, numbers, underscore, and hyphen");
    }
    let normalized = trimmed.to_ascii_lowercase();
    if RESERVED_USERNAMES.contains(&normalized.as_str()) {
        anyhow::bail!("username is reserved");
    }
    Ok(normalized)
}

pub fn validate_password(password: &str, settings: &Settings) -> anyhow::Result<()> {
    if password.chars().count() < settings.accounts.min_password_length {
        anyhow::bail!("password is too short");
    }
    if password.chars().any(char::is_control) {
        anyhow::bail!("password contains control characters");
    }
    Ok(())
}

pub fn clean_post_text(text: &str, max_chars: usize, media_count: usize) -> anyhow::Result<String> {
    let trimmed = text.trim();
    if trimmed.chars().count() > max_chars {
        anyhow::bail!("post is too long");
    }
    if trimmed
        .chars()
        .any(|ch| ch.is_control() && !matches!(ch, '\n' | '\r' | '\t'))
    {
        anyhow::bail!("post contains unsupported control characters");
    }
    if trimmed.is_empty() && media_count == 0 {
        anyhow::bail!("post text or media is required");
    }
    Ok(trimmed.to_owned())
}

pub fn validate_profile_text(
    display_name: &str,
    bio: &str,
    settings: &Settings,
) -> anyhow::Result<()> {
    if display_name.chars().count() > settings.accounts.max_display_name_len {
        anyhow::bail!("display name is too long");
    }
    if bio.chars().count() > settings.accounts.max_bio_len {
        anyhow::bail!("bio is too long");
    }
    if display_name.chars().any(char::is_control)
        || bio.chars().any(|ch| ch.is_control() && ch != '\n')
    {
        anyhow::bail!("profile contains unsupported control characters");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn username_validation() {
        assert_eq!(normalize_username("User-1", 32).expect("valid"), "user-1");
        assert!(normalize_username("../x", 32).is_err());
        assert!(normalize_username("admin", 32).is_err());
        assert!(normalize_username("has space", 32).is_err());
    }

    #[test]
    fn post_limit_uses_unicode_chars() {
        let text = "é".repeat(280);
        assert!(clean_post_text(&text, 280, 0).is_ok());
        let too_long = "é".repeat(281);
        assert!(clean_post_text(&too_long, 280, 0).is_err());
        assert!(clean_post_text("   ", 280, 0).is_err());
        assert!(clean_post_text("   ", 280, 1).is_ok());
    }
}
