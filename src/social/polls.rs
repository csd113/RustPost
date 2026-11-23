//! Account-bound polls. Public views expose counts and only the viewer's own ballot.

use super::{
    BTreeSet, Connection, HashMap, SqlitePool, ensure_post_interaction_accessible_tx, params,
    params_from_iter, query,
};

#[derive(Debug, Clone, Default)]
pub struct PollSpec {
    pub options: Vec<String>,
    pub multiple: bool,
    pub closes_at: Option<i64>,
}

impl PollSpec {
    pub fn parse(
        options: &str,
        multiple: bool,
        closes_at: Option<i64>,
    ) -> anyhow::Result<Option<Self>> {
        let options = options
            .lines()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>();
        if options.is_empty() {
            return Ok(None);
        }
        anyhow::ensure!(
            (2..=6).contains(&options.len()),
            "a poll needs 2 to 6 choices"
        );
        let mut unique = BTreeSet::new();
        for option in &options {
            anyhow::ensure!(
                option.chars().count() <= 100,
                "poll choices must be at most 100 characters"
            );
            anyhow::ensure!(
                unique.insert(option.to_lowercase()),
                "poll choices must be different"
            );
        }
        if let Some(close) = closes_at {
            let now = chrono::Utc::now().timestamp();
            anyhow::ensure!(
                close > now && close <= now + 366 * 86_400,
                "poll close time must be within the next year"
            );
        }
        Ok(Some(Self {
            options,
            multiple,
            closes_at,
        }))
    }
}

#[derive(Debug, Clone)]
pub struct PollChoice {
    pub id: i64,
    pub label: String,
    pub votes: i64,
    pub selected: bool,
}

#[derive(Debug, Clone)]
pub struct PollView {
    pub multiple: bool,
    pub closes_at: Option<i64>,
    pub closed: bool,
    pub voters: i64,
    pub choices: Vec<PollChoice>,
}

pub(super) fn insert_tx(
    tx: &rusqlite::Transaction<'_>,
    post_id: i64,
    spec: &PollSpec,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        (2..=6).contains(&spec.options.len()),
        "invalid poll choices"
    );
    tx.execute(
        "INSERT INTO polls(post_id,multiple,closes_at) VALUES(?,?,?)",
        params![post_id, spec.multiple, spec.closes_at],
    )?;
    for (position, label) in spec.options.iter().enumerate() {
        tx.execute(
            "INSERT INTO poll_options(post_id,label,position) VALUES(?,?,?)",
            params![post_id, label, i64::try_from(position)?],
        )?;
    }
    Ok(())
}

pub(super) fn for_posts(
    conn: &Connection,
    ids: &[i64],
    viewer: Option<i64>,
) -> anyhow::Result<HashMap<i64, PollView>> {
    let mut polls = HashMap::new();
    if ids.is_empty() {
        return Ok(polls);
    }
    let sql = format!(
        "SELECT p.post_id,p.multiple,p.closes_at,(SELECT COUNT(DISTINCT user_id) FROM poll_votes v WHERE v.post_id=p.post_id),o.id,o.label,(SELECT COUNT(*) FROM poll_votes v WHERE v.option_id=o.id),EXISTS(SELECT 1 FROM poll_votes v WHERE v.option_id=o.id AND v.user_id=?) FROM polls p JOIN poll_options o ON o.post_id=p.post_id WHERE p.post_id IN ({}) ORDER BY p.post_id,o.position",
        query::sql_placeholders(ids.len())
    );
    let mut bindings = vec![viewer.unwrap_or(-1)];
    bindings.extend_from_slice(ids);
    let mut statement = conn.prepare(&sql)?;
    let rows = statement.query_map(params_from_iter(bindings), |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, bool>(1)?,
            row.get::<_, Option<i64>>(2)?,
            row.get::<_, i64>(3)?,
            PollChoice {
                id: row.get(4)?,
                label: row.get(5)?,
                votes: row.get(6)?,
                selected: row.get(7)?,
            },
        ))
    })?;
    for row in rows {
        let (id, multiple, closes_at, voters, choice) = row?;
        polls
            .entry(id)
            .or_insert_with(|| PollView {
                multiple,
                closes_at,
                closed: closes_at.is_some_and(|t| t <= chrono::Utc::now().timestamp()),
                voters,
                choices: Vec::new(),
            })
            .choices
            .push(choice);
    }
    Ok(polls)
}

/// Atomically replaces an account's ballot; retries cannot add extra votes.
pub async fn vote(
    pool: &SqlitePool,
    user_id: i64,
    post_id: i64,
    choices: Vec<i64>,
) -> anyhow::Result<()> {
    anyhow::ensure!(choices.len() <= 6, "too many poll choices");
    let choices = choices.into_iter().collect::<BTreeSet<_>>();
    pool.call(move |conn| {
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        ensure_post_interaction_accessible_tx(&tx,user_id,post_id)?;
        let active: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM users WHERE id=? AND is_deleted=0 AND is_suspended=0 AND deletion_requested_at IS NULL)",[user_id],|r|r.get(0))?;
        anyhow::ensure!(active, "account cannot vote");
        let (multiple, closes): (bool,Option<i64>) = tx.query_row("SELECT multiple,closes_at FROM polls WHERE post_id=?",[post_id],|r|Ok((r.get(0)?,r.get(1)?)))?;
        anyhow::ensure!(closes.is_none_or(|t|t>chrono::Utc::now().timestamp()), "poll is closed");
        anyhow::ensure!(multiple || choices.len() <= 1, "choose only one option");
        for choice in &choices {
            let valid: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM poll_options WHERE id=? AND post_id=?)",params![choice,post_id],|r|r.get(0))?;
            anyhow::ensure!(valid, "invalid poll choice");
        }
        tx.execute("DELETE FROM poll_votes WHERE post_id=? AND user_id=?",params![post_id,user_id])?;
        for choice in choices { tx.execute("INSERT INTO poll_votes(post_id,user_id,option_id) VALUES(?,?,?)",params![post_id,user_id,choice])?; }
        tx.commit()?;
        Ok(())
    }).await
}

pub async fn close(pool: &SqlitePool, user_id: i64, post_id: i64) -> anyhow::Result<()> {
    pool.call(move |conn| {
        let count=conn.execute("UPDATE polls SET closes_at=unixepoch() WHERE post_id=? AND EXISTS(SELECT 1 FROM posts WHERE id=? AND user_id=? AND is_deleted=0)",params![post_id,post_id,user_id])?;
        anyhow::ensure!(count==1,"poll not found"); Ok(())
    }).await
}
