//! Administrator release checks; installation is an operator action.

use super::{
    AppError, AppResult, AppState, Arc, CurrentUser, Deserialize, Form, HeaderMap, Html, State,
    admin, form_csrf, page_layout, rate_limit, render, require_admin, validate_csrf,
};
use crate::updates::{self, Discovery};

static CHECKS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

pub(super) async fn admin_updates(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> AppResult<Html<String>> {
    let user = require_admin(&state, &headers).await?;
    updates_page(&state, &user, &headers).await
}

#[derive(Deserialize)]
pub(super) struct CheckForm {
    csrf: String,
}

pub(super) async fn admin_check_updates(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<CheckForm>,
) -> AppResult<Html<String>> {
    let user = require_admin(&state, &headers).await?;
    validate_csrf(&state.pool, &headers, &form.csrf).await?;
    let permit = CHECKS.try_acquire().map_err(|_| AppError::Busy)?;
    rate_limit::check_and_record(
        &state.pool,
        rate_limit::Scope::UpdateCheck,
        &user.id.to_string(),
        5,
        900,
    )
    .await
    .map_err(|error| AppError::BadRequest(error.to_string()))?;
    let discovery = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        updates::discover()
    })
    .await
    .map_err(anyhow::Error::from)?;
    *state.update_discovery.lock().await = Some(discovery);
    admin::audit(
        &state.pool,
        user.id,
        "check_software_updates",
        "GitHub latest release",
    )
    .await?;
    updates_page(&state, &user, &headers).await
}

async fn updates_page(
    state: &AppState,
    user: &CurrentUser,
    headers: &HeaderMap,
) -> AppResult<Html<String>> {
    let csrf = form_csrf(state, headers)
        .await?
        .ok_or(AppError::Unauthorized)?;
    let mut body = render::page_header(
        "Software updates",
        "Check the latest RustPost release on GitHub.",
    );
    body.push_str(&format!(r#"<section class="panel admin-card"><p>Installed version: v{}</p><form method="post" action="/admin/updates/check"><input type="hidden" name="csrf" value="{}"><button type="submit">Check for updates</button></form></section>"#, html_escape::encode_text(updates::VERSION), html_escape::encode_double_quoted_attribute(&csrf)));
    match state.update_discovery.lock().await.as_ref() {
        None => {}
        Some(Discovery::UpToDate) => body.push_str(&render::notice("success", "RustPost is up to date.")),
        Some(Discovery::UnableToCheck) => body.push_str(&render::notice("error", "Unable to check GitHub releases. Try again later.")),
        Some(Discovery::Available { version, url }) => body.push_str(&format!(r#"<section class="panel"><p>RustPost v{} is available.</p><p><a href="{}" rel="noopener noreferrer">View the latest GitHub release</a></p></section>"#, html_escape::encode_text(version), html_escape::encode_double_quoted_attribute(url))),
    }
    Ok(Html(
        page_layout(state, Some(user), Some(&csrf), "Software updates", &body).await?,
    ))
}
