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
                    "INSERT INTO users (user_id, username, description, is_root, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        u.user_id,
                        u.username,
                        u.description,
                        u.is_root as i32,
                        u.created_at.to_rfc3339(),
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
        self.read_conn()
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
        self.read_conn()
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
        self.read_conn()
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

    async fn update_user(
        &self,
        user_id: &str,
        username: Option<&str>,
        description: Option<&str>,
    ) -> Result<bool, ArcaError> {
        let id = user_id.to_string();
        let username = username.map(|s| s.to_string());
        let description = description.map(|s| s.to_string());
        self.conn
            .call(move |conn| {
                let mut sets = Vec::new();
                let mut values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
                if let Some(ref u) = username {
                    sets.push("username = ?");
                    values.push(Box::new(u.clone()));
                }
                if let Some(ref d) = description {
                    sets.push("description = ?");
                    values.push(Box::new(d.clone()));
                }
                if sets.is_empty() {
                    let exists: bool = conn.query_row(
                        "SELECT 1 FROM users WHERE user_id = ?1",
                        params![id],
                        |_| Ok(true),
                    ).unwrap_or(false);
                    return Ok(exists);
                }
                // Bump the LWW timestamp on any real change.
                sets.push("updated_at = ?");
                values.push(Box::new(chrono::Utc::now().to_rfc3339()));
                let sql = format!(
                    "UPDATE users SET {} WHERE user_id = ?",
                    sets.join(", ")
                );
                values.push(Box::new(id));
                let params: Vec<&dyn rusqlite::types::ToSql> = values.iter().map(|v| v.as_ref()).collect();
                let affected = conn.execute(&sql, params.as_slice())?;
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
                // Cascade credential deletion: a credential must never outlive
                // its user, otherwise it would authenticate to a non-existent
                // identity (which the auth layer now denies).
                tx.execute(
                    "DELETE FROM credentials WHERE user_id = ?1",
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

    async fn apply_remote_user(&self, user: &User) -> Result<(), ArcaError> {
        let u = user.clone();
        self.conn
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO users (user_id, username, description, is_root, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                     ON CONFLICT(user_id) DO UPDATE SET
                       username = excluded.username,
                       description = excluded.description,
                       is_root = excluded.is_root,
                       created_at = excluded.created_at,
                       updated_at = excluded.updated_at",
                    params![
                        u.user_id,
                        u.username,
                        u.description,
                        u.is_root as i32,
                        u.created_at.to_rfc3339(),
                        chrono::Utc::now().to_rfc3339(),
                    ],
                )?;
                Ok(())
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("apply_remote_user: {e}")))
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
        assert!(store.update_user("u4", None, Some("new desc")).await.unwrap());
        let fetched = store.get_user("u4").await.unwrap().unwrap();
        assert_eq!(fetched.description, "new desc");
        assert_eq!(fetched.username, "dave"); // unchanged
    }

    #[tokio::test]
    async fn update_user_username() {
        let store = test_store().await;
        store.put_user(&make_user("u4b", "dave2")).await.unwrap();
        assert!(store.update_user("u4b", Some("dave_renamed"), None).await.unwrap());
        let fetched = store.get_user("u4b").await.unwrap().unwrap();
        assert_eq!(fetched.username, "dave_renamed");
        // Also findable by new username
        let by_name = store.get_user_by_username("dave_renamed").await.unwrap();
        assert!(by_name.is_some());
        // Old username no longer resolves
        let old = store.get_user_by_username("dave2").await.unwrap();
        assert!(old.is_none());
    }

    #[tokio::test]
    async fn update_nonexistent_returns_false() {
        let store = test_store().await;
        assert!(!store.update_user("nope", None, Some("x")).await.unwrap());
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

    /// A credential must never outlive its user: the auth layer fails closed on
    /// a credential whose `user_id` no longer resolves, so a dangling
    /// credential would be a permanently unusable (and confusing) access key.
    #[tokio::test]
    async fn delete_user_cascades_to_its_credentials() {
        use arca_core::store::CredentialStore;
        use arca_core::types::Credential;

        let store = test_store().await;
        store.put_user(&make_user("u-cascade", "grace")).await.unwrap();

        let cred = Credential {
            access_key_id: "AKIACASCADE".to_string(),
            secret_access_key: "secret".to_string(),
            description: "grace's key".to_string(),
            created_at: Utc::now(),
            active: true,
            user_id: "u-cascade".to_string(),
        };
        store.put_credential(&cred).await.unwrap();
        assert!(store.get_credential("AKIACASCADE").await.unwrap().is_some());

        assert!(store.delete_user("u-cascade").await.unwrap());

        assert!(store.get_user("u-cascade").await.unwrap().is_none());
        assert!(
            store.get_credential("AKIACASCADE").await.unwrap().is_none(),
            "the deleted user's credential must be gone, not dangling"
        );
    }

    /// The cascade is scoped to the deleted user — another user's credentials
    /// must survive.
    #[tokio::test]
    async fn delete_user_leaves_other_users_credentials_intact() {
        use arca_core::store::CredentialStore;
        use arca_core::types::Credential;

        let store = test_store().await;
        store.put_user(&make_user("u-gone", "heidi")).await.unwrap();
        store.put_user(&make_user("u-stays", "ivan")).await.unwrap();

        for (key, owner) in [("AKIAGONE", "u-gone"), ("AKIASTAYS", "u-stays")] {
            store
                .put_credential(&Credential {
                    access_key_id: key.to_string(),
                    secret_access_key: "secret".to_string(),
                    description: String::new(),
                    created_at: Utc::now(),
                    active: true,
                    user_id: owner.to_string(),
                })
                .await
                .unwrap();
        }

        assert!(store.delete_user("u-gone").await.unwrap());

        assert!(store.get_credential("AKIAGONE").await.unwrap().is_none());
        assert!(store.get_credential("AKIASTAYS").await.unwrap().is_some());
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

    #[tokio::test]
    async fn apply_remote_user_upserts_and_is_idempotent() {
        let store = test_store().await;
        let user = make_user("ru", "alice");
        // Verbatim insert with no prior put_user.
        store.apply_remote_user(&user).await.unwrap();
        assert_eq!(
            store.get_user("ru").await.unwrap().unwrap().username,
            "alice"
        );

        // Re-deliver with an updated username: overwrites in place, no error.
        let mut u2 = user.clone();
        u2.username = "alice2".to_string();
        store.apply_remote_user(&u2).await.unwrap();
        assert_eq!(
            store.get_user("ru").await.unwrap().unwrap().username,
            "alice2"
        );
    }
}
