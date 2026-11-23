//! Private, revision-checked drafts and atomic scheduled publication.

use super::*;

#[derive(Debug, Clone, Default)]
pub struct DraftInput {
    pub id: Option<i64>,
    pub revision: Option<i64>,
    pub client_key: String,
    pub text: String,
    pub parent_post_id: Option<i64>,
    pub media_ids: Vec<i64>,
    pub descriptions: Vec<(i64, String)>,
    pub scheduled_at: Option<i64>,
    pub poll_options: String,
    pub poll_multiple: bool,
    pub poll_closes_at: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct Draft {
    pub id: i64,
    pub revision: i64,
    pub text: String,
    pub parent_post_id: Option<i64>,
    pub scheduled_at: Option<i64>,
    pub status: String,
    pub error: String,
    pub poll_options: String,
    pub poll_multiple: bool,
    pub poll_closes_at: Option<i64>,
    pub media: Vec<DraftMedia>,
}

#[derive(Debug, Clone)]
pub struct DraftMedia {
    pub id: i64,
    pub public_path: String,
    pub media_kind: String,
    pub alt_text: String,
    pub state: String,
}

pub fn parse_utc(value: &str) -> anyhow::Result<Option<i64>> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    if let Ok(date) = chrono::DateTime::parse_from_rfc3339(value) {
        return Ok(Some(date.timestamp()));
    }
    let date = chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M")?;
    Ok(Some(date.and_utc().timestamp()))
}

pub async fn save(
    pool: &SqlitePool,
    settings: &Settings,
    user_id: i64,
    input: DraftInput,
) -> anyhow::Result<i64> {
    if input.id.is_none()
        && !input.client_key.is_empty()
        && let Some(id) = id_for_key(pool, user_id, &input.client_key).await?
    {
        return Ok(id);
    }
    anyhow::ensure!(
        input.text.chars().count() <= settings.posts.max_text_chars,
        "post text is too long"
    );
    anyhow::ensure!(
        input.media_ids.len() <= settings.posts.max_media_per_post,
        "too many media attachments"
    );
    anyhow::ensure!(
        input.poll_options.chars().count() <= 606,
        "poll choices are too long"
    );
    if let Some(at) = input.scheduled_at {
        let poll = polls::PollSpec::parse(
            &input.poll_options,
            input.poll_multiple,
            input.poll_closes_at,
        )?;
        let now = chrono::Utc::now().timestamp();
        anyhow::ensure!(
            (now - 5..=now + 366 * 86_400).contains(&at),
            "schedule must be within the next year"
        );
        clean_post_text(
            &input.text,
            settings.posts.max_text_chars,
            input.media_ids.len(),
        )?;
        if let Some(close) = poll.and_then(|p| p.closes_at) {
            anyhow::ensure!(close > at, "poll must close after publication");
        }
    }
    let settings = settings.clone();
    pool.call(move |conn| {
        let tx=conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let active:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM users WHERE id=? AND is_deleted=0 AND is_suspended=0 AND deletion_requested_at IS NULL AND must_change_password=0)",[user_id],|r|r.get(0))?;
        anyhow::ensure!(active,"account cannot save posts");
        if let Some(parent)=input.parent_post_id {ensure_post_interaction_accessible_tx(&tx,user_id,parent)?;}
        let (images,videos)=posting::media_kind_counts_tx(&tx,Some(user_id),&input.media_ids)?;
        anyhow::ensure!(images<=settings.posts.max_images_per_post && videos<=settings.posts.max_videos_per_post,"too many attachments of this type");
        for id in &input.media_ids {
            let used:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM post_media WHERE media_id=? UNION ALL SELECT 1 FROM users WHERE profile_picture_media_id=? OR banner_media_id=? UNION ALL SELECT 1 FROM draft_media WHERE media_id=? AND draft_id IS NOT ?)",params![id,id,id,id,input.id],|r|r.get(0))?;
            anyhow::ensure!(!used,"attachment is already in use");
        }
        let status=if input.scheduled_at.is_some(){"scheduled"}else{"draft"};
        let id=if let Some(id)=input.id {
            let changed=tx.execute("UPDATE drafts SET text=?,parent_post_id=?,scheduled_at=?,status=?,revision=revision+1,error='',poll_options=?,poll_multiple=?,poll_closes_at=?,updated_at=CURRENT_TIMESTAMP WHERE id=? AND user_id=? AND revision=? AND status!='published'",params![input.text,input.parent_post_id,input.scheduled_at,status,input.poll_options,input.poll_multiple,input.poll_closes_at,id,user_id,input.revision])?;
            anyhow::ensure!(changed==1,"draft changed on another device or was published; reload before editing"); id
        } else {
            let key=if input.client_key.is_empty(){uuid::Uuid::new_v4().to_string()}else{input.client_key.clone()};
            anyhow::ensure!(key.len()<=64 && key.bytes().all(|b|b.is_ascii_alphanumeric() || b==b'-'),"invalid draft request key");
            if let Some(id)=tx.query_row("SELECT id FROM drafts WHERE user_id=? AND client_key=?",params![user_id,key],|r|r.get::<_,i64>(0)).optional()? {return Ok(id)}
            let count:i64=tx.query_row("SELECT COUNT(*) FROM drafts WHERE user_id=? AND status!='published'",[user_id],|r|r.get(0))?;
            anyhow::ensure!(count<200,"draft limit reached; delete an old draft first");
            tx.execute("INSERT INTO drafts(user_id,text,parent_post_id,scheduled_at,status,poll_options,poll_multiple,poll_closes_at,client_key) VALUES(?,?,?,?,?,?,?,?,?)",params![user_id,input.text,input.parent_post_id,input.scheduled_at,status,input.poll_options,input.poll_multiple,input.poll_closes_at,key])?;
            tx.last_insert_rowid()
        };
        tx.execute("DELETE FROM draft_media WHERE draft_id=?",[id])?;
        for (position,media_id) in input.media_ids.iter().enumerate() {
            tx.execute("INSERT INTO draft_media(draft_id,media_id,position) VALUES(?,?,?)",params![id,media_id,i64::try_from(position)?])?;
        }
        for (media_id,description) in input.descriptions {
            anyhow::ensure!(input.media_ids.contains(&media_id) && description.chars().count()<=1000,"invalid image description");
            tx.execute("UPDATE media SET alt_text=? WHERE id=? AND owner_user_id=?",params![description,media_id,user_id])?;
        }
        tx.commit()?; Ok(id)
    }).await
}

pub async fn id_for_key(pool: &SqlitePool, user: i64, key: &str) -> anyhow::Result<Option<i64>> {
    anyhow::ensure!(
        !key.is_empty()
            && key.len() <= 64
            && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'),
        "invalid draft request key"
    );
    let key = key.to_owned();
    pool.call(move |conn| {
        Ok(conn
            .query_row(
                "SELECT id FROM drafts WHERE user_id=? AND client_key=?",
                params![user, key],
                |r| r.get(0),
            )
            .optional()?)
    })
    .await
}

pub async fn list(pool: &SqlitePool, user_id: i64) -> anyhow::Result<Vec<Draft>> {
    pool.call(move |conn| {
        let mut statement=conn.prepare("SELECT id,revision,text,parent_post_id,scheduled_at,status,error,poll_options,poll_multiple,poll_closes_at FROM drafts WHERE user_id=? AND status!='published' ORDER BY id DESC LIMIT 200")?;
        let mut drafts=statement.query_map([user_id],|r|Ok(Draft{id:r.get(0)?,revision:r.get(1)?,text:r.get(2)?,parent_post_id:r.get(3)?,scheduled_at:r.get(4)?,status:r.get(5)?,error:r.get(6)?,poll_options:r.get(7)?,poll_multiple:r.get(8)?,poll_closes_at:r.get(9)?,media:Vec::new()}))?.collect::<Result<Vec<_>,_>>()?;
        for draft in &mut drafts {
            let mut media=conn.prepare("SELECT m.id,m.public_path,m.media_kind,m.alt_text,m.conversion_state FROM draft_media d JOIN media m ON m.id=d.media_id WHERE d.draft_id=? ORDER BY d.position")?;
            draft.media=media.query_map([draft.id],|r|Ok(DraftMedia{id:r.get(0)?,public_path:r.get(1)?,media_kind:r.get(2)?,alt_text:r.get(3)?,state:r.get(4)?}))?.collect::<Result<Vec<_>,_>>()?;
        }
        Ok(drafts)
    }).await
}

/// Cancellation and publication are serialized by SQLite; a cancelled draft cannot publish later.
pub async fn cancel(
    pool: &SqlitePool,
    user_id: i64,
    id: i64,
    revision: i64,
    delete: bool,
) -> anyhow::Result<Vec<i64>> {
    pool.call(move |conn| {
        let tx=conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let changed=tx.execute("UPDATE drafts SET status='draft',scheduled_at=NULL,error='',revision=revision+1,updated_at=CURRENT_TIMESTAMP WHERE id=? AND user_id=? AND revision=? AND status!='published'",params![id,user_id,revision])?;
        anyhow::ensure!(changed==1,"draft changed or was already published; reload to see its current state");
        let mut ids=Vec::new();
        if delete {
            let mut statement=tx.prepare("SELECT media_id FROM draft_media WHERE draft_id=?")?;
            ids=statement.query_map([id],|r|r.get(0))?.collect::<Result<Vec<_>,_>>()?;
            tx.execute("DELETE FROM drafts WHERE id=?",[id])?;
        }
        tx.commit()?; Ok(ids)
    }).await
}

/// All public rows, notifications and the idempotency marker commit together.
pub async fn publish(
    pool: &SqlitePool,
    settings: &Settings,
    id: i64,
) -> anyhow::Result<Option<i64>> {
    let settings = settings.clone();
    pool.call(move |conn| {
        let tx=conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let row=tx.query_row("SELECT user_id,text,parent_post_id,poll_options,poll_multiple,poll_closes_at,published_post_id,status,scheduled_at FROM drafts WHERE id=?",[id],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,Option<i64>>(2)?,r.get::<_,String>(3)?,r.get::<_,bool>(4)?,r.get::<_,Option<i64>>(5)?,r.get::<_,Option<i64>>(6)?,r.get::<_,String>(7)?,r.get::<_,Option<i64>>(8)?))).optional()?;
        let Some((user_id,text,parent_post_id,options,multiple,closes,published,status,at))=row else{return Ok(None)};
        if published.is_some(){return Ok(published)}
        if status!="scheduled" || at.is_none_or(|t|t>chrono::Utc::now().timestamp()) {return Ok(None)}
        let paused:bool=tx.query_row("SELECT COALESCE((SELECT value='1' FROM instance_settings WHERE key='maintenance_mode'),0)",[],|r|r.get(0))?;
        if paused {return Ok(None)}
        let mut statement=tx.prepare("SELECT media_id FROM draft_media WHERE draft_id=? ORDER BY position")?;
        let media_ids=statement.query_map([id],|r|r.get(0))?.collect::<Result<Vec<_>,_>>()?;
        drop(statement);
        tx.execute_batch("SAVEPOINT scheduled_publication")?;
        let result=(|| {
            let poll=polls::PollSpec::parse(&options,multiple,closes)?;
            let input=posting::PostInput {user_id:Some(user_id),youtube_embeds:crate::youtube::embeds_for_text(&text),text,parent_post_id,media_ids,poll,request:None,rate:Some(crate::rate_limit::Limit{scope:if parent_post_id.is_some(){crate::rate_limit::Scope::Reply}else{crate::rate_limit::Scope::Post},actor:format!("user:{user_id}"),max_events:if parent_post_id.is_some(){settings.moderation.replies_per_minute}else{settings.moderation.posts_per_minute},window_secs:60})};
            posting::insert_post_tx(&tx,&settings,&input)
        })();
        match result {
            Ok(post_id)=>{
                tx.execute_batch("RELEASE scheduled_publication")?;
                tx.execute("UPDATE drafts SET status='published',published_post_id=?,revision=revision+1,error='',updated_at=CURRENT_TIMESTAMP WHERE id=?",params![post_id,id])?;
                tx.execute("DELETE FROM draft_media WHERE draft_id=?",[id])?;
                tx.commit()?; Ok(Some(post_id))
            }
            Err(error)=>{
                tx.execute_batch("ROLLBACK TO scheduled_publication; RELEASE scheduled_publication")?;
                if error.is::<crate::rate_limit::LimitExceeded>() {
                    tx.execute("UPDATE drafts SET scheduled_at=unixepoch()+60,revision=revision+1,error='Waiting for the posting rate limit.' WHERE id=?",[id])?;
                    tx.commit()?;return Err(error)
                }
                tx.execute("UPDATE drafts SET status='failed',revision=revision+1,error='Publication failed. Check account status, attachment limits, reply target and poll close time.',updated_at=CURRENT_TIMESTAMP WHERE id=?",[id])?;
                tx.commit()?; Err(error)
            }
        }
    }).await
}

pub async fn publish_due(pool: &SqlitePool, settings: &Settings) -> anyhow::Result<()> {
    let ids=pool.call(|conn| {
        let mut statement=conn.prepare("SELECT id FROM drafts WHERE status='scheduled' AND scheduled_at<=unixepoch() ORDER BY scheduled_at,id LIMIT 16")?;
        Ok(statement.query_map([],|r|r.get::<_,i64>(0))?.collect::<Result<Vec<_>,_>>()?)
    }).await?;
    for id in ids {
        if let Err(error) = publish(pool, settings, id).await {
            tracing::warn!(draft_id=id,error=%error,"scheduled publication failed; retained for owner review");
        }
    }
    Ok(())
}
