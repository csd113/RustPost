//! Notifications for the social subsystem.

use super::{
    Connection, NOTIFICATION_VISIBLE_SQL, NotificationActorView, NotificationGroupKey,
    NotificationGroupTarget, NotificationGroupView, NotificationView, SqlitePool,
    VIEWER_FILTER_BINDINGS, params, params_from_iter,
};

pub(super) fn push_notification_filter_bindings(bindings: &mut Vec<i64>, user_id: i64) {
    bindings.extend(std::iter::repeat_n(user_id, VIEWER_FILTER_BINDINGS));
}

pub async fn notifications(
    pool: &SqlitePool,
    user_id: i64,
) -> anyhow::Result<Vec<NotificationView>> {
    pool.call(move |conn| {
        let sql = format!(
            r#"
            SELECT n.id, n.kind, n.actor_user_id, u.username, u.display_name,
              n.post_id, p.text, p.is_deleted, p.parent_post_id, p.quote_post_id,
              n.read_at, n.created_at
            FROM notifications n
            LEFT JOIN users u ON u.id = n.actor_user_id AND u.is_deleted = 0
            LEFT JOIN posts p ON p.id = n.post_id
            WHERE n.user_id = ?
              AND (
                n.kind != 'like'
                OR n.actor_user_id IS NULL
                OR COALESCE(u.liked_posts_public, 0) != 0
              )
              {NOTIFICATION_VISIBLE_SQL}
            ORDER BY n.id DESC
            LIMIT 80
            "#,
        );
        let mut stmt = conn.prepare(&sql)?;
        let mut bindings = vec![user_id];
        push_notification_filter_bindings(&mut bindings, user_id);
        let rows = stmt
            .query_map(params_from_iter(bindings), |row| {
                let kind = row.get::<_, String>(1)?;
                let post_id = row.get::<_, Option<i64>>(5)?;
                let post_is_deleted = row
                    .get::<_, Option<i64>>(7)?
                    .is_some_and(|value| value != 0);
                let parent_post_id = row.get::<_, Option<i64>>(8)?;
                let quote_post_id = row.get::<_, Option<i64>>(9)?;
                let group_target_post_id = match kind.as_str() {
                    "reply" => parent_post_id.or(post_id),
                    "quote" => quote_post_id.or(post_id),
                    _ => post_id,
                };
                Ok(NotificationView {
                    id: row.get(0)?,
                    kind,
                    actor_user_id: row.get(2)?,
                    actor_username: row.get(3)?,
                    actor_display_name: row.get(4)?,
                    post_id,
                    group_target_post_id,
                    post_text: row.get(6)?,
                    post_available: row
                        .get::<_, Option<i64>>(7)?
                        .is_some_and(|_| !post_is_deleted),
                    read_at: row.get(10)?,
                    created_at: row.get(11)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })
    .await
}

pub async fn notification_groups(
    pool: &SqlitePool,
    user_id: i64,
) -> anyhow::Result<Vec<NotificationGroupView>> {
    let notifications = notifications(pool, user_id).await?;
    let mut groups = group_notifications(&notifications);
    refresh_notification_group_counts(pool, user_id, &mut groups).await?;
    Ok(groups)
}

pub(super) fn group_notifications(
    notifications: &[NotificationView],
) -> Vec<NotificationGroupView> {
    let mut groups: Vec<(NotificationGroupKey, NotificationGroupView)> = Vec::new();
    for notification in notifications {
        let key = notification_group_key(notification);
        if let Some((_key, group)) = groups
            .iter_mut()
            .find(|(existing_key, _group)| *existing_key == key)
        {
            push_notification_group_item(group, notification);
        } else {
            groups.push((key, notification_group_from_item(notification)));
        }
    }
    groups.into_iter().map(|(_key, group)| group).collect()
}

pub(super) fn notification_group_key(notification: &NotificationView) -> NotificationGroupKey {
    let target = match notification.kind.as_str() {
        "follow" => NotificationGroupTarget::Follow,
        "like" | "repost" | "reply" | "mention" | "quote" => {
            notification.group_target_post_id.map_or(
                NotificationGroupTarget::Notification(notification.id),
                NotificationGroupTarget::Post,
            )
        }
        _ => NotificationGroupTarget::Notification(notification.id),
    };
    NotificationGroupKey {
        kind: notification.kind.clone(),
        target,
    }
}

pub(super) fn notification_group_from_item(
    notification: &NotificationView,
) -> NotificationGroupView {
    let mut group = NotificationGroupView {
        id: notification.id,
        kind: notification.kind.clone(),
        group_target_post_id: notification.group_target_post_id,
        post_id: notification.post_id,
        post_text: notification.post_text.clone(),
        post_available: notification.post_available,
        unread_count: 0,
        total_count: 0,
        notification_ids: Vec::new(),
        actors: Vec::new(),
        created_at: notification.created_at.clone(),
    };
    push_notification_group_item(&mut group, notification);
    group
}

pub(super) fn push_notification_group_item(
    group: &mut NotificationGroupView,
    notification: &NotificationView,
) {
    group.total_count += 1;
    group.notification_ids.push(notification.id);
    if notification.read_at.is_none() {
        group.unread_count += 1;
    }
    group.actors.push(NotificationActorView {
        user_id: notification.actor_user_id,
        username: notification.actor_username.clone(),
        display_name: notification.actor_display_name.clone(),
    });
}

pub async fn unread_notification_count(pool: &SqlitePool, user_id: i64) -> anyhow::Result<i64> {
    pool.call(move |conn| {
        let sql = format!(
            r#"
            SELECT COUNT(*)
            FROM notifications n
            LEFT JOIN users u ON u.id = n.actor_user_id AND u.is_deleted = 0
            LEFT JOIN posts p ON p.id = n.post_id
            WHERE n.user_id = ? AND n.read_at IS NULL
              AND (
                n.kind != 'like'
                OR n.actor_user_id IS NULL
                OR COALESCE(u.liked_posts_public, 0) != 0
              )
              {NOTIFICATION_VISIBLE_SQL}
            "#,
        );
        let mut bindings = vec![user_id];
        push_notification_filter_bindings(&mut bindings, user_id);
        Ok(conn.query_row(&sql, params_from_iter(bindings), |row| row.get(0))?)
    })
    .await
}

pub async fn mark_notification_ids_read(
    pool: &SqlitePool,
    user_id: i64,
    notification_ids: &[i64],
) -> anyhow::Result<()> {
    let notification_ids = notification_ids.to_vec();
    pool.call(move |conn| {
        let tx = conn.transaction()?;
        for notification_id in notification_ids {
            tx.execute(
                "UPDATE notifications SET read_at = CURRENT_TIMESTAMP WHERE user_id = ? AND id = ? AND read_at IS NULL",
                params![user_id, notification_id],
            )?;
        }
        tx.commit()?;
        Ok(())
    })
    .await
}

pub(super) async fn refresh_notification_group_counts(
    pool: &SqlitePool,
    user_id: i64,
    groups: &mut [NotificationGroupView],
) -> anyhow::Result<()> {
    let descriptors = groups
        .iter()
        .enumerate()
        .filter(|(_index, group)| {
            group.kind == "follow"
                || (notification_group_target_expr(&group.kind).is_some()
                    && group.group_target_post_id.is_some())
        })
        .map(|(index, group)| (index, group.kind.clone(), group.group_target_post_id))
        .collect::<Vec<_>>();
    let counts = pool
        .call(move |conn| {
            let mut counts = Vec::with_capacity(descriptors.len());
            for (index, kind, target_post_id) in descriptors {
                let (total_count, unread_count) =
                    notification_group_counts_tx(conn, user_id, &kind, target_post_id)?;
                counts.push((index, total_count, unread_count));
            }
            Ok(counts)
        })
        .await?;
    for (index, total_count, unread_count) in counts {
        groups[index].total_count = usize::try_from(total_count)?;
        groups[index].unread_count = unread_count;
    }
    Ok(())
}

pub(super) fn notification_group_counts_tx(
    conn: &Connection,
    user_id: i64,
    kind: &str,
    target_post_id: Option<i64>,
) -> anyhow::Result<(i64, i64)> {
    if kind == "follow" {
        let sql = format!(
            r#"
            SELECT COUNT(*), COALESCE(SUM(CASE WHEN n.read_at IS NULL THEN 1 ELSE 0 END), 0)
            FROM notifications n
            LEFT JOIN users actor ON actor.id = n.actor_user_id AND actor.is_deleted = 0
            LEFT JOIN posts p ON p.id = n.post_id
            WHERE n.user_id = ? AND n.kind = 'follow'
              {NOTIFICATION_VISIBLE_SQL}
            "#
        );
        let mut bindings = vec![user_id];
        push_notification_filter_bindings(&mut bindings, user_id);
        return Ok(conn.query_row(&sql, params_from_iter(bindings), |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?);
    }
    let Some(target_expr) = notification_group_target_expr(kind) else {
        return Ok((0, 0));
    };
    let Some(target_post_id) = target_post_id else {
        return Ok((0, 0));
    };
    let sql = format!(
        r#"
        SELECT COUNT(*), COALESCE(SUM(CASE WHEN n.read_at IS NULL THEN 1 ELSE 0 END), 0)
        FROM notifications n
        LEFT JOIN users actor ON actor.id = n.actor_user_id AND actor.is_deleted = 0
        LEFT JOIN posts p ON p.id = n.post_id
        WHERE n.user_id = ? AND n.kind = ? AND {target_expr} = ?
          AND (
            n.kind != 'like'
            OR n.actor_user_id IS NULL
            OR COALESCE(actor.liked_posts_public, 0) != 0
          )
          {NOTIFICATION_VISIBLE_SQL}
        "#
    );
    let mut bindings = vec![
        rusqlite::types::Value::Integer(user_id),
        rusqlite::types::Value::Text(kind.to_owned()),
        rusqlite::types::Value::Integer(target_post_id),
    ];
    bindings.extend(std::iter::repeat_n(
        rusqlite::types::Value::Integer(user_id),
        VIEWER_FILTER_BINDINGS,
    ));
    Ok(conn.query_row(&sql, params_from_iter(bindings), |row| {
        Ok((row.get(0)?, row.get(1)?))
    })?)
}

pub(super) fn notification_group_target_expr(kind: &str) -> Option<&'static str> {
    match kind {
        "like" | "repost" | "mention" => Some("n.post_id"),
        "reply" => Some("COALESCE(p.parent_post_id, n.post_id)"),
        "quote" => Some("COALESCE(p.quote_post_id, n.post_id)"),
        _ => None,
    }
}

pub async fn mark_notifications_read(pool: &SqlitePool, user_id: i64) -> anyhow::Result<()> {
    pool.call(move |conn| {
        conn.execute(
            "UPDATE notifications SET read_at = CURRENT_TIMESTAMP WHERE user_id = ? AND read_at IS NULL",
            [user_id],
        )?;
        Ok(())
    })
    .await
}

pub async fn mark_notification_group_read(
    pool: &SqlitePool,
    user_id: i64,
    kind: &str,
    target_post_id: Option<i64>,
) -> anyhow::Result<()> {
    let kind = kind.to_owned();
    pool.call(move |conn| {
        if kind == "follow" {
            conn.execute(
                "UPDATE notifications SET read_at = CURRENT_TIMESTAMP WHERE user_id = ? AND kind = 'follow' AND read_at IS NULL",
                [user_id],
            )?;
            return Ok(());
        }
        let Some(target_expr) = notification_group_target_expr(&kind) else {
            anyhow::bail!("notification group kind is invalid");
        };
        let Some(target_post_id) = target_post_id else {
            anyhow::bail!("notification group target is invalid");
        };
        let sql = format!(
            r#"
            UPDATE notifications
            SET read_at = CURRENT_TIMESTAMP
            WHERE user_id = ? AND read_at IS NULL AND id IN (
              SELECT n.id
              FROM notifications n
              LEFT JOIN posts p ON p.id = n.post_id
              WHERE n.user_id = ? AND n.kind = ? AND {target_expr} = ?
            )
            "#
        );
        conn.execute(&sql, params![user_id, user_id, kind, target_post_id])?;
        Ok(())
    })
    .await
}
