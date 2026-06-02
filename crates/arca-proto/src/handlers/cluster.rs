//! Cluster (HA) HTTP endpoints (Phase 29).
//!
//! - `GET /cluster/v1/health` — public liveness + identity (M1).
//! - `PUT /cluster/v1/blob/{id}` — receive a blob's raw bytes + sidecar verbatim.
//! - `GET /cluster/v1/blob/{id}` — serve a blob's raw bytes + sidecar (repair).
//! - `POST /cluster/v1/object` — receive an object row verbatim (LWW upsert).
//! - `POST /cluster/v1/object/delete` — receive a version hard-delete.
//!
//! All but `health` sit behind the `cluster_auth` middleware (shared cluster
//! credential + loop prevention). They transfer already-encoded artifacts
//! (compressed/encrypted bytes, canonical rows) byte-for-byte, so blobs and
//! versions keep identical identities on every node.

use axum::body::{Body, Bytes};
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;

use arca_core::cluster::{
    ClusterManifest, ClusterManifestRequest, ClusterVersionDelete, ControlOp, ManifestEntry,
    CLUSTER_SIDECAR_HEADER,
};
use arca_core::store::SidecarMeta;
use arca_core::types::{BlobId, ObjectRecord};

use crate::state::AppState;

#[derive(Serialize)]
struct ClusterHealthResponse {
    status: &'static str,
    node_id: String,
}

/// `GET /cluster/v1/health` — public liveness + identity for peers.
///
/// Returns this node's `node_id` so a peer's membership manager can identify it
/// (and skip its own entry). Responds 404 when this node is not part of a
/// cluster, so a misconfigured probe gets a clear signal.
pub async fn health(State(state): State<AppState>) -> Response {
    match &state.cluster {
        Some(cluster) => Json(ClusterHealthResponse {
            status: "ok",
            node_id: cluster.node_id().to_string(),
        })
        .into_response(),
        None => (StatusCode::NOT_FOUND, "node is not part of a cluster").into_response(),
    }
}

/// `PUT /cluster/v1/blob/{blob_id}` — store a blob's raw bytes + sidecar verbatim.
///
/// The body is the raw on-disk bytes (already compressed/encrypted), streamed
/// straight to disk under the same `blob_id`. The sidecar travels in the signed
/// [`CLUSTER_SIDECAR_HEADER`] (base64 JSON). Composite blobs (multipart) carry
/// no physical file: an empty body + a sidecar with `composite` set writes only
/// the sidecar. Idempotent — re-delivery overwrites identical bytes.
pub async fn receive_blob(
    State(state): State<AppState>,
    Path(blob_id): Path<String>,
    request: Request,
) -> Response {
    let raw = match &state.cluster_raw_blob {
        Some(r) => r.clone(),
        None => return err(StatusCode::SERVICE_UNAVAILABLE, "node is not part of a cluster"),
    };

    let headers = request.headers().clone();
    let sidecar = match decode_sidecar(&headers) {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let blob_id = BlobId(blob_id);

    // Composite blobs have no file of their own; only the sidecar is stored.
    if sidecar.composite.is_none() {
        let body = request.into_body();
        let stream = crate::handlers::body::body_to_byte_stream(body, &headers, None);
        if let Err(e) = raw.write_raw(&blob_id, stream).await {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("write_raw failed: {e}"),
            );
        }
    }

    if let Err(e) = raw.write_sidecar(&blob_id, &sidecar).await {
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("write_sidecar failed: {e}"),
        );
    }
    StatusCode::OK.into_response()
}

/// `GET /cluster/v1/blob/{blob_id}` — serve a blob's raw bytes + sidecar.
///
/// Used by read-repair / anti-entropy on a peer that is missing the blob. The
/// sidecar is returned in the [`CLUSTER_SIDECAR_HEADER`]; the body streams the
/// raw bytes (empty for composite blobs). 404 when the blob is unknown here.
pub async fn get_blob(State(state): State<AppState>, Path(blob_id): Path<String>) -> Response {
    let raw = match &state.cluster_raw_blob {
        Some(r) => r.clone(),
        None => return err(StatusCode::SERVICE_UNAVAILABLE, "node is not part of a cluster"),
    };
    let blob_id = BlobId(blob_id);

    let sidecar = match raw.read_sidecar(&blob_id).await {
        Ok(Some(s)) => s,
        Ok(None) => return err(StatusCode::NOT_FOUND, "blob not found"),
        Err(e) => {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("read_sidecar failed: {e}"),
            )
        }
    };
    let sidecar_b64 = BASE64.encode(serde_json::to_vec(&sidecar).unwrap_or_default());

    // Composite blobs: no physical bytes to serve — sidecar only.
    if sidecar.composite.is_some() {
        return Response::builder()
            .status(StatusCode::OK)
            .header(CLUSTER_SIDECAR_HEADER, sidecar_b64)
            .header(axum::http::header::CONTENT_LENGTH, 0)
            .body(Body::empty())
            .expect("build composite blob response");
    }

    match raw.read_raw(&blob_id).await {
        Ok(result) => Response::builder()
            .status(StatusCode::OK)
            .header(CLUSTER_SIDECAR_HEADER, sidecar_b64)
            .header(axum::http::header::CONTENT_LENGTH, result.content_length)
            .body(Body::from_stream(result.stream))
            .expect("build blob response"),
        Err(e) => err(
            StatusCode::NOT_FOUND,
            &format!("blob bytes not found: {e}"),
        ),
    }
}

/// `POST /cluster/v1/object` — apply an object row received verbatim.
///
/// The body is a JSON [`ObjectRecord`]; it is applied via
/// [`arca_core::store::MetadataStore::apply_remote_object`] (idempotent upsert,
/// LWW conflict resolution, deterministic `is_latest` recompute).
pub async fn receive_object(State(state): State<AppState>, body: Bytes) -> Response {
    if state.cluster.is_none() {
        return err(StatusCode::SERVICE_UNAVAILABLE, "node is not part of a cluster");
    }
    let record: ObjectRecord = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => return err(StatusCode::BAD_REQUEST, &format!("invalid object json: {e}")),
    };
    match state.metadata.apply_remote_object(&record).await {
        Ok(()) => StatusCode::OK.into_response(),
        Err(e) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("apply_remote_object failed: {e}"),
        ),
    }
}

/// `POST /cluster/v1/object/delete` — apply a replicated version hard-delete.
///
/// The body is a JSON [`ClusterVersionDelete`]; applied via
/// [`arca_core::store::MetadataStore::apply_remote_version_delete`] (idempotent).
pub async fn receive_version_delete(State(state): State<AppState>, body: Bytes) -> Response {
    if state.cluster.is_none() {
        return err(StatusCode::SERVICE_UNAVAILABLE, "node is not part of a cluster");
    }
    let req: ClusterVersionDelete = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => return err(StatusCode::BAD_REQUEST, &format!("invalid delete json: {e}")),
    };
    match state
        .metadata
        .apply_remote_version_delete(&req.bucket, &req.key, &req.version_id)
        .await
    {
        Ok(()) => StatusCode::OK.into_response(),
        Err(e) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("apply_remote_version_delete failed: {e}"),
        ),
    }
}

/// `POST /cluster/v1/op` — apply a replicated control-plane operation.
///
/// Applied through `cluster_inner` (the stores BELOW the cluster decorators)
/// so it is NOT re-fanned-out to peers. Idempotent.
pub async fn receive_op(State(state): State<AppState>, body: Bytes) -> Response {
    // The inner control-plane handles are present together iff clustering is on.
    let inner = match &state.cluster_inner {
        Some(i) => i,
        None => return err(StatusCode::SERVICE_UNAVAILABLE, "node is not part of a cluster"),
    };
    let (metadata, credentials, users, grants, teams, server_config) = (
        &inner.metadata,
        &inner.credentials,
        &inner.users,
        &inner.grants,
        &inner.teams,
        &inner.server_config,
    );
    let op: ControlOp = match serde_json::from_slice(&body) {
        Ok(o) => o,
        Err(e) => return err(StatusCode::BAD_REQUEST, &format!("invalid control op json: {e}")),
    };

    let result = match op {
        ControlOp::BucketUpsert { info } => metadata.apply_remote_bucket(&info).await,
        ControlOp::BucketDelete { name } => metadata.delete_bucket(&name).await.map(|_| ()),
        ControlOp::BucketConfigSet { bucket, key, value } => {
            metadata.set_bucket_config(&bucket, &key, &value).await
        }
        ControlOp::BucketConfigDelete { bucket, key } => {
            metadata.delete_bucket_config(&bucket, &key).await.map(|_| ())
        }
        ControlOp::BucketTags { bucket, tags } => metadata.put_bucket_tags(&bucket, &tags).await,
        ControlOp::CredentialUpsert { credential } => {
            credentials.apply_remote_credential(&credential).await
        }
        ControlOp::CredentialDelete { access_key_id } => {
            credentials.delete_credential(&access_key_id).await.map(|_| ())
        }
        ControlOp::UserUpsert { user } => users.apply_remote_user(&user).await,
        ControlOp::UserDelete { user_id } => users.delete_user(&user_id).await.map(|_| ()),
        ControlOp::GrantUpsert { grant } => grants.apply_remote_grant(&grant).await,
        ControlOp::GrantDelete { grant_id } => grants.delete_grant(&grant_id).await.map(|_| ()),
        ControlOp::UserGrantAttach { user_id, grant_id } => {
            grants.attach_to_user(&user_id, &grant_id).await
        }
        ControlOp::UserGrantDetach { user_id, grant_id } => {
            grants.detach_from_user(&user_id, &grant_id).await.map(|_| ())
        }
        ControlOp::TeamGrantAttach { team_id, grant_id } => {
            grants.attach_to_team(&team_id, &grant_id).await
        }
        ControlOp::TeamGrantDetach { team_id, grant_id } => {
            grants.detach_from_team(&team_id, &grant_id).await.map(|_| ())
        }
        ControlOp::TeamUpsert { team } => teams.apply_remote_team(&team).await,
        ControlOp::TeamDelete { team_id } => teams.delete_team(&team_id).await.map(|_| ()),
        ControlOp::TeamMemberAdd { team_id, user_id } => {
            teams.add_member(&team_id, &user_id).await
        }
        ControlOp::TeamMemberRemove { team_id, user_id } => {
            teams.remove_member(&team_id, &user_id).await.map(|_| ())
        }
        ControlOp::ServerConfigSet { key, value } => {
            server_config.set_server_config(&key, &value).await
        }
        ControlOp::ServerConfigDelete { key } => {
            server_config.delete_server_config(&key).await.map(|_| ())
        }
        ControlOp::ObjectTags {
            bucket,
            key,
            version_id,
            tags,
        } => {
            // put_object_tags is a full replace (idempotent); empty clears.
            metadata.put_object_tags(&bucket, &key, &version_id, &tags).await
        }
        ControlOp::MultipartCreate { record } => {
            metadata.apply_remote_multipart_upload(&record).await
        }
        ControlOp::PartUpsert { part } => {
            // put_part is an idempotent replace by (upload_id, part_number).
            metadata.put_part(&part).await.map(|_| ())
        }
        ControlOp::MultipartDelete { upload_id } => {
            metadata.delete_multipart_upload(&upload_id).await.map(|_| ())
        }
    };

    match result {
        Ok(()) => StatusCode::OK.into_response(),
        Err(e) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("apply control op failed: {e}"),
        ),
    }
}

/// Upper bound on the manifest page size a peer may request, so an
/// anti-entropy pull can never trigger an unbounded scan.
const MANIFEST_MAX_LIMIT: u32 = 1000;

/// `POST /cluster/v1/manifest` — serve this node's changed-since object manifest.
///
/// Body is a JSON [`ClusterManifestRequest`] (`since`, `limit`). Returns every
/// object row with node-local `seq > since` (ascending), capped at
/// [`MANIFEST_MAX_LIMIT`], plus the `cursor` the requester advances to. The peer
/// anti-entropy worker loops this (advancing `since` to `cursor`) until the
/// batch is short, applying each row via `apply_remote_object`.
pub async fn manifest(State(state): State<AppState>, body: Bytes) -> Response {
    if state.cluster.is_none() {
        return err(StatusCode::SERVICE_UNAVAILABLE, "node is not part of a cluster");
    }
    let req: ClusterManifestRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            return err(
                StatusCode::BAD_REQUEST,
                &format!("invalid manifest request json: {e}"),
            )
        }
    };
    let limit = req.limit.clamp(1, MANIFEST_MAX_LIMIT);
    match state.metadata.list_rows_changed_since(req.since, limit).await {
        Ok(rows) => {
            // Empty batch → echo `since` so the caller's cursor doesn't move.
            let cursor = rows.last().map(|(s, _)| *s).unwrap_or(req.since);
            let entries = rows
                .into_iter()
                .map(|(seq, record)| ManifestEntry { seq, record })
                .collect();
            Json(ClusterManifest { entries, cursor }).into_response()
        }
        Err(e) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("manifest failed: {e}"),
        ),
    }
}

/// Decodes the base64-JSON sidecar from the signed cluster sidecar header.
fn decode_sidecar(headers: &HeaderMap) -> Result<SidecarMeta, Response> {
    let b64 = headers
        .get(CLUSTER_SIDECAR_HEADER)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| err(StatusCode::BAD_REQUEST, "missing sidecar header"))?;
    let json = BASE64
        .decode(b64)
        .map_err(|_| err(StatusCode::BAD_REQUEST, "invalid sidecar base64"))?;
    serde_json::from_slice(&json)
        .map_err(|_| err(StatusCode::BAD_REQUEST, "invalid sidecar json"))
}

/// Builds a JSON error response (cluster endpoints speak JSON, not S3 XML).
fn err(status: StatusCode, message: &str) -> Response {
    (status, Json(serde_json::json!({ "error": message }))).into_response()
}
