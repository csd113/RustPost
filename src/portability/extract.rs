//! Extract for the portability subsystem.

use super::*;

/// Validates and imports `archive_path` into the authenticated destination
/// account `user_id`.
///
/// Nothing is written until the whole archive validates. Media files are
/// copied from the staging directory into the destination upload directories
/// first, then one database transaction inserts every row; if the transaction
/// fails the copied files are removed. Imported follows respect protected
/// accounts by creating pending requests, and archives cannot change the
/// destination username, password, or administrator status.
pub async fn import_account(
    pool: &SqlitePool,
    paths: &RuntimePaths,
    settings: &Settings,
    user_id: i64,
    archive_path: &Path,
) -> anyhow::Result<ImportReport> {
    // Extraction, decompression and validation run away from Tokio workers.
    let worker_paths = paths.clone();
    let worker_settings = settings.clone();
    let archive_path = archive_path.to_owned();
    let (staging, staged) = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let _operation = crate::runtime::begin_temp_operation();
        fs::create_dir_all(&worker_paths.tmp_dir)?;
        let staging = tempfile::Builder::new()
            .prefix(IMPORT_TMP_PREFIX)
            .tempdir_in(&worker_paths.tmp_dir)?;
        let limits = ArchiveLimits::from_settings(&worker_settings);
        let extracted = extract_archive(
            &archive_path,
            staging.path(),
            limits,
            worker_settings.storage.min_free_bytes,
        )?;
        let staged = parse_archive(&worker_settings, &extracted, limits)?;
        Ok((staging, staged))
    })
    .await
    .context("account archive validation worker failed")??;

    // 2. Destination and repeat-import checks, still before any mutation.
    ensure_import_destination(pool, user_id).await?;
    if archive_already_imported(pool, user_id, &staged.manifest.archive_id).await? {
        anyhow::bail!(ALREADY_IMPORTED_MESSAGE);
    }

    let reservation =
        crate::storage_limits::Reservation::new(pool, settings, paths, Some(user_id)).await?;
    let logical = staged
        .media
        .iter()
        .map(|media| media.byte_len)
        .try_fold(0_u64, u64::checked_add)
        .ok_or_else(|| anyhow::anyhow!("import size overflow"))?;
    reservation.grow(logical, true).await?;
    // 3. Copy media bytes into the upload directories, claiming unique names.
    let installed = install_media_files(pool, paths, settings, &staged.media).await?;

    // 4. Apply every database row in one transaction. The transaction reads
    //    the installed file paths, so the files must exist before it starts.
    let plan = ImportPlan {
        reservation_id: reservation.id().to_owned(),
        cleanup: installed.cleanup,
        archive_id: staged.manifest.archive_id,
        archive_username: staged.manifest.username,
        profile: staged.profile,
        posts: staged.posts,
        media: staged.media,
        media_install: installed.decisions,
        follows: staged.follows,
        muted_words: staged.muted_words,
    };
    let max_username_len = settings.accounts.max_username_len;
    let result = apply_import(pool, user_id, max_username_len, plan).await;
    drop(staging);
    result
}

/// Extracts the archive into `staging`, enforcing structural limits. Every
/// path, entry type, duplicate name, and size is checked before a single byte
/// is written outside the staging directory, and byte limits are enforced
/// incrementally while entries stream so a compressed bomb never expands past
/// `limits.expanded_bytes` on disk or in memory.
#[expect(
    clippy::too_many_lines,
    reason = "one ordered pass validates paths, documents, and streamed media together"
)]
pub(super) fn extract_archive(
    archive_path: &Path,
    staging: &Path,
    limits: ArchiveLimits,
    minimum_free_bytes: u64,
) -> anyhow::Result<ExtractedArchive> {
    let metadata = fs::symlink_metadata(archive_path)
        .with_context(|| format!("account archive {} does not exist", archive_path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        anyhow::bail!("account archive is not a regular file");
    }
    if metadata.len() > limits.compressed_bytes {
        anyhow::bail!(
            "account archive is too large ({} bytes); the limit is {} bytes",
            metadata.len(),
            limits.compressed_bytes
        );
    }

    let file = File::open(archive_path)
        .with_context(|| format!("failed to open account archive {}", archive_path.display()))?;
    let decoder = GzDecoder::new(file);
    let mut archive = Archive::new(decoder);
    let entries = archive
        .entries()
        .with_context(|| "account archive is not a valid gzip tar archive")?;

    let mut documents = BTreeMap::new();
    let mut media_files = BTreeMap::new();
    let mut seen = BTreeSet::new();
    let mut entry_count = 0usize;
    let mut total_document_bytes = 0u64;
    let mut extracted_media_bytes = 0u64;

    for entry in entries {
        let mut entry = entry.with_context(|| "account archive is not a valid gzip tar archive")?;
        entry_count += 1;
        if entry_count > limits.entries {
            anyhow::bail!(
                "account archive contains more than {} entries",
                limits.entries
            );
        }
        let (name, entry_type) = validate_entry_path(&entry)?;
        // Extraction targets can live on case-insensitive filesystems, so
        // `media/0.png` and `media/0.PNG` must collide before either is written.
        if !seen.insert(name.to_ascii_lowercase()) {
            anyhow::bail!("account archive contains duplicate entry {name}");
        }
        if entry_type.is_dir() {
            if name != MEDIA_DIR {
                anyhow::bail!("account archive contains unexpected directory {name}");
            }
            continue;
        }
        if !entry_type.is_file() {
            anyhow::bail!("account archive contains an unsupported entry type for {name}");
        }
        if DOCUMENT_PATHS.contains(&name.as_str()) {
            let limit = document_limit(&name);
            let bytes = read_limited_entry(&mut entry, limit)
                .with_context(|| format!("{name} is too large or unreadable"))?;
            total_document_bytes = total_document_bytes
                .checked_add(u64::try_from(bytes.len())?)
                .ok_or_else(|| anyhow::anyhow!("account archive document size overflow"))?;
            if total_document_bytes > MAX_TOTAL_DOCUMENT_BYTES {
                anyhow::bail!(
                    "account archive JSON documents exceed the {MAX_TOTAL_DOCUMENT_BYTES}-byte total limit"
                );
            }
            documents.insert(name, bytes);
        } else if let Some(relative) = name.strip_prefix(MEDIA_PREFIX) {
            let relative_path = Path::new(relative);
            let valid_relative = !relative.is_empty()
                && relative_path
                    .components()
                    .all(|component| matches!(component, Component::Normal(_)));
            if !valid_relative || relative_path.components().count() != 1 {
                anyhow::bail!("account archive contains an invalid media path {name}");
            }
            // Reject an oversized entry from its header before reading any of
            // its bytes so a declared giant size cannot consume disk.
            let declared_size = entry
                .header()
                .size()
                .with_context(|| format!("account archive entry {name} has an invalid size"))?;
            if declared_size > limits.expanded_bytes {
                anyhow::bail!(
                    "account archive media entry {name} declares {declared_size} bytes; the limit is {} bytes",
                    limits.expanded_bytes
                );
            }
            // A valid archive can never list more media documents than
            // `media.json` accepts, so stop extracting sooner than the general
            // entry cap allows.
            if media_files.len() >= MAX_MEDIA {
                anyhow::bail!("account archive contains more than {MAX_MEDIA} media files");
            }
            let media_dir = staging.join(MEDIA_DIR);
            fs::create_dir_all(&media_dir)?;
            let target = media_dir.join(relative);
            let (size, sha256, prefix, total) = extract_media_entry(
                &mut entry,
                &target,
                extracted_media_bytes,
                limits.expanded_bytes,
                minimum_free_bytes,
            )?;
            extracted_media_bytes = total;
            media_files.insert(
                name,
                ExtractedMediaFile {
                    staged_path: target,
                    size,
                    sha256,
                    prefix,
                },
            );
        } else {
            anyhow::bail!("account archive contains unexpected entry {name}");
        }
    }

    for document in DOCUMENT_PATHS {
        if !documents.contains_key(document) {
            anyhow::bail!("account archive is missing required document {document}");
        }
    }
    Ok(ExtractedArchive {
        documents,
        media_files,
        entry_count,
    })
}

/// Validates one tar path and returns its normalized name and type.
pub(super) fn validate_entry_path(
    entry: &tar::Entry<'_, impl Read>,
) -> anyhow::Result<(String, EntryType)> {
    let entry_type = entry.header().entry_type();
    if !entry_type.is_file() && !entry_type.is_dir() {
        anyhow::bail!("account archive contains an unsupported entry type");
    }
    let raw = entry.path_bytes();
    let raw = std::str::from_utf8(raw.as_ref())
        .map_err(|_utf8| anyhow::anyhow!("account archive contains a non-UTF-8 path"))?;
    if raw.is_empty() || raw.starts_with('/') {
        anyhow::bail!("account archive contains an absolute or empty path");
    }
    // Archives written by RustPost only use ASCII names; anything else is
    // almost certainly crafted to confuse normalization or filesystems.
    if !raw.is_ascii() {
        anyhow::bail!("account archive contains a non-ASCII path");
    }
    if raw.contains('\\')
        || raw.contains(':')
        || raw.contains("//")
        || raw.contains('\u{2215}')
        || raw.contains('\u{2044}')
        || raw.contains('\u{29f8}')
        || raw.contains('\u{ff0f}')
    {
        anyhow::bail!("account archive path contains unsafe characters");
    }
    let lower = raw.to_ascii_lowercase();
    if lower.contains("%2e") || lower.contains("%2f") || lower.contains("%5c") {
        anyhow::bail!("account archive path contains unsafe characters");
    }
    let normalized = if entry_type.is_dir() {
        raw.strip_suffix('/').unwrap_or(raw)
    } else {
        raw
    };
    if normalized.is_empty() || normalized.contains("//") || normalized.ends_with('/') {
        anyhow::bail!("account archive path contains unsafe separators");
    }
    let path = Path::new(normalized);
    if path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        anyhow::bail!("account archive entry path contains traversal");
    }
    if let Some(relative) = normalized.strip_prefix(MEDIA_PREFIX)
        && !relative.is_empty()
    {
        let file_name = Path::new(relative)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(relative);
        if file_name.len() > MAX_MEDIA_FILE_NAME_BYTES {
            anyhow::bail!("account archive media entry name is too long");
        }
        if file_name.ends_with('.') || file_name.ends_with(' ') {
            anyhow::bail!("account archive media entry name ends with '.' or a space");
        }
    }
    Ok((normalized.to_owned(), entry_type))
}

pub(super) fn document_limit(name: &str) -> u64 {
    if name == MANIFEST_PATH {
        MANIFEST_MAX_BYTES
    } else {
        DOCUMENT_MAX_BYTES
    }
}

pub(super) fn read_limited_entry(
    entry: &mut tar::Entry<'_, impl Read>,
    limit: u64,
) -> anyhow::Result<Vec<u8>> {
    let mut buffer = Vec::new();
    let mut limited = entry.take(limit.saturating_add(1));
    limited.read_to_end(&mut buffer)?;
    if u64::try_from(buffer.len()).unwrap_or(u64::MAX) > limit {
        anyhow::bail!("document exceeds the {limit}-byte limit");
    }
    Ok(buffer)
}

/// Streams one media entry into staging while hashing it, collecting a small
/// content-sniffing prefix, and enforcing the per-entry and running-total byte
/// caps. Memory use is one fixed-size buffer plus the bounded prefix.
pub(super) fn extract_media_entry(
    entry: &mut tar::Entry<'_, impl Read>,
    target: &Path,
    already_extracted: u64,
    expanded_limit: u64,
    minimum_free_bytes: u64,
) -> anyhow::Result<(u64, String, Vec<u8>, u64)> {
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(target)
        .with_context(|| format!("failed to extract media file {}", target.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; COPY_BUFFER_BYTES];
    let mut prefix = Vec::with_capacity(MEDIA_SNIFF_PREFIX_BYTES);
    let mut size = 0_u64;
    let mut total = already_extracted;
    loop {
        let read = entry.read(&mut buffer).with_context(|| {
            format!(
                "failed to read account archive media entry for {}",
                target.display()
            )
        })?;
        if read == 0 {
            break;
        }
        let read = u64::try_from(read)?;
        size = size
            .checked_add(read)
            .ok_or_else(|| anyhow::anyhow!("account archive media size overflow"))?;
        total = total
            .checked_add(read)
            .ok_or_else(|| anyhow::anyhow!("account archive media size overflow"))?;
        if size > expanded_limit || total > expanded_limit {
            anyhow::bail!("account archive media exceeds the {expanded_limit}-byte limit");
        }
        let chunk = &buffer[..usize::try_from(read)?];
        if prefix.len() < MEDIA_SNIFF_PREFIX_BYTES {
            let take = (MEDIA_SNIFF_PREFIX_BYTES - prefix.len()).min(chunk.len());
            prefix.extend_from_slice(&chunk[..take]);
        }
        hasher.update(chunk);
        if size == read || size / (8 * 1024 * 1024) != (size - read) / (8 * 1024 * 1024) {
            crate::storage_limits::ensure_headroom(
                target
                    .parent()
                    .ok_or_else(|| anyhow::anyhow!("missing staging parent"))?,
                minimum_free_bytes,
                read,
            )?;
        }
        output.write_all(chunk)?;
    }
    output
        .sync_all()
        .with_context(|| format!("failed to flush extracted media file {}", target.display()))?;
    Ok((size, hex_lower(hasher.finalize().as_ref()), prefix, total))
}

/// Parses and validates every document, applying destination instance limits.
pub(super) fn parse_archive(
    settings: &Settings,
    extracted: &ExtractedArchive,
    limits: ArchiveLimits,
) -> anyhow::Result<StagedArchive> {
    let document = |name: &str| -> anyhow::Result<&[u8]> {
        extracted
            .documents
            .get(name)
            .map(Vec::as_slice)
            .ok_or_else(|| anyhow::anyhow!("account archive is missing required document {name}"))
    };

    let manifest: ManifestDocument = parse_document(MANIFEST_PATH, document(MANIFEST_PATH)?)?;
    if manifest.format_version != FORMAT_VERSION {
        anyhow::bail!(
            "unsupported account archive format version {} (this instance supports {FORMAT_VERSION})",
            manifest.format_version
        );
    }
    if manifest.app != APP_NAME {
        anyhow::bail!("account archive is not a RustPost account archive");
    }
    validate_timestamp_text(&manifest.created_at, "manifest created_at")?;
    if manifest.archive_id.is_empty()
        || manifest.archive_id.chars().count() > 128
        || manifest.archive_id.chars().any(char::is_control)
    {
        anyhow::bail!("manifest archive_id is invalid");
    }
    if manifest.username.is_empty()
        || manifest.username.chars().count() > 128
        || manifest.username.chars().any(char::is_control)
    {
        anyhow::bail!("manifest username is invalid");
    }
    if manifest.entry_count != extracted.entry_count {
        anyhow::bail!("manifest entry_count does not match the number of archive entries");
    }

    let mut profile: ProfileDocument = parse_document(PROFILE_PATH, document(PROFILE_PATH)?)?;
    if profile.theme != "light" && profile.theme != "dark" {
        anyhow::bail!("profile theme must be light or dark");
    }
    validate_profile_text(&profile.display_name, &profile.bio, settings)
        .with_context(|| "profile contains invalid text")?;
    if profile.location.chars().count() > MAX_LOCATION_CHARS
        || profile.location.chars().any(char::is_control)
    {
        anyhow::bail!("profile location is invalid");
    }
    let website = profile.website.trim().to_owned();
    if website.chars().count() > MAX_WEBSITE_CHARS || website.chars().any(char::is_control) {
        anyhow::bail!("profile website is invalid");
    }
    if !(website.is_empty() || website.starts_with("http://") || website.starts_with("https://")) {
        anyhow::bail!("profile website must start with http:// or https://");
    }
    profile.website = website;
    validate_timestamp_text(&profile.created_at, "profile created_at")?;

    let media_documents: Vec<MediaDocument> = parse_document(MEDIA_PATH, document(MEDIA_PATH)?)?;
    let media = validate_media_documents(&media_documents, extracted, limits)?;
    let media_ids = media.iter().map(|entry| entry.id).collect::<BTreeSet<_>>();

    let post_documents: Vec<PostDocument> = parse_document(POSTS_PATH, document(POSTS_PATH)?)?;
    let posts = validate_post_documents(&post_documents, settings, &media_ids)?;

    let follows: Vec<FollowDocument> = parse_document(FOLLOWS_PATH, document(FOLLOWS_PATH)?)?;
    if follows.len() > MAX_FOLLOWS {
        anyhow::bail!("follows.json contains more than {MAX_FOLLOWS} entries");
    }
    for follow in &follows {
        if follow.username.is_empty()
            || follow.username.chars().count() > 128
            || follow.username.chars().any(char::is_control)
        {
            anyhow::bail!("follows.json contains an invalid username");
        }
        validate_timestamp_text(&follow.created_at, "follows.json created_at")?;
    }

    let settings_document: SettingsDocument =
        parse_document(SETTINGS_PATH, document(SETTINGS_PATH)?)?;
    if settings_document.muted_words.len() > MAX_MUTED_WORDS {
        anyhow::bail!("settings.json contains more than {MAX_MUTED_WORDS} muted words");
    }
    let mut muted_words = Vec::with_capacity(settings_document.muted_words.len());
    for word in settings_document.muted_words {
        let term = clean_muted_word(&word.term)?;
        validate_timestamp_text(&word.created_at, "settings.json muted word created_at")?;
        muted_words.push(MutedWordDocument {
            term,
            created_at: word.created_at,
        });
    }

    Ok(StagedArchive {
        manifest,
        profile,
        posts,
        media,
        follows,
        muted_words,
    })
}

/// `serde_json` applies its default recursion limit (128), so deeply nested
/// documents are rejected instead of risking stack exhaustion.
pub(super) fn parse_document<T: serde::de::DeserializeOwned>(
    name: &str,
    bytes: &[u8],
) -> anyhow::Result<T> {
    serde_json::from_slice(bytes).with_context(|| format!("{name} is not a valid JSON document"))
}

#[expect(
    clippy::too_many_lines,
    reason = "media documents are validated in two passes over ids, paths, bytes, and sniffed content"
)]
pub(super) fn validate_media_documents(
    documents: &[MediaDocument],
    extracted: &ExtractedArchive,
    limits: ArchiveLimits,
) -> anyhow::Result<Vec<ValidatedMedia>> {
    if documents.len() > MAX_MEDIA {
        anyhow::bail!("media.json contains more than {MAX_MEDIA} media entries");
    }

    let mut ids = BTreeSet::new();
    let mut paths = BTreeSet::new();
    let mut declared_total = 0_u64;
    for document in documents {
        if document.id < 0 {
            anyhow::bail!("media.json contains a negative media id");
        }
        if !ids.insert(document.id) {
            anyhow::bail!("media.json contains duplicate media id {}", document.id);
        }
        if document.media_kind != "image" && document.media_kind != "video" {
            anyhow::bail!("media {} has an unsupported media kind", document.id);
        }
        if document.mime_type.is_empty()
            || document.mime_type.chars().count() > 255
            || document.mime_type.chars().any(char::is_control)
            || !document
                .mime_type
                .starts_with(&format!("{}/", document.media_kind))
        {
            anyhow::bail!(
                "media {} mime type does not match its media kind",
                document.id
            );
        }
        if document.sha256.len() != 64
            || !document.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            anyhow::bail!("media {} sha256 is not 64 hex characters", document.id);
        }
        if document.original_filename.chars().count() > MAX_ORIGINAL_FILENAME_CHARS {
            anyhow::bail!("media {} original filename is too long", document.id);
        }
        if document.alt_text.chars().count() > MAX_ALT_TEXT_CHARS {
            anyhow::bail!("media {} alt text is too long", document.id);
        }
        if document.conversion_state.chars().count() > MAX_CONVERSION_STATE_CHARS {
            anyhow::bail!("media {} conversion state is too long", document.id);
        }
        if !paths.insert(document.archive_path.clone()) {
            anyhow::bail!(
                "media.json lists archive path {} more than once",
                document.archive_path
            );
        }
        declared_total = declared_total
            .checked_add(document.byte_len)
            .ok_or_else(|| anyhow::anyhow!("media.json byte length total overflow"))?;
        if declared_total > limits.expanded_bytes {
            anyhow::bail!(
                "media.json declares more than the {}-byte account archive media limit",
                limits.expanded_bytes
            );
        }
    }

    let mut validated = Vec::with_capacity(documents.len());
    for document in documents {
        let Some(extracted_file) = extracted.media_files.get(&document.archive_path) else {
            anyhow::bail!(
                "media {} archive path {} is missing from the archive",
                document.id,
                document.archive_path
            );
        };
        if extracted_file.size != document.byte_len {
            anyhow::bail!(
                "media {} byte length does not match the extracted file",
                document.id
            );
        }
        if !extracted_file.sha256.eq_ignore_ascii_case(&document.sha256) {
            anyhow::bail!(
                "media {} sha256 does not match the extracted file",
                document.id
            );
        }
        // Interactive uploads derive the stored mime type from `infer`; an
        // imported file must agree with its declared type so a mislabeled
        // payload is never installed as media.
        let Some(sniffed) = infer::get(&extracted_file.prefix) else {
            anyhow::bail!(
                "media {} content is not a recognized media type",
                document.id
            );
        };
        if !sniffed
            .mime_type()
            .eq_ignore_ascii_case(&document.mime_type)
        {
            anyhow::bail!(
                "media {} content type {} does not match the declared {}",
                document.id,
                sniffed.mime_type(),
                document.mime_type
            );
        }
        validated.push(ValidatedMedia {
            id: document.id,
            original_filename: document.original_filename.clone(),
            mime_type: document.mime_type.clone(),
            media_kind: document.media_kind.clone(),
            byte_len: document.byte_len,
            alt_text: document.alt_text.clone(),
            is_nsfw: document.is_nsfw,
            sha256: document.sha256.to_ascii_lowercase(),
            archive_path: document.archive_path.clone(),
            staged_path: extracted_file.staged_path.clone(),
        });
    }

    if extracted.media_files.len() != documents.len() {
        for name in extracted.media_files.keys() {
            if !paths.contains(name) {
                anyhow::bail!("account archive contains unlisted media file {name}");
            }
        }
        anyhow::bail!("media.json does not list every media file in the archive");
    }
    Ok(validated)
}

pub(super) fn validate_post_documents(
    documents: &[PostDocument],
    settings: &Settings,
    media_ids: &BTreeSet<i64>,
) -> anyhow::Result<Vec<ValidatedPost>> {
    if documents.len() > MAX_POSTS {
        anyhow::bail!("posts.json contains more than {MAX_POSTS} posts");
    }
    let mut ids = BTreeSet::new();
    for document in documents {
        if document.id < 0 {
            anyhow::bail!("posts.json contains a negative post id");
        }
        if !ids.insert(document.id) {
            anyhow::bail!("posts.json contains duplicate post id {}", document.id);
        }
    }

    let mut validated = Vec::with_capacity(documents.len());
    for document in documents {
        let text = clean_post_text(
            &document.text,
            settings.posts.max_text_chars,
            document.media_ids.len(),
        )
        .with_context(|| format!("post {} text is invalid", document.id))?;
        validate_timestamp_text(&document.created_at, "post created_at")?;
        if let Some(edited_at) = &document.edited_at {
            validate_timestamp_text(edited_at, "post edited_at")?;
        }
        for media_id in &document.media_ids {
            if !media_ids.contains(media_id) {
                anyhow::bail!(
                    "post {} references unknown media id {media_id}",
                    document.id
                );
            }
        }
        if document.embeds.len() > MAX_EMBEDS_PER_POST {
            anyhow::bail!(
                "post {} contains more than {MAX_EMBEDS_PER_POST} embeds",
                document.id
            );
        }
        let mut embed_positions = BTreeSet::new();
        let mut embed_videos = BTreeSet::new();
        for embed in &document.embeds {
            if embed.provider != "youtube" {
                anyhow::bail!(
                    "post {} contains an unsupported embed provider",
                    document.id
                );
            }
            if embed.video_id.is_empty()
                || embed.video_id.chars().count() > MAX_EMBED_VIDEO_ID_CHARS
                || embed.video_id.chars().any(char::is_control)
            {
                anyhow::bail!("post {} contains an invalid embed video id", document.id);
            }
            if embed.position < 0 || !embed_positions.insert(embed.position) {
                anyhow::bail!("post {} contains duplicate embed positions", document.id);
            }
            if !embed_videos.insert(embed.video_id.clone()) {
                anyhow::bail!("post {} lists the same embed video twice", document.id);
            }
            for url in [
                &embed.original_url,
                &embed.canonical_url,
                &embed.thumbnail_url,
                &embed.embed_url,
            ] {
                if url.is_empty()
                    || url.chars().count() > MAX_EMBED_URL_CHARS
                    || url.chars().any(char::is_control)
                {
                    anyhow::bail!("post {} contains an invalid embed URL", document.id);
                }
            }
            if let Some(title) = &embed.title
                && (title.chars().count() > MAX_EMBED_TITLE_CHARS
                    || title.chars().any(char::is_control))
            {
                anyhow::bail!("post {} contains an invalid embed title", document.id);
            }
        }
        validated.push(ValidatedPost {
            archive_id: document.id,
            text,
            created_at: document.created_at.clone(),
            edited_at: document.edited_at.clone(),
            parent_id: document.parent_id,
            root_id: document.root_id,
            quote_id: document.quote_id,
            media_ids: document.media_ids.clone(),
            embeds: document.embeds.clone(),
        });
    }
    Ok(validated)
}

pub(super) fn validate_timestamp_text(value: &str, field: &str) -> anyhow::Result<()> {
    if value.is_empty()
        || value.chars().count() > MAX_TIMESTAMP_CHARS
        || value.chars().any(char::is_control)
    {
        anyhow::bail!("{field} is not a valid timestamp");
    }
    Ok(())
}

/// Mirrors `social::clean_muted_word` (private) so imports normalize muted
/// words exactly like the interactive path.
pub(super) fn clean_muted_word(term: &str) -> anyhow::Result<String> {
    let trimmed = term.trim();
    if trimmed.is_empty() {
        anyhow::bail!("muted word cannot be empty");
    }
    if trimmed.chars().count() > 100 {
        anyhow::bail!("muted word is too long");
    }
    if trimmed
        .chars()
        .any(|ch| ch.is_control() && !matches!(ch, '\n' | '\r' | '\t'))
    {
        anyhow::bail!("muted word contains unsupported control characters");
    }
    Ok(trimmed.to_owned())
}
