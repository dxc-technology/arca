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
    ControlSnapshotStore, ControlTombstoneStore, CredentialStore, GrantStore, TeamStore, UserStore,
};

/// Applies the IDENTITY part of a computed merge plan (credentials, users,
/// teams, grants) plus tombstone adopt/clear, via the per-entity store methods.
///
/// Buckets are deliberately NOT applied here: they live in the metadata store,
/// which may be wrapped by [`crate::CachingMetadataStore`]. Applying them on the
/// concrete store would skip cache invalidation, so the reconcile worker applies
/// `plan.upsert_buckets` / `plan.delete_buckets` through its cache-aware
/// `MetadataStore` handle instead (the same path the object anti-entropy uses).
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
        + ControlTombstoneStore,
{
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
    for t in &plan.adopt_tombstones {
        store.apply_control_tombstone(t).await?;
    }
    for t in &plan.clear_tombstones {
        store
            .delete_control_tombstone(&t.entity_type, &t.entity_key)
            .await?;
    }
    Ok(())
}
