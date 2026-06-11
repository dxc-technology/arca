//! `GrantStore` implementation for `SqliteStore`.

use arca_core::error::ArcaError;
use arca_core::policy::PolicyDocument;
use arca_core::store::GrantStore;
use arca_core::types::Grant;
use chrono::DateTime;
use rusqlite::params;

use super::{SqliteStore, TrError};

#[async_trait::async_trait]
impl GrantStore for SqliteStore {
    async fn put_grant(&self, grant: &Grant) -> Result<(), ArcaError> {
        let g = grant.clone();
        let doc_json = serde_json::to_string(&g.document)
            .map_err(|e| ArcaError::Internal(format!("serialize grant document: {e}")))?;
        self.conn
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO grants (grant_id, name, description, document, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        g.grant_id,
                        g.name,
                        g.description,
                        doc_json,
                        g.created_at.to_rfc3339(),
                        g.updated_at.to_rfc3339(),
                    ],
                )?;
                Ok(())
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("put_grant: {e}")))
    }

    async fn get_grant(&self, grant_id: &str) -> Result<Option<Grant>, ArcaError> {
        let id = grant_id.to_string();
        self.read_conn()
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT grant_id, name, description, document, created_at, updated_at
                     FROM grants WHERE grant_id = ?1",
                )?;
                let result = stmt.query_row(params![id], |row| Ok(row_to_grant(row)));
                match result {
                    Ok(grant) => Ok(Some(grant?)),
                    Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                    Err(e) => Err(e.into()),
                }
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("get_grant: {e}")))
    }

    async fn get_grant_by_name(&self, name: &str) -> Result<Option<Grant>, ArcaError> {
        let n = name.to_string();
        self.read_conn()
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT grant_id, name, description, document, created_at, updated_at
                     FROM grants WHERE name = ?1",
                )?;
                let result = stmt.query_row(params![n], |row| Ok(row_to_grant(row)));
                match result {
                    Ok(grant) => Ok(Some(grant?)),
                    Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                    Err(e) => Err(e.into()),
                }
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("get_grant_by_name: {e}")))
    }

    async fn list_grants(&self) -> Result<Vec<Grant>, ArcaError> {
        self.read_conn()
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT grant_id, name, description, document, created_at, updated_at
                     FROM grants ORDER BY created_at",
                )?;
                let rows = stmt.query_map([], |row| Ok(row_to_grant(row)))?;
                let mut grants = Vec::new();
                for row in rows {
                    grants.push(row??);
                }
                Ok(grants)
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("list_grants: {e}")))
    }

    async fn update_grant(
        &self,
        grant_id: &str,
        name: Option<&str>,
        description: Option<&str>,
        document: Option<&PolicyDocument>,
    ) -> Result<bool, ArcaError> {
        let id = grant_id.to_string();
        let name_owned = name.map(|s| s.to_string());
        let desc_owned = description.map(|s| s.to_string());
        let doc_json = document
            .map(|d| serde_json::to_string(d))
            .transpose()
            .map_err(|e| ArcaError::Internal(format!("serialize grant document: {e}")))?;
        let now = chrono::Utc::now().to_rfc3339();

        self.conn
            .call(move |conn| {
                // Build dynamic UPDATE
                let mut sets = vec!["updated_at = ?1"];
                let mut idx = 2u32;
                if name_owned.is_some() {
                    sets.push("name = ?2");
                    idx = 3;
                }
                if desc_owned.is_some() {
                    if idx == 2 {
                        sets.push("description = ?2");
                        idx = 3;
                    } else {
                        sets.push("description = ?3");
                        idx = 4;
                    }
                }
                if doc_json.is_some() {
                    match idx {
                        2 => sets.push("document = ?2"),
                        3 => sets.push("document = ?3"),
                        _ => sets.push("document = ?4"),
                    }
                    idx += 1;
                }

                let sql = format!(
                    "UPDATE grants SET {} WHERE grant_id = ?{}",
                    sets.join(", "),
                    idx
                );

                // Use a boxed params approach
                let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
                param_values.push(Box::new(now));
                if let Some(ref n) = name_owned {
                    param_values.push(Box::new(n.clone()));
                }
                if let Some(ref d) = desc_owned {
                    param_values.push(Box::new(d.clone()));
                }
                if let Some(ref dj) = doc_json {
                    param_values.push(Box::new(dj.clone()));
                }
                param_values.push(Box::new(id));

                let params_ref: Vec<&dyn rusqlite::types::ToSql> =
                    param_values.iter().map(|p| p.as_ref()).collect();
                let affected = conn.execute(&sql, params_ref.as_slice())?;
                Ok(affected > 0)
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("update_grant: {e}")))
    }

    async fn delete_grant(&self, grant_id: &str) -> Result<bool, ArcaError> {
        let id = grant_id.to_string();
        self.conn
            .call(move |conn| {
                let tx = conn.transaction()?;
                tx.execute(
                    "DELETE FROM user_grants WHERE grant_id = ?1",
                    params![id],
                )?;
                tx.execute(
                    "DELETE FROM team_grants WHERE grant_id = ?1",
                    params![id],
                )?;
                let affected = tx.execute(
                    "DELETE FROM grants WHERE grant_id = ?1",
                    params![id],
                )?;
                tx.commit()?;
                Ok(affected > 0)
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("delete_grant: {e}")))
    }

    async fn attach_to_user(&self, user_id: &str, grant_id: &str) -> Result<(), ArcaError> {
        let uid = user_id.to_string();
        let gid = grant_id.to_string();
        self.conn
            .call(move |conn| {
                // Refresh updated_at on an idempotent re-attach too: the LWW
                // reconcile (R5) must see a re-attach as newer than any
                // concurrent detach tombstone, or the user's intent is lost.
                conn.execute(
                    "INSERT INTO user_grants (user_id, grant_id, updated_at) VALUES (?1, ?2, ?3)
                     ON CONFLICT(user_id, grant_id) DO UPDATE SET updated_at = excluded.updated_at",
                    params![uid, gid, chrono::Utc::now().to_rfc3339()],
                )?;
                Ok(())
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("attach_to_user: {e}")))
    }

    async fn detach_from_user(&self, user_id: &str, grant_id: &str) -> Result<bool, ArcaError> {
        let uid = user_id.to_string();
        let gid = grant_id.to_string();
        self.conn
            .call(move |conn| {
                let affected = conn.execute(
                    "DELETE FROM user_grants WHERE user_id = ?1 AND grant_id = ?2",
                    params![uid, gid],
                )?;
                Ok(affected > 0)
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("detach_from_user: {e}")))
    }

    async fn attach_to_team(&self, team_id: &str, grant_id: &str) -> Result<(), ArcaError> {
        let tid = team_id.to_string();
        let gid = grant_id.to_string();
        self.conn
            .call(move |conn| {
                // updated_at refreshed on re-attach — see attach_to_user.
                conn.execute(
                    "INSERT INTO team_grants (team_id, grant_id, updated_at) VALUES (?1, ?2, ?3)
                     ON CONFLICT(team_id, grant_id) DO UPDATE SET updated_at = excluded.updated_at",
                    params![tid, gid, chrono::Utc::now().to_rfc3339()],
                )?;
                Ok(())
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("attach_to_team: {e}")))
    }

    async fn detach_from_team(&self, team_id: &str, grant_id: &str) -> Result<bool, ArcaError> {
        let tid = team_id.to_string();
        let gid = grant_id.to_string();
        self.conn
            .call(move |conn| {
                let affected = conn.execute(
                    "DELETE FROM team_grants WHERE team_id = ?1 AND grant_id = ?2",
                    params![tid, gid],
                )?;
                Ok(affected > 0)
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("detach_from_team: {e}")))
    }

    async fn list_user_grants(&self, user_id: &str) -> Result<Vec<Grant>, ArcaError> {
        let uid = user_id.to_string();
        self.read_conn()
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT g.grant_id, g.name, g.description, g.document, g.created_at, g.updated_at
                     FROM grants g
                     JOIN user_grants ug ON ug.grant_id = g.grant_id
                     WHERE ug.user_id = ?1
                     ORDER BY g.name",
                )?;
                let rows = stmt.query_map(params![uid], |row| Ok(row_to_grant(row)))?;
                let mut grants = Vec::new();
                for row in rows {
                    grants.push(row??);
                }
                Ok(grants)
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("list_user_grants: {e}")))
    }

    async fn list_team_grants(&self, team_id: &str) -> Result<Vec<Grant>, ArcaError> {
        let tid = team_id.to_string();
        self.read_conn()
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT g.grant_id, g.name, g.description, g.document, g.created_at, g.updated_at
                     FROM grants g
                     JOIN team_grants tg ON tg.grant_id = g.grant_id
                     WHERE tg.team_id = ?1
                     ORDER BY g.name",
                )?;
                let rows = stmt.query_map(params![tid], |row| Ok(row_to_grant(row)))?;
                let mut grants = Vec::new();
                for row in rows {
                    grants.push(row??);
                }
                Ok(grants)
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("list_team_grants: {e}")))
    }

    async fn get_effective_policies(
        &self,
        user_id: &str,
    ) -> Result<Vec<PolicyDocument>, ArcaError> {
        let uid = user_id.to_string();
        self.read_conn()
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT g.document
                     FROM grants g
                     WHERE g.grant_id IN (
                         SELECT grant_id FROM user_grants WHERE user_id = ?1
                         UNION
                         SELECT tg.grant_id FROM team_grants tg
                         JOIN team_members tm ON tm.team_id = tg.team_id
                         WHERE tm.user_id = ?1
                     )",
                )?;
                let rows = stmt.query_map(params![uid], |row| {
                    let doc_json: String = row.get(0)?;
                    Ok(doc_json)
                })?;
                let mut policies = Vec::new();
                for row in rows {
                    let json = row?;
                    let doc: PolicyDocument = serde_json::from_str(&json).map_err(|e| {
                        rusqlite::Error::FromSqlConversionFailure(
                            0,
                            rusqlite::types::Type::Text,
                            Box::new(e),
                        )
                    })?;
                    policies.push(doc);
                }
                Ok(policies)
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("get_effective_policies: {e}")))
    }

    async fn apply_remote_grant(&self, grant: &Grant) -> Result<(), ArcaError> {
        let g = grant.clone();
        let doc_json = serde_json::to_string(&g.document)
            .map_err(|e| ArcaError::Internal(format!("serialize grant document: {e}")))?;
        self.conn
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO grants (grant_id, name, description, document, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                     ON CONFLICT(grant_id) DO UPDATE SET
                       name = excluded.name,
                       description = excluded.description,
                       document = excluded.document,
                       created_at = excluded.created_at,
                       updated_at = excluded.updated_at",
                    params![
                        g.grant_id,
                        g.name,
                        g.description,
                        doc_json,
                        g.created_at.to_rfc3339(),
                        g.updated_at.to_rfc3339(),
                    ],
                )?;
                Ok(())
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("apply_remote_grant: {e}")))
    }
}

fn row_to_grant(row: &rusqlite::Row) -> Result<Grant, rusqlite::Error> {
    let doc_json: String = row.get(3)?;
    let created_at_str: String = row.get(4)?;
    let updated_at_str: String = row.get(5)?;

    let document: PolicyDocument = serde_json::from_str(&doc_json).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, Box::new(e))
    })?;

    let created_at = DateTime::parse_from_rfc3339(&created_at_str)
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                4,
                rusqlite::types::Type::Text,
                Box::new(e),
            )
        })?;

    let updated_at = DateTime::parse_from_rfc3339(&updated_at_str)
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                5,
                rusqlite::types::Type::Text,
                Box::new(e),
            )
        })?;

    Ok(Grant {
        grant_id: row.get(0)?,
        name: row.get(1)?,
        description: row.get(2)?,
        document,
        created_at,
        updated_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use arca_core::policy::{Effect, Statement};
    use arca_core::store::{TeamStore, UserStore};
    use arca_core::types::{Team, User};
    use chrono::Utc;

    async fn test_store() -> SqliteStore {
        SqliteStore::open_in_memory().await.unwrap()
    }

    fn make_grant(grant_id: &str, name: &str) -> Grant {
        Grant {
            grant_id: grant_id.to_string(),
            name: name.to_string(),
            description: String::new(),
            document: PolicyDocument {
                version: "2012-10-17".to_string(),
                statement: vec![Statement {
                    sid: None,
                    effect: Effect::Allow,
                    action: vec!["s3:GetObject".to_string()],
                    resource: vec!["*".to_string()],
                }],
            },
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
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

    fn make_team(team_id: &str, name: &str) -> Team {
        Team {
            team_id: team_id.to_string(),
            name: name.to_string(),
            description: String::new(),
            created_at: Utc::now(),
        }
    }

    #[tokio::test]
    async fn apply_remote_grant_upserts_and_is_idempotent() {
        let store = test_store().await;
        let g = make_grant("g-remote", "remote-grant");
        store.apply_remote_grant(&g).await.unwrap();
        assert!(store.get_grant("g-remote").await.unwrap().is_some());

        // Re-deliver with an updated description: overwrites in place, no error.
        let mut g2 = make_grant("g-remote", "remote-grant");
        g2.description = "updated".to_string();
        store.apply_remote_grant(&g2).await.unwrap();
        assert_eq!(
            store
                .get_grant("g-remote")
                .await
                .unwrap()
                .unwrap()
                .description,
            "updated"
        );
    }

    #[tokio::test]
    async fn builtin_grants_exist() {
        let store = test_store().await;
        let grants = store.list_grants().await.unwrap();
        assert!(grants.len() >= 3);
        assert!(grants.iter().any(|g| g.name == "AdministratorAccess"));
        assert!(grants.iter().any(|g| g.name == "S3FullAccess"));
        assert!(grants.iter().any(|g| g.name == "S3ReadOnlyAccess"));
    }

    #[tokio::test]
    async fn put_and_get_grant() {
        let store = test_store().await;
        let grant = make_grant("g1", "TestGrant");
        store.put_grant(&grant).await.unwrap();

        let fetched = store.get_grant("g1").await.unwrap().unwrap();
        assert_eq!(fetched.name, "TestGrant");
        assert_eq!(fetched.document.statement.len(), 1);
    }

    #[tokio::test]
    async fn get_grant_by_name() {
        let store = test_store().await;
        let grant = make_grant("g2", "NamedGrant");
        store.put_grant(&grant).await.unwrap();

        let fetched = store.get_grant_by_name("NamedGrant").await.unwrap().unwrap();
        assert_eq!(fetched.grant_id, "g2");
    }

    #[tokio::test]
    async fn update_grant_document() {
        let store = test_store().await;
        let grant = make_grant("g3", "UpdateMe");
        store.put_grant(&grant).await.unwrap();

        let new_doc = PolicyDocument {
            version: "2012-10-17".to_string(),
            statement: vec![Statement {
                sid: None,
                effect: Effect::Deny,
                action: vec!["s3:DeleteObject".to_string()],
                resource: vec!["*".to_string()],
            }],
        };
        assert!(store.update_grant("g3", None, None, Some(&new_doc)).await.unwrap());
        let fetched = store.get_grant("g3").await.unwrap().unwrap();
        assert_eq!(fetched.document.statement[0].effect, Effect::Deny);
    }

    #[tokio::test]
    async fn delete_grant() {
        let store = test_store().await;
        let grant = make_grant("g4", "DeleteMe");
        store.put_grant(&grant).await.unwrap();
        assert!(store.delete_grant("g4").await.unwrap());
        assert!(store.get_grant("g4").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn duplicate_name_fails() {
        let store = test_store().await;
        store.put_grant(&make_grant("g5", "Unique")).await.unwrap();
        assert!(store.put_grant(&make_grant("g6", "Unique")).await.is_err());
    }

    #[tokio::test]
    async fn attach_and_list_user_grants() {
        let store = test_store().await;
        let grant = make_grant("g7", "UserGrant");
        store.put_grant(&grant).await.unwrap();
        store.put_user(&make_user("u1", "alice")).await.unwrap();

        store.attach_to_user("u1", "g7").await.unwrap();
        // Attach again is no-op
        store.attach_to_user("u1", "g7").await.unwrap();

        let grants = store.list_user_grants("u1").await.unwrap();
        assert_eq!(grants.len(), 1);
        assert_eq!(grants[0].name, "UserGrant");
    }

    #[tokio::test]
    async fn detach_from_user() {
        let store = test_store().await;
        let grant = make_grant("g8", "DetachUser");
        store.put_grant(&grant).await.unwrap();
        store.put_user(&make_user("u2", "bob")).await.unwrap();

        store.attach_to_user("u2", "g8").await.unwrap();
        assert!(store.detach_from_user("u2", "g8").await.unwrap());
        assert!(!store.detach_from_user("u2", "g8").await.unwrap()); // already detached
        assert!(store.list_user_grants("u2").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn attach_and_list_team_grants() {
        let store = test_store().await;
        let grant = make_grant("g9", "TeamGrant");
        store.put_grant(&grant).await.unwrap();
        store.put_team(&make_team("t1", "Alpha")).await.unwrap();

        store.attach_to_team("t1", "g9").await.unwrap();
        let grants = store.list_team_grants("t1").await.unwrap();
        assert_eq!(grants.len(), 1);
        assert_eq!(grants[0].name, "TeamGrant");
    }

    #[tokio::test]
    async fn effective_policies_direct_only() {
        let store = test_store().await;
        store.put_user(&make_user("u3", "carol")).await.unwrap();
        let grant = make_grant("g10", "DirectGrant");
        store.put_grant(&grant).await.unwrap();
        store.attach_to_user("u3", "g10").await.unwrap();

        let policies = store.get_effective_policies("u3").await.unwrap();
        assert_eq!(policies.len(), 1);
    }

    #[tokio::test]
    async fn effective_policies_via_team() {
        let store = test_store().await;
        store.put_user(&make_user("u4", "dave")).await.unwrap();
        store.put_team(&make_team("t2", "Beta")).await.unwrap();
        store.add_member("t2", "u4").await.unwrap();

        let grant = make_grant("g11", "TeamOnlyGrant");
        store.put_grant(&grant).await.unwrap();
        store.attach_to_team("t2", "g11").await.unwrap();

        let policies = store.get_effective_policies("u4").await.unwrap();
        assert_eq!(policies.len(), 1);
    }

    #[tokio::test]
    async fn effective_policies_combined() {
        let store = test_store().await;
        store.put_user(&make_user("u5", "eve")).await.unwrap();
        store.put_team(&make_team("t3", "Gamma")).await.unwrap();
        store.add_member("t3", "u5").await.unwrap();

        let direct = make_grant("g12", "DirectGrant2");
        store.put_grant(&direct).await.unwrap();
        store.attach_to_user("u5", "g12").await.unwrap();

        let team = make_grant("g13", "TeamGrant2");
        store.put_grant(&team).await.unwrap();
        store.attach_to_team("t3", "g13").await.unwrap();

        let policies = store.get_effective_policies("u5").await.unwrap();
        assert_eq!(policies.len(), 2);
    }

    #[tokio::test]
    async fn effective_policies_no_duplicates() {
        let store = test_store().await;
        store.put_user(&make_user("u6", "frank")).await.unwrap();
        store.put_team(&make_team("t4", "Delta")).await.unwrap();
        store.add_member("t4", "u6").await.unwrap();

        // Attach same grant to both user and team
        let grant = make_grant("g14", "SharedGrant");
        store.put_grant(&grant).await.unwrap();
        store.attach_to_user("u6", "g14").await.unwrap();
        store.attach_to_team("t4", "g14").await.unwrap();

        let policies = store.get_effective_policies("u6").await.unwrap();
        assert_eq!(policies.len(), 1); // DISTINCT prevents duplicates
    }

    #[tokio::test]
    async fn effective_policies_empty_for_no_grants() {
        let store = test_store().await;
        store.put_user(&make_user("u7", "grace")).await.unwrap();
        let policies = store.get_effective_policies("u7").await.unwrap();
        assert!(policies.is_empty());
    }

    #[tokio::test]
    async fn delete_grant_cleans_attachments() {
        let store = test_store().await;
        store.put_user(&make_user("u8", "heidi")).await.unwrap();
        store.put_team(&make_team("t5", "Epsilon")).await.unwrap();

        let grant = make_grant("g15", "CleanupGrant");
        store.put_grant(&grant).await.unwrap();
        store.attach_to_user("u8", "g15").await.unwrap();
        store.attach_to_team("t5", "g15").await.unwrap();

        store.delete_grant("g15").await.unwrap();
        assert!(store.list_user_grants("u8").await.unwrap().is_empty());
        assert!(store.list_team_grants("t5").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn root_has_administrator_access() {
        let store = test_store().await;
        let grants = store.list_user_grants("root").await.unwrap();
        assert!(grants.iter().any(|g| g.name == "AdministratorAccess"));
    }
}
