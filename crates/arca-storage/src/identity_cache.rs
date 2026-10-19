//! Caching wrapper for the identity stores.
//!
//! Every authenticated request resolves its access key to a credential, the
//! credential to a user and, for non-root users, the user to the effective
//! policies of their grants: three metadata reads per request whose answers
//! change only when an administrator (or a cluster peer) changes them.
//! [`CachingIdentityStore`] serves those three lookups from memory.
//!
//! Invalidation is coarse on purpose: any write through any of the identity
//! traits (credentials, users, grants, teams, the control-plane snapshot and
//! tombstones) empties the whole cache. Writes are rare, and a single delete
//! can reach rows its arguments do not name (deleting a user deletes its
//! credentials, deleting a grant changes the policies of every holder), so a
//! targeted invalidation would have to replay every cascade. The wrapper sits
//! directly on the backend, under the cluster decorators, so the admin API,
//! the replicated control-plane ops and the anti-entropy merge all write
//! through it.
//!
//! What it cannot see are writes from another process sharing the database
//! (the `arca credential` / `arca user` commands run against a live server):
//! entries expire after [`IDENTITY_CACHE_TTL`], which bounds how long such a
//! change takes to apply. Lookups that find nothing are not cached, so a new
//! credential works at once whoever created it.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use chrono::{DateTime, Utc};
use moka::sync::Cache;

use arca_core::cluster::{ControlMergePlan, ControlSnapshot};
use arca_core::error::ArcaError;
use arca_core::policy::PolicyDocument;
use arca_core::store::{
    ControlSnapshotStore, ControlTombstone, ControlTombstoneStore, CredentialStore, GrantStore,
    TeamStore, UserStore,
};
use arca_core::types::{Credential, Grant, Team, User};

/// How long a cached identity lookup is served without going back to the
/// database. Bounds the delay of changes made by another process; changes
/// made through this server apply at once.
pub const IDENTITY_CACHE_TTL: Duration = Duration::from_secs(5);

/// Entries per lookup kind: far more than the active access keys and users
/// of any deployment, so the working set never evicts itself.
const IDENTITY_CACHE_CAPACITY: u64 = 10_000;

/// The identity stores of `S` with their per-request lookups cached.
pub struct CachingIdentityStore<S> {
    inner: Arc<S>,
    credentials: Cache<String, Credential>,
    users: Cache<String, User>,
    policies: Cache<String, Vec<PolicyDocument>>,
    /// Bumped by every invalidation. A lookup records it before reading the
    /// database and caches its answer only if it is unchanged, under this
    /// same lock: an answer read before a write can never be cached after
    /// that write's invalidation.
    generation: Mutex<u64>,
}

impl<S> CachingIdentityStore<S> {
    pub fn new(inner: Arc<S>) -> Self {
        Self::with_ttl(inner, IDENTITY_CACHE_TTL)
    }

    pub fn with_ttl(inner: Arc<S>, ttl: Duration) -> Self {
        fn cache<V: Clone + Send + Sync + 'static>(ttl: Duration) -> Cache<String, V> {
            Cache::builder()
                .max_capacity(IDENTITY_CACHE_CAPACITY)
                .time_to_live(ttl)
                .build()
        }
        Self {
            inner,
            credentials: cache(ttl),
            users: cache(ttl),
            policies: cache(ttl),
            generation: Mutex::new(0),
        }
    }

    fn lock_generation(&self) -> MutexGuard<'_, u64> {
        // The guarded value is a plain counter: a panic elsewhere cannot leave
        // it inconsistent, so a poisoned lock is still safe to use.
        self.generation.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn generation(&self) -> u64 {
        *self.lock_generation()
    }

    /// Caches `value` unless an invalidation happened since `generation`.
    fn insert_if_current<V: Clone + Send + Sync + 'static>(
        &self,
        cache: &Cache<String, V>,
        generation: u64,
        key: &str,
        value: V,
    ) {
        let current = self.lock_generation();
        if *current == generation {
            cache.insert(key.to_string(), value);
        }
    }

    fn invalidate(&self) {
        let mut generation = self.lock_generation();
        *generation += 1;
        self.credentials.invalidate_all();
        self.users.invalidate_all();
        self.policies.invalidate_all();
    }

    /// Runs a write and empties the cache afterwards, whatever its outcome:
    /// a failed write costs a few cache misses, never a stale answer.
    async fn write<T>(
        &self,
        op: impl std::future::Future<Output = Result<T, ArcaError>>,
    ) -> Result<T, ArcaError> {
        let result = op.await;
        self.invalidate();
        result
    }
}

#[async_trait::async_trait]
impl<S: CredentialStore + 'static> CredentialStore for CachingIdentityStore<S> {
    async fn put_credential(&self, credential: &Credential) -> Result<(), ArcaError> {
        self.write(self.inner.put_credential(credential)).await
    }

    async fn get_credential(&self, access_key_id: &str) -> Result<Option<Credential>, ArcaError> {
        if let Some(credential) = self.credentials.get(access_key_id) {
            return Ok(Some(credential));
        }
        let generation = self.generation();
        let credential = self.inner.get_credential(access_key_id).await?;
        if let Some(ref c) = credential {
            self.insert_if_current(&self.credentials, generation, access_key_id, c.clone());
        }
        Ok(credential)
    }

    async fn list_credentials(&self) -> Result<Vec<Credential>, ArcaError> {
        self.inner.list_credentials().await
    }

    async fn delete_credential(&self, access_key_id: &str) -> Result<bool, ArcaError> {
        self.write(self.inner.delete_credential(access_key_id)).await
    }

    async fn update_credential(
        &self,
        access_key_id: &str,
        active: Option<bool>,
        description: Option<&str>,
    ) -> Result<bool, ArcaError> {
        self.write(self.inner.update_credential(access_key_id, active, description))
            .await
    }

    async fn count_active_credentials(&self) -> Result<u64, ArcaError> {
        self.inner.count_active_credentials().await
    }

    async fn apply_remote_credential(&self, credential: &Credential) -> Result<(), ArcaError> {
        self.write(self.inner.apply_remote_credential(credential)).await
    }
}

#[async_trait::async_trait]
impl<S: UserStore + 'static> UserStore for CachingIdentityStore<S> {
    async fn put_user(&self, user: &User) -> Result<(), ArcaError> {
        self.write(self.inner.put_user(user)).await
    }

    async fn get_user(&self, user_id: &str) -> Result<Option<User>, ArcaError> {
        if let Some(user) = self.users.get(user_id) {
            return Ok(Some(user));
        }
        let generation = self.generation();
        let user = self.inner.get_user(user_id).await?;
        if let Some(ref u) = user {
            self.insert_if_current(&self.users, generation, user_id, u.clone());
        }
        Ok(user)
    }

    async fn get_user_by_username(&self, username: &str) -> Result<Option<User>, ArcaError> {
        self.inner.get_user_by_username(username).await
    }

    async fn list_users(&self) -> Result<Vec<User>, ArcaError> {
        self.inner.list_users().await
    }

    async fn update_user(
        &self,
        user_id: &str,
        username: Option<&str>,
        description: Option<&str>,
    ) -> Result<bool, ArcaError> {
        self.write(self.inner.update_user(user_id, username, description))
            .await
    }

    async fn delete_user(&self, user_id: &str) -> Result<bool, ArcaError> {
        self.write(self.inner.delete_user(user_id)).await
    }

    async fn apply_remote_user(&self, user: &User) -> Result<(), ArcaError> {
        self.write(self.inner.apply_remote_user(user)).await
    }
}

#[async_trait::async_trait]
impl<S: GrantStore + 'static> GrantStore for CachingIdentityStore<S> {
    async fn put_grant(&self, grant: &Grant) -> Result<(), ArcaError> {
        self.write(self.inner.put_grant(grant)).await
    }

    async fn get_grant(&self, grant_id: &str) -> Result<Option<Grant>, ArcaError> {
        self.inner.get_grant(grant_id).await
    }

    async fn get_grant_by_name(&self, name: &str) -> Result<Option<Grant>, ArcaError> {
        self.inner.get_grant_by_name(name).await
    }

    async fn list_grants(&self) -> Result<Vec<Grant>, ArcaError> {
        self.inner.list_grants().await
    }

    async fn update_grant(
        &self,
        grant_id: &str,
        name: Option<&str>,
        description: Option<&str>,
        document: Option<&PolicyDocument>,
    ) -> Result<bool, ArcaError> {
        self.write(self.inner.update_grant(grant_id, name, description, document))
            .await
    }

    async fn delete_grant(&self, grant_id: &str) -> Result<bool, ArcaError> {
        self.write(self.inner.delete_grant(grant_id)).await
    }

    async fn attach_to_user(&self, user_id: &str, grant_id: &str) -> Result<(), ArcaError> {
        self.write(self.inner.attach_to_user(user_id, grant_id)).await
    }

    async fn detach_from_user(&self, user_id: &str, grant_id: &str) -> Result<bool, ArcaError> {
        self.write(self.inner.detach_from_user(user_id, grant_id)).await
    }

    async fn attach_to_team(&self, team_id: &str, grant_id: &str) -> Result<(), ArcaError> {
        self.write(self.inner.attach_to_team(team_id, grant_id)).await
    }

    async fn detach_from_team(&self, team_id: &str, grant_id: &str) -> Result<bool, ArcaError> {
        self.write(self.inner.detach_from_team(team_id, grant_id)).await
    }

    async fn list_user_grants(&self, user_id: &str) -> Result<Vec<Grant>, ArcaError> {
        self.inner.list_user_grants(user_id).await
    }

    async fn list_team_grants(&self, team_id: &str) -> Result<Vec<Grant>, ArcaError> {
        self.inner.list_team_grants(team_id).await
    }

    async fn get_effective_policies(&self, user_id: &str) -> Result<Vec<PolicyDocument>, ArcaError> {
        if let Some(policies) = self.policies.get(user_id) {
            return Ok(policies);
        }
        let generation = self.generation();
        let policies = self.inner.get_effective_policies(user_id).await?;
        self.insert_if_current(&self.policies, generation, user_id, policies.clone());
        Ok(policies)
    }

    async fn apply_remote_grant(&self, grant: &Grant) -> Result<(), ArcaError> {
        self.write(self.inner.apply_remote_grant(grant)).await
    }
}

#[async_trait::async_trait]
impl<S: TeamStore + 'static> TeamStore for CachingIdentityStore<S> {
    async fn put_team(&self, team: &Team) -> Result<(), ArcaError> {
        self.write(self.inner.put_team(team)).await
    }

    async fn get_team(&self, team_id: &str) -> Result<Option<Team>, ArcaError> {
        self.inner.get_team(team_id).await
    }

    async fn list_teams(&self) -> Result<Vec<Team>, ArcaError> {
        self.inner.list_teams().await
    }

    async fn update_team(
        &self,
        team_id: &str,
        name: Option<&str>,
        description: Option<&str>,
    ) -> Result<bool, ArcaError> {
        self.write(self.inner.update_team(team_id, name, description))
            .await
    }

    async fn delete_team(&self, team_id: &str) -> Result<bool, ArcaError> {
        self.write(self.inner.delete_team(team_id)).await
    }

    async fn add_member(&self, team_id: &str, user_id: &str) -> Result<(), ArcaError> {
        self.write(self.inner.add_member(team_id, user_id)).await
    }

    async fn remove_member(&self, team_id: &str, user_id: &str) -> Result<bool, ArcaError> {
        self.write(self.inner.remove_member(team_id, user_id)).await
    }

    async fn list_members(&self, team_id: &str) -> Result<Vec<User>, ArcaError> {
        self.inner.list_members(team_id).await
    }

    async fn list_user_teams(&self, user_id: &str) -> Result<Vec<Team>, ArcaError> {
        self.inner.list_user_teams(user_id).await
    }

    async fn apply_remote_team(&self, team: &Team) -> Result<(), ArcaError> {
        self.write(self.inner.apply_remote_team(team)).await
    }
}

#[async_trait::async_trait]
impl<S: ControlSnapshotStore + 'static> ControlSnapshotStore for CachingIdentityStore<S> {
    async fn build_control_snapshot(&self) -> Result<ControlSnapshot, ArcaError> {
        self.inner.build_control_snapshot().await
    }

    async fn apply_credential_at(
        &self,
        credential: &Credential,
        updated_at: DateTime<Utc>,
    ) -> Result<(), ArcaError> {
        self.write(self.inner.apply_credential_at(credential, updated_at))
            .await
    }

    async fn apply_user_at(&self, user: &User, updated_at: DateTime<Utc>) -> Result<(), ArcaError> {
        self.write(self.inner.apply_user_at(user, updated_at)).await
    }

    async fn apply_team_at(&self, team: &Team, updated_at: DateTime<Utc>) -> Result<(), ArcaError> {
        self.write(self.inner.apply_team_at(team, updated_at)).await
    }

    async fn apply_user_grant_at(
        &self,
        user_id: &str,
        grant_id: &str,
        updated_at: DateTime<Utc>,
    ) -> Result<(), ArcaError> {
        self.write(self.inner.apply_user_grant_at(user_id, grant_id, updated_at))
            .await
    }

    async fn apply_team_grant_at(
        &self,
        team_id: &str,
        grant_id: &str,
        updated_at: DateTime<Utc>,
    ) -> Result<(), ArcaError> {
        self.write(self.inner.apply_team_grant_at(team_id, grant_id, updated_at))
            .await
    }

    async fn apply_team_member_at(
        &self,
        team_id: &str,
        user_id: &str,
        updated_at: DateTime<Utc>,
    ) -> Result<(), ArcaError> {
        self.write(self.inner.apply_team_member_at(team_id, user_id, updated_at))
            .await
    }

    async fn apply_server_config_at(
        &self,
        key: &str,
        value: &str,
        updated_at: DateTime<Utc>,
    ) -> Result<(), ArcaError> {
        // Server settings are not identity data: nothing to invalidate.
        self.inner.apply_server_config_at(key, value, updated_at).await
    }

    async fn apply_control_merge(&self, plan: &ControlMergePlan) -> Result<(), ArcaError> {
        self.write(self.inner.apply_control_merge(plan)).await
    }
}

#[async_trait::async_trait]
impl<S: ControlTombstoneStore + 'static> ControlTombstoneStore for CachingIdentityStore<S> {
    async fn record_control_tombstone(
        &self,
        entity_type: &str,
        entity_key: &str,
    ) -> Result<(), ArcaError> {
        self.inner.record_control_tombstone(entity_type, entity_key).await
    }

    async fn apply_control_tombstone(&self, tombstone: &ControlTombstone) -> Result<(), ArcaError> {
        // Applying a tombstone deletes the entity it names.
        self.write(self.inner.apply_control_tombstone(tombstone)).await
    }

    async fn list_control_tombstones(&self) -> Result<Vec<ControlTombstone>, ArcaError> {
        self.inner.list_control_tombstones().await
    }

    async fn delete_control_tombstone(
        &self,
        entity_type: &str,
        entity_key: &str,
    ) -> Result<bool, ArcaError> {
        self.inner.delete_control_tombstone(entity_type, entity_key).await
    }

    async fn purge_control_tombstones(&self, before: DateTime<Utc>) -> Result<u64, ArcaError> {
        self.inner.purge_control_tombstones(before).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SqliteStore;

    /// The cache over an in-memory backend, plus the backend itself: writing
    /// to the backend directly is how a test changes the database behind the
    /// cache's back.
    async fn stores() -> (CachingIdentityStore<SqliteStore>, Arc<SqliteStore>) {
        let raw = Arc::new(SqliteStore::open_in_memory().await.unwrap());
        (CachingIdentityStore::new(raw.clone()), raw)
    }

    fn user(user_id: &str) -> User {
        User {
            user_id: user_id.to_string(),
            username: user_id.to_string(),
            description: String::new(),
            is_root: false,
            created_at: Utc::now(),
        }
    }

    fn credential(access_key_id: &str, user_id: &str) -> Credential {
        Credential {
            access_key_id: access_key_id.to_string(),
            secret_access_key: "secret".to_string(),
            description: String::new(),
            created_at: Utc::now(),
            active: true,
            user_id: user_id.to_string(),
        }
    }

    fn team(team_id: &str) -> Team {
        Team {
            team_id: team_id.to_string(),
            name: team_id.to_string(),
            description: String::new(),
            created_at: Utc::now(),
        }
    }

    /// Seeded by the migrations.
    const ADMIN_GRANT: &str = "grant-administrator-access";

    async fn active(cache: &CachingIdentityStore<SqliteStore>, ak: &str) -> Option<bool> {
        cache.get_credential(ak).await.unwrap().map(|c| c.active)
    }

    #[tokio::test]
    async fn lookups_are_served_from_the_cache() {
        let (cache, raw) = stores().await;
        cache.put_user(&user("alice")).await.unwrap();
        cache.put_credential(&credential("AK1", "alice")).await.unwrap();
        assert_eq!(active(&cache, "AK1").await, Some(true));
        assert!(cache.get_user("alice").await.unwrap().is_some());

        // Changed behind the cache's back: the cached answers still stand.
        raw.update_credential("AK1", Some(false), None).await.unwrap();
        raw.update_user("alice", Some("renamed"), None).await.unwrap();
        assert_eq!(active(&cache, "AK1").await, Some(true));
        assert_eq!(cache.get_user("alice").await.unwrap().unwrap().username, "alice");
    }

    #[tokio::test]
    async fn a_write_through_the_cache_invalidates_every_lookup() {
        let (cache, raw) = stores().await;
        cache.put_user(&user("alice")).await.unwrap();
        cache.put_credential(&credential("AK1", "alice")).await.unwrap();
        assert_eq!(active(&cache, "AK1").await, Some(true));

        raw.update_credential("AK1", Some(false), None).await.unwrap();
        // An unrelated write (another user) still empties the cache.
        cache.put_user(&user("bob")).await.unwrap();
        assert_eq!(active(&cache, "AK1").await, Some(false));
    }

    #[tokio::test]
    async fn missing_credentials_are_not_cached() {
        let (cache, raw) = stores().await;
        assert_eq!(active(&cache, "AK-NEW").await, None);
        // Created by another process (the `arca credential` command).
        raw.put_credential(&credential("AK-NEW", "root")).await.unwrap();
        assert_eq!(active(&cache, "AK-NEW").await, Some(true));
    }

    /// Deleting a user deletes its credentials in the same transaction: the
    /// cached credential must go too, though the delete never names it.
    #[tokio::test]
    async fn deleting_a_user_drops_its_cached_credentials() {
        let (cache, _raw) = stores().await;
        cache.put_user(&user("alice")).await.unwrap();
        cache.put_credential(&credential("AK1", "alice")).await.unwrap();
        assert_eq!(active(&cache, "AK1").await, Some(true));

        cache.delete_user("alice").await.unwrap();
        assert_eq!(active(&cache, "AK1").await, None);
        assert!(cache.get_user("alice").await.unwrap().is_none());
    }

    /// Joining a team that holds a grant changes the user's policies without
    /// touching the user: the cached policy set must not survive it.
    #[tokio::test]
    async fn team_membership_changes_the_cached_policies() {
        let (cache, _raw) = stores().await;
        cache.put_user(&user("alice")).await.unwrap();
        cache.put_team(&team("ops")).await.unwrap();
        cache.attach_to_team("ops", ADMIN_GRANT).await.unwrap();
        assert!(cache.get_effective_policies("alice").await.unwrap().is_empty());

        cache.add_member("ops", "alice").await.unwrap();
        assert_eq!(cache.get_effective_policies("alice").await.unwrap().len(), 1);

        cache.delete_grant(ADMIN_GRANT).await.unwrap();
        assert!(cache.get_effective_policies("alice").await.unwrap().is_empty());
    }

    /// The anti-entropy reconcile applies its plan through the snapshot store,
    /// not through the credential store: it must invalidate as well.
    #[tokio::test]
    async fn the_control_plane_merge_invalidates() {
        let (cache, _raw) = stores().await;
        cache.put_user(&user("alice")).await.unwrap();
        cache.put_credential(&credential("AK1", "alice")).await.unwrap();
        assert_eq!(active(&cache, "AK1").await, Some(true));

        let plan = ControlMergePlan {
            delete_credentials: vec!["AK1".to_string()],
            ..Default::default()
        };
        cache.apply_control_merge(&plan).await.unwrap();
        assert_eq!(active(&cache, "AK1").await, None);
    }

    /// The remote-apply methods have "unsupported" default implementations:
    /// the wrapper must forward to the backend's, never fall back to those.
    #[tokio::test]
    async fn remote_applies_reach_the_backend() {
        let (cache, _raw) = stores().await;
        cache.apply_remote_user(&user("alice")).await.unwrap();
        cache.apply_remote_credential(&credential("AK1", "alice")).await.unwrap();
        cache.apply_remote_team(&team("ops")).await.unwrap();
        assert_eq!(active(&cache, "AK1").await, Some(true));
    }

    #[tokio::test]
    async fn entries_expire_after_the_ttl() {
        let raw = Arc::new(SqliteStore::open_in_memory().await.unwrap());
        let cache = CachingIdentityStore::with_ttl(raw.clone(), Duration::from_millis(50));
        cache.put_credential(&credential("AK1", "root")).await.unwrap();
        assert_eq!(active(&cache, "AK1").await, Some(true));

        raw.update_credential("AK1", Some(false), None).await.unwrap();
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert_eq!(active(&cache, "AK1").await, Some(false));
    }

    /// A lookup that read the database before a write must not cache its
    /// (now stale) answer after that write's invalidation.
    #[tokio::test]
    async fn an_answer_read_before_an_invalidation_is_not_cached() {
        let (cache, _raw) = stores().await;
        let generation = cache.generation();
        cache.invalidate();
        cache.insert_if_current(&cache.credentials, generation, "AK1", credential("AK1", "root"));
        assert!(cache.credentials.get("AK1").is_none());

        let generation = cache.generation();
        cache.insert_if_current(&cache.credentials, generation, "AK1", credential("AK1", "root"));
        assert!(cache.credentials.get("AK1").is_some());
    }
}
