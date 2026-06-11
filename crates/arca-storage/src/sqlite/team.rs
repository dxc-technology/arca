//! `TeamStore` implementation for `SqliteStore`.

use arca_core::error::ArcaError;
use arca_core::store::TeamStore;
use arca_core::types::{Team, User};
use chrono::DateTime;
use rusqlite::params;

use super::{SqliteStore, TrError};

#[async_trait::async_trait]
impl TeamStore for SqliteStore {
    async fn put_team(&self, team: &Team) -> Result<(), ArcaError> {
        let t = team.clone();
        self.conn
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO teams (team_id, name, description, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        t.team_id,
                        t.name,
                        t.description,
                        t.created_at.to_rfc3339(),
                        t.created_at.to_rfc3339(),
                    ],
                )?;
                Ok(())
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("put_team: {e}")))
    }

    async fn get_team(&self, team_id: &str) -> Result<Option<Team>, ArcaError> {
        let id = team_id.to_string();
        self.read_conn()
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT team_id, name, description, created_at
                     FROM teams WHERE team_id = ?1",
                )?;
                let result = stmt.query_row(params![id], |row| Ok(row_to_team(row)));
                match result {
                    Ok(team) => Ok(Some(team?)),
                    Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                    Err(e) => Err(e.into()),
                }
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("get_team: {e}")))
    }

    async fn list_teams(&self) -> Result<Vec<Team>, ArcaError> {
        self.read_conn()
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT team_id, name, description, created_at
                     FROM teams ORDER BY created_at",
                )?;
                let rows = stmt.query_map([], |row| Ok(row_to_team(row)))?;
                let mut teams = Vec::new();
                for row in rows {
                    teams.push(row??);
                }
                Ok(teams)
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("list_teams: {e}")))
    }

    async fn update_team(
        &self,
        team_id: &str,
        name: Option<&str>,
        description: Option<&str>,
    ) -> Result<bool, ArcaError> {
        let id = team_id.to_string();
        let name = name.map(|s| s.to_string());
        let description = description.map(|s| s.to_string());
        self.conn
            .call(move |conn| {
                let mut sets = Vec::new();
                let mut values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
                if let Some(ref n) = name {
                    sets.push("name = ?");
                    values.push(Box::new(n.clone()));
                }
                if let Some(ref d) = description {
                    sets.push("description = ?");
                    values.push(Box::new(d.clone()));
                }
                if sets.is_empty() {
                    let exists: bool = conn.query_row(
                        "SELECT 1 FROM teams WHERE team_id = ?1",
                        params![id],
                        |_| Ok(true),
                    ).unwrap_or(false);
                    return Ok(exists);
                }
                // Bump the LWW timestamp on any real change.
                sets.push("updated_at = ?");
                values.push(Box::new(chrono::Utc::now().to_rfc3339()));
                let sql = format!(
                    "UPDATE teams SET {} WHERE team_id = ?",
                    sets.join(", ")
                );
                values.push(Box::new(id));
                let params: Vec<&dyn rusqlite::types::ToSql> = values.iter().map(|v| v.as_ref()).collect();
                let affected = conn.execute(&sql, params.as_slice())?;
                Ok(affected > 0)
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("update_team: {e}")))
    }

    async fn delete_team(&self, team_id: &str) -> Result<bool, ArcaError> {
        let id = team_id.to_string();
        self.conn
            .call(move |conn| {
                let tx = conn.transaction()?;
                tx.execute(
                    "DELETE FROM team_members WHERE team_id = ?1",
                    params![id],
                )?;
                tx.execute(
                    "DELETE FROM team_grants WHERE team_id = ?1",
                    params![id],
                )?;
                let affected = tx.execute(
                    "DELETE FROM teams WHERE team_id = ?1",
                    params![id],
                )?;
                tx.commit()?;
                Ok(affected > 0)
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("delete_team: {e}")))
    }

    async fn add_member(&self, team_id: &str, user_id: &str) -> Result<(), ArcaError> {
        let tid = team_id.to_string();
        let uid = user_id.to_string();
        self.conn
            .call(move |conn| {
                // updated_at refreshed on an idempotent re-add too: the LWW
                // reconcile (R5) must see it as newer than any concurrent
                // remove-member tombstone.
                conn.execute(
                    "INSERT INTO team_members (team_id, user_id, updated_at) VALUES (?1, ?2, ?3)
                     ON CONFLICT(team_id, user_id) DO UPDATE SET updated_at = excluded.updated_at",
                    params![tid, uid, chrono::Utc::now().to_rfc3339()],
                )?;
                Ok(())
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("add_member: {e}")))
    }

    async fn remove_member(&self, team_id: &str, user_id: &str) -> Result<bool, ArcaError> {
        let tid = team_id.to_string();
        let uid = user_id.to_string();
        self.conn
            .call(move |conn| {
                let affected = conn.execute(
                    "DELETE FROM team_members WHERE team_id = ?1 AND user_id = ?2",
                    params![tid, uid],
                )?;
                Ok(affected > 0)
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("remove_member: {e}")))
    }

    async fn list_members(&self, team_id: &str) -> Result<Vec<User>, ArcaError> {
        let tid = team_id.to_string();
        self.read_conn()
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT u.user_id, u.username, u.description, u.is_root, u.created_at
                     FROM users u
                     JOIN team_members tm ON tm.user_id = u.user_id
                     WHERE tm.team_id = ?1
                     ORDER BY u.username",
                )?;
                let rows = stmt.query_map(params![tid], |row| {
                    Ok(super::user::row_to_user(row))
                })?;
                let mut users = Vec::new();
                for row in rows {
                    users.push(row??);
                }
                Ok(users)
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("list_members: {e}")))
    }

    async fn list_user_teams(&self, user_id: &str) -> Result<Vec<Team>, ArcaError> {
        let uid = user_id.to_string();
        self.read_conn()
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT t.team_id, t.name, t.description, t.created_at
                     FROM teams t
                     JOIN team_members tm ON tm.team_id = t.team_id
                     WHERE tm.user_id = ?1
                     ORDER BY t.name",
                )?;
                let rows = stmt.query_map(params![uid], |row| Ok(row_to_team(row)))?;
                let mut teams = Vec::new();
                for row in rows {
                    teams.push(row??);
                }
                Ok(teams)
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("list_user_teams: {e}")))
    }

    async fn apply_remote_team(&self, team: &Team) -> Result<(), ArcaError> {
        let t = team.clone();
        self.conn
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO teams (team_id, name, description, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT(team_id) DO UPDATE SET
                       name = excluded.name,
                       description = excluded.description,
                       created_at = excluded.created_at,
                       updated_at = excluded.updated_at",
                    params![
                        t.team_id,
                        t.name,
                        t.description,
                        t.created_at.to_rfc3339(),
                        chrono::Utc::now().to_rfc3339(),
                    ],
                )?;
                Ok(())
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("apply_remote_team: {e}")))
    }
}

fn row_to_team(row: &rusqlite::Row) -> Result<Team, rusqlite::Error> {
    let created_at_str: String = row.get(3)?;

    let created_at = DateTime::parse_from_rfc3339(&created_at_str)
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                3,
                rusqlite::types::Type::Text,
                Box::new(e),
            )
        })?;

    Ok(Team {
        team_id: row.get(0)?,
        name: row.get(1)?,
        description: row.get(2)?,
        created_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use arca_core::store::UserStore;
    use arca_core::types::User;
    use chrono::Utc;

    async fn test_store() -> SqliteStore {
        SqliteStore::open_in_memory().await.unwrap()
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
    async fn apply_remote_team_upserts_and_is_idempotent() {
        let store = test_store().await;
        let t = make_team("t-remote", "remote-team");
        store.apply_remote_team(&t).await.unwrap();
        assert!(store.get_team("t-remote").await.unwrap().is_some());

        // Re-deliver with an updated description: overwrites in place, no error.
        let mut t2 = make_team("t-remote", "remote-team");
        t2.description = "updated".to_string();
        store.apply_remote_team(&t2).await.unwrap();
        assert_eq!(
            store.get_team("t-remote").await.unwrap().unwrap().description,
            "updated"
        );
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
    async fn put_and_get_team() {
        let store = test_store().await;
        let team = make_team("t1", "Engineering");
        store.put_team(&team).await.unwrap();

        let fetched = store.get_team("t1").await.unwrap().unwrap();
        assert_eq!(fetched.name, "Engineering");
    }

    #[tokio::test]
    async fn get_nonexistent_returns_none() {
        let store = test_store().await;
        assert!(store.get_team("nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn list_teams() {
        let store = test_store().await;
        store.put_team(&make_team("t2", "DevOps")).await.unwrap();
        store.put_team(&make_team("t3", "Security")).await.unwrap();
        let teams = store.list_teams().await.unwrap();
        assert_eq!(teams.len(), 2);
    }

    #[tokio::test]
    async fn update_team_description() {
        let store = test_store().await;
        store.put_team(&make_team("t4", "QA")).await.unwrap();
        assert!(store.update_team("t4", None, Some("Quality Assurance")).await.unwrap());
        let fetched = store.get_team("t4").await.unwrap().unwrap();
        assert_eq!(fetched.description, "Quality Assurance");
        assert_eq!(fetched.name, "QA"); // unchanged
    }

    #[tokio::test]
    async fn update_team_name() {
        let store = test_store().await;
        store.put_team(&make_team("t4b", "OldName")).await.unwrap();
        assert!(store.update_team("t4b", Some("NewName"), None).await.unwrap());
        let fetched = store.get_team("t4b").await.unwrap().unwrap();
        assert_eq!(fetched.name, "NewName");
    }

    #[tokio::test]
    async fn delete_team() {
        let store = test_store().await;
        store.put_team(&make_team("t5", "Temp")).await.unwrap();
        assert!(store.delete_team("t5").await.unwrap());
        assert!(store.get_team("t5").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn duplicate_name_fails() {
        let store = test_store().await;
        store.put_team(&make_team("t6", "Unique")).await.unwrap();
        assert!(store.put_team(&make_team("t7", "Unique")).await.is_err());
    }

    #[tokio::test]
    async fn add_and_list_members() {
        let store = test_store().await;
        store.put_team(&make_team("t8", "Team8")).await.unwrap();
        store.put_user(&make_user("u1", "alice")).await.unwrap();
        store.put_user(&make_user("u2", "bob")).await.unwrap();

        store.add_member("t8", "u1").await.unwrap();
        store.add_member("t8", "u2").await.unwrap();
        // Adding again is a no-op
        store.add_member("t8", "u1").await.unwrap();

        let members = store.list_members("t8").await.unwrap();
        assert_eq!(members.len(), 2);
    }

    #[tokio::test]
    async fn remove_member() {
        let store = test_store().await;
        store.put_team(&make_team("t9", "Team9")).await.unwrap();
        store.put_user(&make_user("u3", "carol")).await.unwrap();
        store.add_member("t9", "u3").await.unwrap();

        assert!(store.remove_member("t9", "u3").await.unwrap());
        assert!(!store.remove_member("t9", "u3").await.unwrap()); // already removed
        assert!(store.list_members("t9").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn list_user_teams() {
        let store = test_store().await;
        store.put_team(&make_team("t10", "Alpha")).await.unwrap();
        store.put_team(&make_team("t11", "Beta")).await.unwrap();
        store.put_user(&make_user("u4", "dave")).await.unwrap();

        store.add_member("t10", "u4").await.unwrap();
        store.add_member("t11", "u4").await.unwrap();

        let teams = store.list_user_teams("u4").await.unwrap();
        assert_eq!(teams.len(), 2);
    }

    #[tokio::test]
    async fn delete_team_cleans_memberships() {
        let store = test_store().await;
        store.put_team(&make_team("t12", "Cleanup")).await.unwrap();
        store.put_user(&make_user("u5", "eve")).await.unwrap();
        store.add_member("t12", "u5").await.unwrap();

        store.delete_team("t12").await.unwrap();
        let teams = store.list_user_teams("u5").await.unwrap();
        assert!(teams.is_empty());
    }
}
