//! `UserStore` implementation for `SqliteStore`.

use arca_core::error::ArcaError;
use arca_core::store::UserStore;
use arca_core::types::User;
use chrono::DateTime;
use rusqlite::params;

use super::{SqliteStore, TrError};

#[async_trait::async_trait]
impl UserStore for SqliteStore {
    async fn put_user(&self, user: &User) -> Result<(), ArcaError> {
        let u = user.clone();
        self.conn
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO users (user_id, username, description, is_root, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        u.user_id,
                        u.username,
                        u.description,
                        u.is_root as i32,
                        u.created_at.to_rfc3339(),
                    ],
                )?;
                Ok(())
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("put_user: {e}")))
    }

    async fn get_user(&self, user_id: &str) -> Result<Option<User>, ArcaError> {
        let id = user_id.to_string();
        self.conn
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT user_id, username, description, is_root, created_at
                     FROM users WHERE user_id = ?1",
                )?;
                let result = stmt.query_row(params![id], |row| Ok(row_to_user(row)));
                match result {
                    Ok(user) => Ok(Some(user?)),
                    Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                    Err(e) => Err(e.into()),
                }
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("get_user: {e}")))
    }

    async fn get_user_by_username(&self, username: &str) -> Result<Option<User>, ArcaError> {
        let name = username.to_string();
        self.conn
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT user_id, username, description, is_root, created_at
                     FROM users WHERE username = ?1",
                )?;
                let result = stmt.query_row(params![name], |row| Ok(row_to_user(row)));
                match result {
                    Ok(user) => Ok(Some(user?)),
                    Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                    Err(e) => Err(e.into()),
                }
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("get_user_by_username: {e}")))
    }

    async fn list_users(&self) -> Result<Vec<User>, ArcaError> {
        self.conn
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT user_id, username, description, is_root, created_at
                     FROM users ORDER BY created_at",
                )?;
                let rows = stmt.query_map([], |row| Ok(row_to_user(row)))?;
                let mut users = Vec::new();
                for row in rows {
                    users.push(row??);
                }
                Ok(users)
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("list_users: {e}")))
    }

    async fn update_user(&self, user_id: &str, description: &str) -> Result<bool, ArcaError> {
        let id = user_id.to_string();
        let desc = description.to_string();
        self.conn
            .call(move |conn| {
                let affected = conn.execute(
                    "UPDATE users SET description = ?1 WHERE user_id = ?2",
                    params![desc, id],
                )?;
                Ok(affected > 0)
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("update_user: {e}")))
    }

    async fn delete_user(&self, user_id: &str) -> Result<bool, ArcaError> {
        let id = user_id.to_string();
        self.conn
            .call(move |conn| {
                let tx = conn.transaction()?;
                // Remove memberships and grant attachments first
                tx.execute(
                    "DELETE FROM team_members WHERE user_id = ?1",
                    params![id],
                )?;
                tx.execute(
                    "DELETE FROM user_grants WHERE user_id = ?1",
                    params![id],
                )?;
                let affected = tx.execute(
                    "DELETE FROM users WHERE user_id = ?1",
                    params![id],
                )?;
                tx.commit()?;
                Ok(affected > 0)
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("delete_user: {e}")))
    }
}

pub(crate) fn row_to_user(row: &rusqlite::Row) -> Result<User, rusqlite::Error> {
    let created_at_str: String = row.get(4)?;
    let is_root_int: i32 = row.get(3)?;

    let created_at = DateTime::parse_from_rfc3339(&created_at_str)
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                4,
                rusqlite::types::Type::Text,
                Box::new(e),
            )
        })?;

    Ok(User {
        user_id: row.get(0)?,
        username: row.get(1)?,
        description: row.get(2)?,
        is_root: is_root_int != 0,
        created_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    async fn test_store() -> SqliteStore {
        SqliteStore::open_in_memory().await.unwrap()
    }

    fn make_user(user_id: &str, username: &str) -> User {
        User {
            user_id: user_id.to_string(),
            username: username.to_string(),
            description: String::new(),
            is_root: false,
            created_at: Utc::now(),
        }
    }

    #[tokio::test]
    async fn root_user_exists_after_migration() {
        let store = test_store().await;
        let root = store.get_user("root").await.unwrap().unwrap();
        assert_eq!(root.username, "root");
        assert!(root.is_root);
    }

    #[tokio::test]
    async fn put_and_get_user() {
        let store = test_store().await;
        let user = make_user("u1", "alice");
        store.put_user(&user).await.unwrap();

        let fetched = store.get_user("u1").await.unwrap().unwrap();
        assert_eq!(fetched.username, "alice");
        assert!(!fetched.is_root);
    }

    #[tokio::test]
    async fn get_user_by_username() {
        let store = test_store().await;
        let user = make_user("u2", "bob");
        store.put_user(&user).await.unwrap();

        let fetched = store.get_user_by_username("bob").await.unwrap().unwrap();
        assert_eq!(fetched.user_id, "u2");
    }

    #[tokio::test]
    async fn get_nonexistent_returns_none() {
        let store = test_store().await;
        assert!(store.get_user("nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn list_users() {
        let store = test_store().await;
        // root already exists from migration
        store.put_user(&make_user("u3", "carol")).await.unwrap();
        let users = store.list_users().await.unwrap();
        assert!(users.len() >= 2); // root + carol
    }

    #[tokio::test]
    async fn update_user_description() {
        let store = test_store().await;
        store.put_user(&make_user("u4", "dave")).await.unwrap();
        assert!(store.update_user("u4", "new desc").await.unwrap());
        let fetched = store.get_user("u4").await.unwrap().unwrap();
        assert_eq!(fetched.description, "new desc");
    }

    #[tokio::test]
    async fn update_nonexistent_returns_false() {
        let store = test_store().await;
        assert!(!store.update_user("nope", "x").await.unwrap());
    }

    #[tokio::test]
    async fn delete_user() {
        let store = test_store().await;
        store.put_user(&make_user("u5", "eve")).await.unwrap();
        assert!(store.delete_user("u5").await.unwrap());
        assert!(store.get_user("u5").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn delete_nonexistent_returns_false() {
        let store = test_store().await;
        assert!(!store.delete_user("nope").await.unwrap());
    }

    #[tokio::test]
    async fn duplicate_username_fails() {
        let store = test_store().await;
        store.put_user(&make_user("u6", "frank")).await.unwrap();
        let dup = make_user("u7", "frank");
        assert!(store.put_user(&dup).await.is_err());
    }

    #[tokio::test]
    async fn duplicate_user_id_fails() {
        let store = test_store().await;
        store.put_user(&make_user("u8", "grace")).await.unwrap();
        let dup = make_user("u8", "heidi");
        assert!(store.put_user(&dup).await.is_err());
    }
}
