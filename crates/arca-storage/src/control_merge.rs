//! Backend-agnostic apply loop for a cluster control-plane merge plan.
//!
//! The LWW + tombstone decision logic is the pure
//! [`arca_core::cluster::plan_control_merge`]; this helper just executes the
//! resulting plan against any store that implements the per-entity traits, so
//! the sqlite and pg `apply_control_merge` bodies are one line each (no
//! duplicated loop).

use arca_core::cluster::ControlMergePlan;
use arca_core::error::ArcaError;
use arca_core::store::{
    ControlSnapshotStore, ControlTombstoneStore, CredentialStore, GrantStore, ServerConfigStore,
    TeamStore, UserStore,
};

/// Applies the IDENTITY part of a computed merge plan (credentials, users,
/// teams, grants, their attachments/memberships, cluster-wide server config)
/// plus tombstone adopt/clear, via the per-entity store methods.
///
/// Buckets — with bucket config and bucket tags — and the multipart rows are
/// deliberately NOT applied here: they live in the metadata store, which may be
/// wrapped by [`crate::CachingMetadataStore`]. Applying them on the concrete
/// store would skip cache invalidation, so the reconcile worker applies those
/// plan entries through its cache-aware `MetadataStore` handle instead (the
/// same path the object anti-entropy uses).
pub(crate) async fn apply_control_merge_via_traits<S>(
    store: &S,
    plan: &ControlMergePlan,
) -> Result<(), ArcaError>
where
    S: ControlSnapshotStore
        + CredentialStore
        + UserStore
        + TeamStore
        + GrantStore
        + ServerConfigStore
        + ControlTombstoneStore,
{
    // Tombstones are adopted BEFORE the deletes execute (review §2.3): each
    // call commits in its own transaction, so a crash (or a concurrent
    // snapshot) part-way through must leave the safe state — tombstone present,
    // row possibly still alive — which converges to the delete on the next
    // round. The reverse order leaves "row gone + no tombstone", and a peer
    // still holding the live row would resurrect it (for a revoked credential,
    // a security hole).
    for t in &plan.adopt_tombstones {
        store.apply_control_tombstone(t).await?;
    }
    // Parents before children: the join-table upserts reference users, teams
    // and grants (FK constraints on the PostgreSQL backend).
    for c in &plan.upsert_credentials {
        store.apply_credential_at(&c.credential, c.updated_at).await?;
    }
    for u in &plan.upsert_users {
        store.apply_user_at(&u.user, u.updated_at).await?;
    }
    for t in &plan.upsert_teams {
        store.apply_team_at(&t.team, t.updated_at).await?;
    }
    for g in &plan.upsert_grants {
        store.apply_remote_grant(g).await?;
    }
    for x in &plan.upsert_user_grants {
        store
            .apply_user_grant_at(&x.user_id, &x.grant_id, x.updated_at)
            .await?;
    }
    for x in &plan.upsert_team_grants {
        store
            .apply_team_grant_at(&x.team_id, &x.grant_id, x.updated_at)
            .await?;
    }
    for x in &plan.upsert_team_members {
        store
            .apply_team_member_at(&x.team_id, &x.user_id, x.updated_at)
            .await?;
    }
    for x in &plan.upsert_server_configs {
        store
            .apply_server_config_at(&x.key, &x.value, x.updated_at)
            .await?;
    }
    // Child deletes before parent deletes is not required (parent deletes
    // cascade their join rows; a second delete is a no-op), but running the
    // narrow ones first keeps the work minimal.
    for (user_id, grant_id) in &plan.delete_user_grants {
        store.detach_from_user(user_id, grant_id).await?;
    }
    for (team_id, grant_id) in &plan.delete_team_grants {
        store.detach_from_team(team_id, grant_id).await?;
    }
    for (team_id, user_id) in &plan.delete_team_members {
        store.remove_member(team_id, user_id).await?;
    }
    for key in &plan.delete_server_configs {
        store.delete_server_config(key).await?;
    }
    for k in &plan.delete_credentials {
        store.delete_credential(k).await?;
    }
    for k in &plan.delete_users {
        store.delete_user(k).await?;
    }
    for k in &plan.delete_teams {
        store.delete_team(k).await?;
    }
    for k in &plan.delete_grants {
        store.delete_grant(k).await?;
    }
    // Clears run LAST: a stale tombstone next to a newer alive row is harmless
    // (LWW keeps the row), so clearing after the upserts narrows the window in
    // which a crash could leave a cleared tombstone without its re-created row.
    for t in &plan.clear_tombstones {
        store
            .delete_control_tombstone(&t.entity_type, &t.entity_key)
            .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use arca_core::cluster::{
        ControlSnapshot, TimestampedCredential, TimestampedTeam, TimestampedUser,
    };
    use arca_core::policy::PolicyDocument;
    use arca_core::store::ControlTombstone;
    use arca_core::types::{Credential, Grant, Team, User};
    use chrono::{DateTime, Utc};
    use std::sync::Mutex;

    /// Records the order of every merge-plan apply call, so the test can assert
    /// the crash-safety ordering (tombstones adopted BEFORE deletes execute).
    #[derive(Default)]
    struct RecordingStore {
        calls: Mutex<Vec<String>>,
    }

    impl RecordingStore {
        fn log(&self, call: impl Into<String>) {
            self.calls.lock().unwrap().push(call.into());
        }
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl ControlSnapshotStore for RecordingStore {
        async fn build_control_snapshot(&self) -> Result<ControlSnapshot, ArcaError> {
            unreachable!("not used by apply_control_merge_via_traits")
        }
        async fn apply_credential_at(
            &self,
            c: &Credential,
            _updated_at: DateTime<Utc>,
        ) -> Result<(), ArcaError> {
            self.log(format!("upsert_credential:{}", c.access_key_id));
            Ok(())
        }
        async fn apply_user_at(&self, u: &User, _updated_at: DateTime<Utc>) -> Result<(), ArcaError> {
            self.log(format!("upsert_user:{}", u.user_id));
            Ok(())
        }
        async fn apply_team_at(&self, t: &Team, _updated_at: DateTime<Utc>) -> Result<(), ArcaError> {
            self.log(format!("upsert_team:{}", t.team_id));
            Ok(())
        }
        async fn apply_user_grant_at(
            &self,
            user_id: &str,
            grant_id: &str,
            _updated_at: DateTime<Utc>,
        ) -> Result<(), ArcaError> {
            self.log(format!("upsert_user_grant:{user_id}:{grant_id}"));
            Ok(())
        }
        async fn apply_team_grant_at(
            &self,
            team_id: &str,
            grant_id: &str,
            _updated_at: DateTime<Utc>,
        ) -> Result<(), ArcaError> {
            self.log(format!("upsert_team_grant:{team_id}:{grant_id}"));
            Ok(())
        }
        async fn apply_team_member_at(
            &self,
            team_id: &str,
            user_id: &str,
            _updated_at: DateTime<Utc>,
        ) -> Result<(), ArcaError> {
            self.log(format!("upsert_team_member:{team_id}:{user_id}"));
            Ok(())
        }
        async fn apply_server_config_at(
            &self,
            key: &str,
            value: &str,
            _updated_at: DateTime<Utc>,
        ) -> Result<(), ArcaError> {
            self.log(format!("upsert_server_config:{key}={value}"));
            Ok(())
        }
        async fn apply_control_merge(
            &self,
            _plan: &arca_core::cluster::ControlMergePlan,
        ) -> Result<(), ArcaError> {
            unreachable!("not used by apply_control_merge_via_traits")
        }
    }

    #[async_trait::async_trait]
    impl ServerConfigStore for RecordingStore {
        async fn get_server_config(&self, _key: &str) -> Result<Option<String>, ArcaError> {
            unreachable!()
        }
        async fn set_server_config(&self, _key: &str, _value: &str) -> Result<(), ArcaError> {
            unreachable!()
        }
        async fn delete_server_config(&self, key: &str) -> Result<bool, ArcaError> {
            self.log(format!("delete_server_config:{key}"));
            Ok(true)
        }
        async fn list_server_config(&self) -> Result<Vec<(String, String)>, ArcaError> {
            unreachable!()
        }
    }

    #[async_trait::async_trait]
    impl CredentialStore for RecordingStore {
        async fn put_credential(&self, _c: &Credential) -> Result<(), ArcaError> {
            unreachable!()
        }
        async fn get_credential(&self, _id: &str) -> Result<Option<Credential>, ArcaError> {
            unreachable!()
        }
        async fn list_credentials(&self) -> Result<Vec<Credential>, ArcaError> {
            unreachable!()
        }
        async fn delete_credential(&self, access_key_id: &str) -> Result<bool, ArcaError> {
            self.log(format!("delete_credential:{access_key_id}"));
            Ok(true)
        }
        async fn update_credential(
            &self,
            _id: &str,
            _active: Option<bool>,
            _description: Option<&str>,
        ) -> Result<bool, ArcaError> {
            unreachable!()
        }
        async fn count_active_credentials(&self) -> Result<u64, ArcaError> {
            unreachable!()
        }
    }

    #[async_trait::async_trait]
    impl UserStore for RecordingStore {
        async fn put_user(&self, _u: &User) -> Result<(), ArcaError> {
            unreachable!()
        }
        async fn get_user(&self, _id: &str) -> Result<Option<User>, ArcaError> {
            unreachable!()
        }
        async fn get_user_by_username(&self, _name: &str) -> Result<Option<User>, ArcaError> {
            unreachable!()
        }
        async fn list_users(&self) -> Result<Vec<User>, ArcaError> {
            unreachable!()
        }
        async fn update_user(
            &self,
            _id: &str,
            _username: Option<&str>,
            _description: Option<&str>,
        ) -> Result<bool, ArcaError> {
            unreachable!()
        }
        async fn delete_user(&self, user_id: &str) -> Result<bool, ArcaError> {
            self.log(format!("delete_user:{user_id}"));
            Ok(true)
        }
    }

    #[async_trait::async_trait]
    impl TeamStore for RecordingStore {
        async fn put_team(&self, _t: &Team) -> Result<(), ArcaError> {
            unreachable!()
        }
        async fn get_team(&self, _id: &str) -> Result<Option<Team>, ArcaError> {
            unreachable!()
        }
        async fn list_teams(&self) -> Result<Vec<Team>, ArcaError> {
            unreachable!()
        }
        async fn update_team(
            &self,
            _id: &str,
            _name: Option<&str>,
            _description: Option<&str>,
        ) -> Result<bool, ArcaError> {
            unreachable!()
        }
        async fn delete_team(&self, team_id: &str) -> Result<bool, ArcaError> {
            self.log(format!("delete_team:{team_id}"));
            Ok(true)
        }
        async fn add_member(&self, _team: &str, _user: &str) -> Result<(), ArcaError> {
            unreachable!()
        }
        async fn remove_member(&self, team: &str, user: &str) -> Result<bool, ArcaError> {
            self.log(format!("delete_team_member:{team}:{user}"));
            Ok(true)
        }
        async fn list_members(&self, _team: &str) -> Result<Vec<User>, ArcaError> {
            unreachable!()
        }
        async fn list_user_teams(&self, _user: &str) -> Result<Vec<Team>, ArcaError> {
            unreachable!()
        }
    }

    #[async_trait::async_trait]
    impl GrantStore for RecordingStore {
        async fn put_grant(&self, _g: &Grant) -> Result<(), ArcaError> {
            unreachable!()
        }
        async fn get_grant(&self, _id: &str) -> Result<Option<Grant>, ArcaError> {
            unreachable!()
        }
        async fn get_grant_by_name(&self, _name: &str) -> Result<Option<Grant>, ArcaError> {
            unreachable!()
        }
        async fn list_grants(&self) -> Result<Vec<Grant>, ArcaError> {
            unreachable!()
        }
        async fn update_grant(
            &self,
            _id: &str,
            _name: Option<&str>,
            _description: Option<&str>,
            _document: Option<&PolicyDocument>,
        ) -> Result<bool, ArcaError> {
            unreachable!()
        }
        async fn delete_grant(&self, grant_id: &str) -> Result<bool, ArcaError> {
            self.log(format!("delete_grant:{grant_id}"));
            Ok(true)
        }
        async fn attach_to_user(&self, _user: &str, _grant: &str) -> Result<(), ArcaError> {
            unreachable!()
        }
        async fn detach_from_user(&self, user: &str, grant: &str) -> Result<bool, ArcaError> {
            self.log(format!("delete_user_grant:{user}:{grant}"));
            Ok(true)
        }
        async fn attach_to_team(&self, _team: &str, _grant: &str) -> Result<(), ArcaError> {
            unreachable!()
        }
        async fn detach_from_team(&self, team: &str, grant: &str) -> Result<bool, ArcaError> {
            self.log(format!("delete_team_grant:{team}:{grant}"));
            Ok(true)
        }
        async fn list_user_grants(&self, _user: &str) -> Result<Vec<Grant>, ArcaError> {
            unreachable!()
        }
        async fn list_team_grants(&self, _team: &str) -> Result<Vec<Grant>, ArcaError> {
            unreachable!()
        }
        async fn get_effective_policies(
            &self,
            _user: &str,
        ) -> Result<Vec<PolicyDocument>, ArcaError> {
            unreachable!()
        }
        async fn apply_remote_grant(&self, grant: &Grant) -> Result<(), ArcaError> {
            self.log(format!("upsert_grant:{}", grant.grant_id));
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl ControlTombstoneStore for RecordingStore {
        async fn record_control_tombstone(
            &self,
            _entity_type: &str,
            _entity_key: &str,
        ) -> Result<(), ArcaError> {
            unreachable!()
        }
        async fn apply_control_tombstone(
            &self,
            tombstone: &ControlTombstone,
        ) -> Result<(), ArcaError> {
            self.log(format!(
                "adopt_tombstone:{}:{}",
                tombstone.entity_type, tombstone.entity_key
            ));
            Ok(())
        }
        async fn list_control_tombstones(&self) -> Result<Vec<ControlTombstone>, ArcaError> {
            unreachable!()
        }
        async fn delete_control_tombstone(
            &self,
            entity_type: &str,
            entity_key: &str,
        ) -> Result<bool, ArcaError> {
            self.log(format!("clear_tombstone:{entity_type}:{entity_key}"));
            Ok(true)
        }
        async fn purge_control_tombstones(
            &self,
            _before: DateTime<Utc>,
        ) -> Result<u64, ArcaError> {
            unreachable!()
        }
    }

    fn tombstone(entity_type: &str, entity_key: &str) -> ControlTombstone {
        ControlTombstone {
            entity_type: entity_type.to_string(),
            entity_key: entity_key.to_string(),
            deleted_at: Utc::now(),
        }
    }

    fn policy() -> PolicyDocument {
        PolicyDocument {
            version: "2012-10-17".to_string(),
            statement: vec![],
        }
    }

    /// Review §2.3 (P0): a crash between a delete and its tombstone adoption
    /// must never leave "row gone + no tombstone" (a peer would resurrect it —
    /// for a revoked credential, a security hole). Tombstones are adopted FIRST,
    /// so any interruption leaves the safe state (tombstone present, row maybe
    /// still alive → LWW converges to the delete on the next round).
    #[tokio::test]
    async fn tombstones_adopted_before_any_delete() {
        let store = RecordingStore::default();
        let now = Utc::now();
        let plan = ControlMergePlan {
            upsert_credentials: vec![TimestampedCredential {
                credential: Credential {
                    access_key_id: "ck-new".into(),
                    secret_access_key: "s".into(),
                    description: String::new(),
                    created_at: now,
                    active: true,
                    admin: false,
                    user_id: "u-new".into(),
                },
                updated_at: now,
            }],
            upsert_users: vec![TimestampedUser {
                user: User {
                    user_id: "u-new".into(),
                    username: "new".into(),
                    description: String::new(),
                    is_root: false,
                    created_at: now,
                },
                updated_at: now,
            }],
            upsert_teams: vec![TimestampedTeam {
                team: Team {
                    team_id: "t-new".into(),
                    name: "new".into(),
                    description: String::new(),
                    created_at: now,
                },
                updated_at: now,
            }],
            upsert_grants: vec![Grant {
                grant_id: "g-new".into(),
                name: "new".into(),
                description: String::new(),
                document: policy(),
                created_at: now,
                updated_at: now,
            }],
            upsert_buckets: vec![],
            delete_credentials: vec!["ck-dead".into()],
            delete_users: vec!["u-dead".into()],
            delete_teams: vec!["t-dead".into()],
            delete_grants: vec!["g-dead".into()],
            delete_buckets: vec![],
            // R5 family deletes also count as "deletes" for the ordering pin.
            delete_user_grants: vec![("u-dead".into(), "g-dead".into())],
            delete_team_members: vec![("t-dead".into(), "u-dead".into())],
            delete_server_configs: vec!["sc-dead".into()],
            adopt_tombstones: vec![
                tombstone("credential", "ck-dead"),
                tombstone("user", "u-dead"),
                tombstone("team", "t-dead"),
                tombstone("grant", "g-dead"),
            ],
            clear_tombstones: vec![tombstone("credential", "ck-new")],
            ..Default::default()
        };

        apply_control_merge_via_traits(&store, &plan).await.unwrap();

        let calls = store.calls();
        let last_adopt = calls
            .iter()
            .rposition(|c| c.starts_with("adopt_tombstone:"))
            .expect("tombstones were adopted");
        let first_delete = calls
            .iter()
            .position(|c| c.starts_with("delete_"))
            .expect("deletes were executed");
        assert!(
            last_adopt < first_delete,
            "every tombstone must be adopted before any delete executes \
             (crash window safety); call order was: {calls:?}"
        );
    }

    /// The merge plan applies every family: all four upserts, all four deletes,
    /// the adoptions and the clears (pinning that the reorder lost nothing).
    #[tokio::test]
    async fn merge_plan_applies_all_families() {
        let store = RecordingStore::default();
        let plan = ControlMergePlan {
            delete_credentials: vec!["c1".into()],
            delete_users: vec!["u1".into()],
            delete_teams: vec!["t1".into()],
            delete_grants: vec!["g1".into()],
            adopt_tombstones: vec![tombstone("credential", "c1")],
            clear_tombstones: vec![tombstone("user", "u9")],
            ..Default::default()
        };

        apply_control_merge_via_traits(&store, &plan).await.unwrap();

        let calls = store.calls();
        assert!(calls.contains(&"delete_credential:c1".to_string()));
        assert!(calls.contains(&"delete_user:u1".to_string()));
        assert!(calls.contains(&"delete_team:t1".to_string()));
        assert!(calls.contains(&"delete_grant:g1".to_string()));
        assert!(calls.contains(&"adopt_tombstone:credential:c1".to_string()));
        assert!(calls.contains(&"clear_tombstone:user:u9".to_string()));
    }
}
