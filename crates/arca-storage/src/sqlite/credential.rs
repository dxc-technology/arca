//! `CredentialStore` implementation for `SqliteStore`.

use arca_core::error::ArcaError;
use arca_core::store::CredentialStore;
use arca_core::types::Credential;
use chrono::DateTime;
use rusqlite::params;

use super::{SqliteStore, TrError};

#[async_trait::async_trait]
impl CredentialStore for SqliteStore {
    async fn put_credential(&self, credential: &Credential) -> Result<(), ArcaError> {
        let cred = credential.clone();
        self.conn
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO credentials (access_key_id, secret_access_key, description, created_at, active, admin, user_id)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        cred.access_key_id,
                        cred.secret_access_key,
                        cred.description,
                        cred.created_at.to_rfc3339(),
                        cred.active as i32,
                        cred.admin as i32,
                        cred.user_id,
                    ],
                )?;
                Ok(())
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("put_credential: {e}")))
    }

    async fn get_credential(
        &self,
        access_key_id: &str,
    ) -> Result<Option<Credential>, ArcaError> {
        let key = access_key_id.to_string();
        self.conn
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT access_key_id, secret_access_key, description, created_at, active, admin, user_id
                     FROM credentials WHERE access_key_id = ?1",
                )?;
                let result = stmt.query_row(params![key], |row| {
                    Ok(row_to_credential(row))
                });
                match result {
                    Ok(cred) => Ok(Some(cred?)),
                    Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                    Err(e) => Err(e.into()),
                }
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("get_credential: {e}")))
    }

    async fn list_credentials(&self) -> Result<Vec<Credential>, ArcaError> {
        self.conn
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT access_key_id, secret_access_key, description, created_at, active, admin, user_id
                     FROM credentials ORDER BY created_at",
                )?;
                let rows = stmt.query_map([], |row| Ok(row_to_credential(row)))?;
                let mut creds = Vec::new();
                for row in rows {
                    creds.push(row??);
                }
                Ok(creds)
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("list_credentials: {e}")))
    }

    async fn delete_credential(&self, access_key_id: &str) -> Result<bool, ArcaError> {
        let key = access_key_id.to_string();
        self.conn
            .call(move |conn| {
                let affected = conn.execute(
                    "DELETE FROM credentials WHERE access_key_id = ?1",
                    params![key],
                )?;
                Ok(affected > 0)
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("delete_credential: {e}")))
    }

    async fn update_credential(
        &self,
        access_key_id: &str,
        active: Option<bool>,
        description: Option<&str>,
    ) -> Result<bool, ArcaError> {
        let key = access_key_id.to_string();
        let active = active;
        let description = description.map(|s| s.to_string());
        self.conn
            .call(move |conn| {
                let mut sets = Vec::new();
                let mut values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
                if let Some(a) = active {
                    sets.push("active = ?");
                    values.push(Box::new(a as i32));
                }
                if let Some(ref d) = description {
                    sets.push("description = ?");
                    values.push(Box::new(d.clone()));
                }
                if sets.is_empty() {
                    // Nothing to update, just check existence.
                    let exists: bool = conn.query_row(
                        "SELECT 1 FROM credentials WHERE access_key_id = ?1",
                        params![key],
                        |_| Ok(true),
                    ).unwrap_or(false);
                    return Ok(exists);
                }
                let sql = format!(
                    "UPDATE credentials SET {} WHERE access_key_id = ?",
                    sets.join(", ")
                );
                values.push(Box::new(key));
                let params: Vec<&dyn rusqlite::types::ToSql> = values.iter().map(|v| v.as_ref()).collect();
                let affected = conn.execute(&sql, params.as_slice())?;
                Ok(affected > 0)
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("update_credential: {e}")))
    }

    async fn count_active_credentials(&self) -> Result<u64, ArcaError> {
        self.conn
            .call(move |conn| {
                let count: u64 = conn.query_row(
                    "SELECT COUNT(*) FROM credentials WHERE active = 1",
                    [],
                    |row| row.get(0),
                )?;
                Ok(count)
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("count_active_credentials: {e}")))
    }
}

/// Converts a SQLite row to a `Credential`.
///
/// Expects columns: access_key_id, secret_access_key, description, created_at, active, admin, user_id.
fn row_to_credential(row: &rusqlite::Row) -> Result<Credential, rusqlite::Error> {
    let created_at_str: String = row.get(3)?;
    let active_int: i32 = row.get(4)?;
    let admin_int: i32 = row.get(5)?;

    let created_at = DateTime::parse_from_rfc3339(&created_at_str)
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                3,
                rusqlite::types::Type::Text,
                Box::new(e),
            )
        })?;

    Ok(Credential {
        access_key_id: row.get(0)?,
        secret_access_key: row.get(1)?,
        description: row.get(2)?,
        created_at,
        active: active_int != 0,
        admin: admin_int != 0,
        user_id: row.get(6)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    async fn test_store() -> SqliteStore {
        SqliteStore::open_in_memory().await.unwrap()
    }

    #[tokio::test]
    async fn put_and_get_credential() {
        let store = test_store().await;
        let cred = Credential {
            access_key_id: "AKIAIOSFODNN7EXAMPLE".to_string(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".to_string(),
            description: "test key".to_string(),
            created_at: Utc::now(),
            active: true,
            admin: true,
            user_id: "root".to_string(),
        };

        store.put_credential(&cred).await.unwrap();

        let fetched = store
            .get_credential("AKIAIOSFODNN7EXAMPLE")
            .await
            .unwrap()
            .expect("credential should exist");

        assert_eq!(fetched.access_key_id, cred.access_key_id);
        assert_eq!(fetched.secret_access_key, cred.secret_access_key);
        assert_eq!(fetched.description, "test key");
        assert!(fetched.active);
        assert!(fetched.admin);
    }

    #[tokio::test]
    async fn get_nonexistent_returns_none() {
        let store = test_store().await;
        let result = store.get_credential("DOESNOTEXIST").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn list_empty() {
        let store = test_store().await;
        let creds = store.list_credentials().await.unwrap();
        assert!(creds.is_empty());
    }

    #[tokio::test]
    async fn list_all() {
        let store = test_store().await;

        for i in 0..3 {
            let cred = Credential {
                access_key_id: format!("KEY{i}"),
                secret_access_key: format!("SECRET{i}"),
                description: format!("key {i}"),
                created_at: Utc::now(),
                active: true,
                admin: false,
                user_id: "root".to_string(),
            };
            store.put_credential(&cred).await.unwrap();
        }

        let creds = store.list_credentials().await.unwrap();
        assert_eq!(creds.len(), 3);
    }

    #[tokio::test]
    async fn delete_credential() {
        let store = test_store().await;
        let cred = Credential {
            access_key_id: "TODELETE".to_string(),
            secret_access_key: "SECRET".to_string(),
            description: String::new(),
            created_at: Utc::now(),
            active: true,
            admin: false,
            user_id: "root".to_string(),
        };
        store.put_credential(&cred).await.unwrap();

        let deleted = store.delete_credential("TODELETE").await.unwrap();
        assert!(deleted);

        let fetched = store.get_credential("TODELETE").await.unwrap();
        assert!(fetched.is_none());
    }

    #[tokio::test]
    async fn delete_nonexistent_returns_false() {
        let store = test_store().await;
        let deleted = store.delete_credential("NOPE").await.unwrap();
        assert!(!deleted);
    }

    #[tokio::test]
    async fn duplicate_key_fails() {
        let store = test_store().await;
        let cred = Credential {
            access_key_id: "DUPE".to_string(),
            secret_access_key: "SECRET".to_string(),
            description: String::new(),
            created_at: Utc::now(),
            active: true,
            admin: false,
            user_id: "root".to_string(),
        };
        store.put_credential(&cred).await.unwrap();

        let result = store.put_credential(&cred).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn count_active_skips_inactive() {
        let store = test_store().await;

        let active = Credential {
            access_key_id: "ACTIVE".to_string(),
            secret_access_key: "SECRET".to_string(),
            description: String::new(),
            created_at: Utc::now(),
            active: true,
            admin: false,
            user_id: "root".to_string(),
        };
        let inactive = Credential {
            access_key_id: "INACTIVE".to_string(),
            secret_access_key: "SECRET".to_string(),
            description: String::new(),
            created_at: Utc::now(),
            active: false,
            admin: false,
            user_id: "root".to_string(),
        };

        store.put_credential(&active).await.unwrap();
        store.put_credential(&inactive).await.unwrap();

        let count = store.count_active_credentials().await.unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn admin_flag_persisted() {
        let store = test_store().await;

        let admin_cred = Credential {
            access_key_id: "ADMIN1".to_string(),
            secret_access_key: "SECRET".to_string(),
            description: "admin".to_string(),
            created_at: Utc::now(),
            active: true,
            admin: true,
            user_id: "root".to_string(),
        };
        let user_cred = Credential {
            access_key_id: "USER1".to_string(),
            secret_access_key: "SECRET".to_string(),
            description: "user".to_string(),
            created_at: Utc::now(),
            active: true,
            admin: false,
            user_id: "root".to_string(),
        };

        store.put_credential(&admin_cred).await.unwrap();
        store.put_credential(&user_cred).await.unwrap();

        let admin = store.get_credential("ADMIN1").await.unwrap().unwrap();
        assert!(admin.admin);

        let user = store.get_credential("USER1").await.unwrap().unwrap();
        assert!(!user.admin);
    }

    #[tokio::test]
    async fn update_credential_active() {
        let store = test_store().await;
        let cred = Credential {
            access_key_id: "TOGGLE".to_string(),
            secret_access_key: "SECRET".to_string(),
            description: String::new(),
            created_at: Utc::now(),
            active: true,
            admin: false,
            user_id: "root".to_string(),
        };
        store.put_credential(&cred).await.unwrap();

        // Deactivate
        let ok = store.update_credential("TOGGLE", Some(false), None).await.unwrap();
        assert!(ok);
        let fetched = store.get_credential("TOGGLE").await.unwrap().unwrap();
        assert!(!fetched.active);

        // Reactivate
        let ok = store.update_credential("TOGGLE", Some(true), None).await.unwrap();
        assert!(ok);
        let fetched = store.get_credential("TOGGLE").await.unwrap().unwrap();
        assert!(fetched.active);
    }

    #[tokio::test]
    async fn update_credential_description() {
        let store = test_store().await;
        let cred = Credential {
            access_key_id: "DESC".to_string(),
            secret_access_key: "SECRET".to_string(),
            description: "original".to_string(),
            created_at: Utc::now(),
            active: true,
            admin: false,
            user_id: "root".to_string(),
        };
        store.put_credential(&cred).await.unwrap();

        let ok = store.update_credential("DESC", None, Some("updated")).await.unwrap();
        assert!(ok);
        let fetched = store.get_credential("DESC").await.unwrap().unwrap();
        assert_eq!(fetched.description, "updated");
        assert!(fetched.active); // unchanged
    }

    #[tokio::test]
    async fn update_credential_both_fields() {
        let store = test_store().await;
        let cred = Credential {
            access_key_id: "BOTH".to_string(),
            secret_access_key: "SECRET".to_string(),
            description: "old".to_string(),
            created_at: Utc::now(),
            active: true,
            admin: false,
            user_id: "root".to_string(),
        };
        store.put_credential(&cred).await.unwrap();

        let ok = store.update_credential("BOTH", Some(false), Some("new desc")).await.unwrap();
        assert!(ok);
        let fetched = store.get_credential("BOTH").await.unwrap().unwrap();
        assert!(!fetched.active);
        assert_eq!(fetched.description, "new desc");
    }

    #[tokio::test]
    async fn update_credential_nonexistent_returns_false() {
        let store = test_store().await;
        let ok = store.update_credential("NOPE", Some(false), None).await.unwrap();
        assert!(!ok);
    }

    #[tokio::test]
    async fn list_credentials_includes_admin_flag() {
        let store = test_store().await;

        let admin_cred = Credential {
            access_key_id: "ADMIN2".to_string(),
            secret_access_key: "SECRET".to_string(),
            description: String::new(),
            created_at: Utc::now(),
            active: true,
            admin: true,
            user_id: "root".to_string(),
        };
        let user_cred = Credential {
            access_key_id: "USER2".to_string(),
            secret_access_key: "SECRET".to_string(),
            description: String::new(),
            created_at: Utc::now(),
            active: true,
            admin: false,
            user_id: "root".to_string(),
        };

        store.put_credential(&admin_cred).await.unwrap();
        store.put_credential(&user_cred).await.unwrap();

        let creds = store.list_credentials().await.unwrap();
        let admins: Vec<_> = creds.iter().filter(|c| c.admin).collect();
        let users: Vec<_> = creds.iter().filter(|c| !c.admin).collect();
        assert_eq!(admins.len(), 1);
        assert_eq!(admins[0].access_key_id, "ADMIN2");
        assert_eq!(users.len(), 1);
        assert_eq!(users[0].access_key_id, "USER2");
    }
}
