# Admin API

JSON-based administration API for managing the Arca server. All endpoints live under `/admin/*` on the same port as the S3 API (default 9000).

## Authentication

All endpoints except `/admin/health` require **AWS SigV4** authentication — the same mechanism used for S3 requests. Only credentials with the **admin** flag can access admin endpoints. Non-admin credentials receive a `403 AccessDenied` response but can still use the S3 API normally.

Authorization is controlled through grants (policy documents) attached to users and teams. Root users have unrestricted access to all endpoints. Non-root users need grants with the appropriate `arca:*` actions to access admin endpoints.

## Error Responses

Admin API errors are returned as JSON (not S3 XML):

```json
{
    "error": "ErrorCode",
    "message": "Human-readable description"
}
```

Common error codes:

| Code | HTTP Status | Description |
|------|-------------|-------------|
| `BadRequest` | 400 | Invalid input (empty name, malformed JSON, etc.) |
| `NotFound` | 404 | Entity not found |
| `Conflict` | 409 | Constraint violation (duplicate name, cannot delete last admin, etc.) |
| `InternalError` | 500 | Server-side error |

---

## Health & Info

### Health Check

```
GET /admin/health
```

**Auth**: None (designed for load balancer probes).

**Response** `200`:

```json
{
    "status": "ok"
}
```

---

### Server Info

```
GET /admin/info
```

**Auth**: SigV4

**Response** `200`:

```json
{
    "version": "0.4.0",
    "uptime_seconds": 3600,
    "tls_enabled": true,
    "encryption_enabled": true,
    "kms_provider": "vault",
    "kms_endpoint": "http://vault:8200"
}
```

| Field | Type | Description |
|-------|------|-------------|
| `version` | string | Arca server version |
| `uptime_seconds` | integer | Seconds since server start |
| `tls_enabled` | boolean | Whether TLS is enabled on the main listener |
| `encryption_enabled` | boolean | Whether server-side encryption (SSE-S3) is enabled globally |
| `kms_provider` | string? | Key source: `"local"` (config file) or `"vault"` (Vault/OpenBAO). Omitted when no encryption. |
| `kms_endpoint` | string? | Vault/OpenBAO endpoint URL. Only present when `kms_provider` is `"vault"`. |

---

### Storage Stats

```
GET /admin/stats
```

**Auth**: SigV4

**Response** `200`:

```json
{
    "bucket_count": 5,
    "object_count": 142,
    "total_size_bytes": 1073741824,
    "disk_total_bytes": 107374182400,
    "disk_available_bytes": 53687091200
}
```

| Field | Type | Description |
|-------|------|-------------|
| `bucket_count` | integer | Total number of buckets |
| `object_count` | integer | Total number of objects across all buckets |
| `total_size_bytes` | integer | Total size of all objects in bytes |
| `disk_total_bytes` | integer? | Total disk capacity in bytes. Omitted if unavailable. |
| `disk_available_bytes` | integer? | Available disk space in bytes. Omitted if unavailable. |

---

### Current User Identity

```
GET /admin/me
```

**Auth**: SigV4

Returns the authenticated user's identity and effective permissions (from all grants, both direct and via teams).

**Response** `200`:

```json
{
    "user": {
        "user_id": "550e8400-e29b-41d4-a716-446655440000",
        "username": "admin",
        "is_root": true
    },
    "effective_actions": ["*"]
}
```

For non-root users, `effective_actions` lists the union of all Allow actions from all effective policy documents:

```json
{
    "user": {
        "user_id": "7c9e6679-7425-40de-944b-e07fc1f90ae7",
        "username": "alice",
        "is_root": false
    },
    "effective_actions": [
        "s3:GetObject",
        "s3:PutObject",
        "s3:ListBucket",
        "s3:ListAllMyBuckets"
    ]
}
```

---

## Credentials

Credentials are access key / secret key pairs used for SigV4 authentication. Each credential belongs to a user. The legacy `/admin/credentials` endpoints operate on the calling user's credentials, while the `/admin/users/{user_id}/credentials` endpoints (in the Users section) allow managing credentials for any user.

### List Credentials

```
GET /admin/credentials
```

**Auth**: SigV4

Returns all credentials. Secret keys are **never** included in list responses.

**Response** `200`:

```json
[
    {
        "access_key_id": "AKIAIOSFODNN7EXAMPLE",
        "description": "root credential",
        "created_at": "2025-01-15T10:30:00+00:00",
        "active": true,
        "admin": true
    }
]
```

---

### Create Credential

```
POST /admin/credentials
```

**Auth**: SigV4

Creates a new credential for the calling user.

**Request body**:

```json
{
    "description": "CI/CD pipeline",
    "admin": false
}
```

Both fields are optional. `description` defaults to empty string, `admin` defaults to `false`.

**Response** `201`:

```json
{
    "access_key_id": "AKXYZ123456789ABCDEF",
    "secret_access_key": "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
    "description": "CI/CD pipeline",
    "created_at": "2025-06-01T12:00:00+00:00",
    "active": true,
    "admin": false
}
```

!!! warning
    The `secret_access_key` is **only returned once** at creation time. Store it securely.

---

### Update Credential

```
PUT /admin/credentials/{access_key_id}
```

**Auth**: SigV4

Update a credential's `active` status and/or `description`. Both fields are optional; only provided fields are updated.

**Request body**:

```json
{
    "active": false,
    "description": "Disabled for rotation"
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `active` | boolean | No | Enable or disable the credential |
| `description` | string | No | Update the credential description |

**Response** `204`: Credential updated successfully (no body).

**Response** `404`: Credential not found.

**Response** `409`: Cannot deactivate the last active credential or last active admin credential (prevents lockout).

---

### Delete Credential

```
DELETE /admin/credentials/{access_key_id}
```

**Auth**: SigV4

**Response** `204`: Credential deleted successfully (no body).

**Response** `404`: Credential not found.

**Response** `409`: Cannot delete the last active credential or last admin credential (prevents lockout).

---

## Users

User management endpoints for the RBAC system. Users own credentials and can be members of teams. Grants (policy documents) can be attached directly to users or inherited through team membership.

### List Users

```
GET /admin/users
```

**Auth**: SigV4

**Response** `200`:

```json
[
    {
        "user_id": "550e8400-e29b-41d4-a716-446655440000",
        "username": "admin",
        "description": "Root administrator",
        "is_root": true,
        "created_at": "2025-01-01T00:00:00+00:00",
        "credential_count": 2,
        "team_count": 0,
        "grant_count": 1
    },
    {
        "user_id": "7c9e6679-7425-40de-944b-e07fc1f90ae7",
        "username": "alice",
        "description": "Developer",
        "is_root": false,
        "created_at": "2025-06-01T12:00:00+00:00",
        "credential_count": 1,
        "team_count": 2,
        "grant_count": 0
    }
]
```

---

### Create User

```
POST /admin/users
```

**Auth**: SigV4

**Request body**:

```json
{
    "username": "alice",
    "description": "Developer"
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `username` | string | Yes | Unique username (must not be empty) |
| `description` | string | No | User description (defaults to empty string) |

**Response** `201`:

```json
{
    "user_id": "7c9e6679-7425-40de-944b-e07fc1f90ae7",
    "username": "alice",
    "description": "Developer",
    "is_root": false,
    "created_at": "2025-06-01T12:00:00+00:00"
}
```

**Response** `400`: Username is empty.

**Response** `409`: Username already exists.

!!! note
    New users are always created with `is_root: false`. The root flag cannot be set through the API.

---

### Get User

```
GET /admin/users/{user_id}
```

**Auth**: SigV4

Returns detailed user information including counts of related entities.

**Response** `200`:

```json
{
    "user_id": "7c9e6679-7425-40de-944b-e07fc1f90ae7",
    "username": "alice",
    "description": "Developer",
    "is_root": false,
    "created_at": "2025-06-01T12:00:00+00:00",
    "credential_count": 1,
    "team_count": 2,
    "grant_count": 3
}
```

**Response** `404`: User not found.

---

### Update User

```
PUT /admin/users/{user_id}
```

**Auth**: SigV4

Update a user's `username` and/or `description`. Both fields are optional; only provided fields are updated.

**Request body**:

```json
{
    "username": "alice-new",
    "description": "Senior Developer"
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `username` | string | No | New unique username |
| `description` | string | No | New description |

**Response** `204`: User updated successfully (no body).

**Response** `404`: User not found.

**Response** `409`: Cannot modify the root user, or new username already exists.

---

### Delete User

```
DELETE /admin/users/{user_id}
```

**Auth**: SigV4

**Response** `204`: User deleted successfully (no body).

**Response** `404`: User not found.

**Response** `409`: Cannot delete the root user, or user still has credentials (remove credentials first).

---

### List User Credentials

```
GET /admin/users/{user_id}/credentials
```

**Auth**: SigV4

Returns all credentials belonging to the specified user. Secret keys are **never** included.

**Response** `200`:

```json
[
    {
        "access_key_id": "AKIAIOSFODNN7EXAMPLE",
        "description": "Main credential",
        "created_at": "2025-06-01T12:00:00+00:00",
        "active": true
    }
]
```

**Response** `404`: User not found.

---

### Create User Credential

```
POST /admin/users/{user_id}/credentials
```

**Auth**: SigV4

Creates a new credential for the specified user. Access key and secret key can be auto-generated or provided explicitly.

**Request body**:

```json
{
    "description": "CI credential",
    "access_key_id": "MYCUSTOMKEYID",
    "secret_access_key": "myCustomSecretKey123"
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `description` | string | No | Credential description (defaults to empty string) |
| `access_key_id` | string | No | Custom access key ID. Auto-generated if omitted or empty. |
| `secret_access_key` | string | No | Custom secret access key. Auto-generated if omitted or empty. |

**Response** `201`:

```json
{
    "access_key_id": "MYCUSTOMKEYID",
    "secret_access_key": "myCustomSecretKey123",
    "description": "CI credential",
    "created_at": "2025-06-01T12:00:00+00:00",
    "active": true,
    "user_id": "7c9e6679-7425-40de-944b-e07fc1f90ae7"
}
```

**Response** `404`: User not found.

**Response** `409`: Access key ID already exists.

!!! warning
    The `secret_access_key` is **only returned once** at creation time. Store it securely.

---

### List User Grants

```
GET /admin/users/{user_id}/grants
```

**Auth**: SigV4

Returns grants directly attached to the user (not inherited from teams).

**Response** `200`:

```json
[
    {
        "grant_id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
        "name": "S3ReadOnly",
        "description": "Read-only access to all buckets"
    }
]
```

**Response** `404`: User not found.

---

### Attach Grant to User

```
PUT /admin/users/{user_id}/grants/{grant_id}
```

**Auth**: SigV4

Attach a grant (policy) directly to a user. No request body is required.

**Response** `204`: Grant attached successfully (no body).

**Response** `404`: User or grant not found.

---

### Detach Grant from User

```
DELETE /admin/users/{user_id}/grants/{grant_id}
```

**Auth**: SigV4

Remove a directly-attached grant from a user. No request body is required.

**Response** `204`: Grant detached successfully (no body).

**Response** `404`: Grant not attached to user.

---

### List User Teams

```
GET /admin/users/{user_id}/teams
```

**Auth**: SigV4

Returns the teams the user belongs to.

**Response** `200`:

```json
[
    {
        "team_id": "b2c3d4e5-f6a7-8901-bcde-f12345678901",
        "name": "developers",
        "description": "Development team"
    }
]
```

**Response** `404`: User not found.

---

### Effective User Grants

```
GET /admin/users/{user_id}/effective-grants
```

**Auth**: SigV4

Returns all effective grants for a user, combining direct grants and grants inherited from team membership. Each entry includes a `source` field indicating where the grant comes from.

**Response** `200`:

```json
[
    {
        "grant_id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
        "name": "S3ReadOnly",
        "description": "Read-only access to all buckets",
        "source": "direct"
    },
    {
        "grant_id": "c3d4e5f6-a7b8-9012-cdef-123456789012",
        "name": "S3FullAccess",
        "description": "Full S3 access",
        "source": "team:developers"
    }
]
```

| Source format | Description |
|---------------|-------------|
| `"direct"` | Grant is attached directly to the user |
| `"team:<name>"` | Grant is inherited from the named team |

**Response** `404`: User not found.

---

## Teams

Teams are groups of users. Grants attached to a team are inherited by all members, providing a convenient way to manage permissions for groups of users.

### List Teams

```
GET /admin/teams
```

**Auth**: SigV4

**Response** `200`:

```json
[
    {
        "team_id": "b2c3d4e5-f6a7-8901-bcde-f12345678901",
        "name": "developers",
        "description": "Development team",
        "created_at": "2025-06-01T12:00:00+00:00",
        "member_count": 5,
        "grant_count": 2
    }
]
```

---

### Create Team

```
POST /admin/teams
```

**Auth**: SigV4

**Request body**:

```json
{
    "name": "developers",
    "description": "Development team"
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `name` | string | Yes | Unique team name (must not be empty) |
| `description` | string | No | Team description (defaults to empty string) |

**Response** `201`:

```json
{
    "team_id": "b2c3d4e5-f6a7-8901-bcde-f12345678901",
    "name": "developers",
    "description": "Development team",
    "created_at": "2025-06-01T12:00:00+00:00"
}
```

**Response** `400`: Team name is empty.

**Response** `409`: Team name already exists.

---

### Get Team

```
GET /admin/teams/{team_id}
```

**Auth**: SigV4

Returns detailed team information including counts of members and grants.

**Response** `200`:

```json
{
    "team_id": "b2c3d4e5-f6a7-8901-bcde-f12345678901",
    "name": "developers",
    "description": "Development team",
    "created_at": "2025-06-01T12:00:00+00:00",
    "member_count": 5,
    "grant_count": 2
}
```

**Response** `404`: Team not found.

---

### Update Team

```
PUT /admin/teams/{team_id}
```

**Auth**: SigV4

Update a team's `name` and/or `description`. Both fields are optional; only provided fields are updated.

**Request body**:

```json
{
    "name": "dev-team",
    "description": "Renamed development team"
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `name` | string | No | New unique team name |
| `description` | string | No | New description |

**Response** `204`: Team updated successfully (no body).

**Response** `404`: Team not found.

**Response** `409`: Team name already exists.

---

### Delete Team

```
DELETE /admin/teams/{team_id}
```

**Auth**: SigV4

Deletes the team and removes all member and grant associations.

**Response** `204`: Team deleted successfully (no body).

**Response** `404`: Team not found.

---

### List Team Members

```
GET /admin/teams/{team_id}/members
```

**Auth**: SigV4

**Response** `200`:

```json
[
    {
        "user_id": "7c9e6679-7425-40de-944b-e07fc1f90ae7",
        "username": "alice",
        "description": "Developer",
        "is_root": false
    }
]
```

**Response** `404`: Team not found.

---

### Add Team Member

```
PUT /admin/teams/{team_id}/members/{user_id}
```

**Auth**: SigV4

Add a user to the team. No request body is required.

**Response** `204`: Member added successfully (no body).

**Response** `404`: Team or user not found.

---

### Remove Team Member

```
DELETE /admin/teams/{team_id}/members/{user_id}
```

**Auth**: SigV4

Remove a user from the team. No request body is required.

**Response** `204`: Member removed successfully (no body).

**Response** `404`: Member not found in team.

---

### List Team Grants

```
GET /admin/teams/{team_id}/grants
```

**Auth**: SigV4

Returns grants attached to the team.

**Response** `200`:

```json
[
    {
        "grant_id": "c3d4e5f6-a7b8-9012-cdef-123456789012",
        "name": "S3FullAccess",
        "description": "Full S3 access"
    }
]
```

**Response** `404`: Team not found.

---

### Attach Grant to Team

```
PUT /admin/teams/{team_id}/grants/{grant_id}
```

**Auth**: SigV4

Attach a grant (policy) to a team. All team members will inherit this grant. No request body is required.

**Response** `204`: Grant attached successfully (no body).

**Response** `404`: Team or grant not found.

---

### Detach Grant from Team

```
DELETE /admin/teams/{team_id}/grants/{grant_id}
```

**Auth**: SigV4

Remove a grant from a team. No request body is required.

**Response** `204`: Grant detached successfully (no body).

**Response** `404`: Grant not attached to team.

---

## Grants

Grants are named, reusable IAM-compatible policy documents. They can be attached to users (directly) or teams (inherited by members). The policy evaluation engine uses AWS-style deny-overrides semantics: an explicit Deny in any grant always wins over Allow.

### Policy Document Format

Grants contain an IAM-compatible policy document with this structure:

```json
{
    "Version": "2012-10-17",
    "Statement": [
        {
            "Sid": "OptionalStatementId",
            "Effect": "Allow",
            "Action": ["s3:GetObject", "s3:ListBucket"],
            "Resource": ["arn:aws:s3:::my-bucket", "arn:aws:s3:::my-bucket/*"]
        }
    ]
}
```

**Supported actions**:

| Action | Description |
|--------|-------------|
| `*` | All actions (full wildcard) |
| `s3:*` | All S3 actions |
| `s3:CreateBucket` | Create bucket |
| `s3:DeleteBucket` | Delete bucket |
| `s3:ListBucket` | List objects in a bucket |
| `s3:ListAllMyBuckets` | List all buckets |
| `s3:GetBucketLocation` | Get bucket location |
| `s3:GetBucketEncryption` | Get bucket encryption configuration |
| `s3:PutBucketEncryption` | Set bucket encryption configuration |
| `s3:DeleteBucketEncryption` | Delete bucket encryption configuration |
| `s3:GetObject` | Get (download) an object |
| `s3:PutObject` | Put (upload) an object |
| `s3:DeleteObject` | Delete an object |
| `arca:*` | All Arca admin actions |
| `arca:ViewServerInfo` | View server info and stats |
| `arca:ManageUsers` | Manage users |
| `arca:ManageTeams` | Manage teams |
| `arca:ManageGrants` | Manage grants |
| `arca:ManageCredentials` | Manage credentials |
| `arca:CreatePresignedUrl` | Generate presigned URLs |
| `arca:CreateArchive` | Create archives |

**Resource format**: `*` (all resources) or S3 ARN patterns like `arn:aws:s3:::bucket-name` (bucket-level) and `arn:aws:s3:::bucket-name/*` (object-level). Wildcards `*` and `?` are supported in ARN patterns.

---

### List Grants

```
GET /admin/grants
```

**Auth**: SigV4

**Response** `200`:

```json
[
    {
        "grant_id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
        "name": "S3ReadOnly",
        "description": "Read-only access to all buckets",
        "created_at": "2025-06-01T12:00:00+00:00"
    }
]
```

---

### Create Grant

```
POST /admin/grants
```

**Auth**: SigV4

**Request body**:

```json
{
    "name": "S3ReadOnly",
    "description": "Read-only access to all buckets",
    "document": {
        "Version": "2012-10-17",
        "Statement": [
            {
                "Sid": "AllowRead",
                "Effect": "Allow",
                "Action": [
                    "s3:GetObject",
                    "s3:ListBucket",
                    "s3:ListAllMyBuckets",
                    "s3:GetBucketLocation"
                ],
                "Resource": ["*"]
            }
        ]
    }
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `name` | string | Yes | Unique grant name (must not be empty) |
| `description` | string | No | Grant description (defaults to empty string) |
| `document` | object | Yes | IAM-compatible policy document (validated on creation) |

**Response** `201`:

```json
{
    "grant_id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
    "name": "S3ReadOnly",
    "description": "Read-only access to all buckets",
    "document": {
        "Version": "2012-10-17",
        "Statement": [
            {
                "Sid": "AllowRead",
                "Effect": "Allow",
                "Action": [
                    "s3:GetObject",
                    "s3:ListBucket",
                    "s3:ListAllMyBuckets",
                    "s3:GetBucketLocation"
                ],
                "Resource": ["*"]
            }
        ]
    },
    "created_at": "2025-06-01T12:00:00+00:00",
    "updated_at": "2025-06-01T12:00:00+00:00"
}
```

**Response** `400`: Grant name is empty, policy document is invalid JSON, or policy validation fails (unknown action, invalid resource ARN, missing required fields, wrong version).

**Response** `409`: Grant name already exists.

---

### Get Grant

```
GET /admin/grants/{grant_id}
```

**Auth**: SigV4

Returns the full grant including its policy document.

**Response** `200`:

```json
{
    "grant_id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
    "name": "S3ReadOnly",
    "description": "Read-only access to all buckets",
    "document": {
        "Version": "2012-10-17",
        "Statement": [
            {
                "Effect": "Allow",
                "Action": ["s3:GetObject", "s3:ListBucket"],
                "Resource": ["*"]
            }
        ]
    },
    "created_at": "2025-06-01T12:00:00+00:00",
    "updated_at": "2025-06-01T12:00:00+00:00"
}
```

**Response** `404`: Grant not found.

---

### Update Grant

```
PUT /admin/grants/{grant_id}
```

**Auth**: SigV4

Update a grant's `name`, `description`, and/or `document`. All fields are optional; only provided fields are updated. When `document` is provided, it is validated like at creation time.

**Request body**:

```json
{
    "name": "S3ReadWrite",
    "description": "Read-write access to all buckets",
    "document": {
        "Version": "2012-10-17",
        "Statement": [
            {
                "Effect": "Allow",
                "Action": ["s3:GetObject", "s3:PutObject", "s3:ListBucket"],
                "Resource": ["*"]
            }
        ]
    }
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `name` | string | No | New unique grant name |
| `description` | string | No | New description |
| `document` | object | No | New policy document (validated) |

**Response** `204`: Grant updated successfully (no body).

**Response** `400`: Policy document validation failed.

**Response** `404`: Grant not found.

---

### Delete Grant

```
DELETE /admin/grants/{grant_id}
```

**Auth**: SigV4

Deletes the grant and removes all user and team associations.

**Response** `204`: Grant deleted successfully (no body).

**Response** `404`: Grant not found.

---

## Archive

### Download Objects as Archive

```
POST /admin/archive
```

**Auth**: SigV4

Stream a tar.gz archive containing the requested objects. The response is a streaming `application/gzip` download, no temporary files are created server-side.

**Request body**:

```json
{
    "bucket": "my-bucket",
    "keys": ["documents/report.pdf", "images/logo.png", "data/export.csv"]
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `bucket` | string | Yes | Bucket name |
| `keys` | array | Yes | List of object keys to include (must not be empty) |

**Response** `200`: Streaming `application/gzip` response with `Content-Disposition: attachment; filename="my-bucket.tar.gz"`.

**Response** `400`: Keys list is empty.

**Response** `404`: Bucket not found, or any requested object key not found.

!!! note
    All objects are resolved before streaming begins. If any key is missing, the request fails immediately with a 404 before any data is sent.

---

## Presigned URLs

### Generate Presigned URL

```
POST /admin/presign
```

**Auth**: SigV4

Generate a presigned URL for downloading or uploading an object without requiring credentials.

**Request body**:

```json
{
    "bucket": "my-bucket",
    "key": "path/to/file.txt",
    "method": "GET",
    "expires": 3600,
    "endpoint": "https://arca.example.com:9443"
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `bucket` | string | Yes | Bucket name |
| `key` | string | Yes | Object key |
| `method` | string | No | HTTP method: `"GET"` (default), `"PUT"`, `"HEAD"`, or `"DELETE"` |
| `expires` | integer | No | Expiry in seconds (default: 3600, max: 604800 = 7 days) |
| `endpoint` | string | No | Base URL override (e.g. `"https://arca.example.com:9443"`). When provided, the presigned URL uses this host/scheme instead of the request's Host header. Useful when the client connects through a different endpoint than the public-facing URL. |

**Response** `200`:

```json
{
    "url": "http://localhost:9000/my-bucket/path/to/file.txt?X-Amz-Algorithm=AWS4-HMAC-SHA256&...",
    "expires_at": "2026-03-17T12:00:00Z"
}
```

| Field | Type | Description |
|-------|------|-------------|
| `url` | string | Presigned URL with embedded SigV4 query parameters |
| `expires_at` | string | ISO 8601 expiration timestamp |

The generated URL can be used with `curl`, browsers, or any HTTP client without AWS credentials.

---

## Maintenance Jobs

Long-running maintenance operations (re-encryption, metadata migration) are tracked as jobs and processed by a background worker. Only **one job runs at a time**, progress is persisted across restarts, and jobs can be paused, resumed and cancelled. In a cluster the worker is leader-gated. See the [Migration & Maintenance guide](../guide/maintenance.md) for the full model (live vs maintenance mode, leader-gating, the re-encryption / migrate-db / migrate-topology operations).

Known job types: `noop`, `encrypt`, `decrypt`, `migrate-db`.

### Job object

```json
{
    "id": "8f3c…",
    "job_type": "encrypt",
    "status": "running",
    "mode": "live",
    "params": { "bucket": "my-bucket", "rate_bytes_per_sec": 10485760 },
    "total": 1280,
    "done": 412,
    "rate": 37.5,
    "last_error": null,
    "created_at": "2026-06-24T10:00:00Z",
    "updated_at": "2026-06-24T10:02:11Z",
    "started_at": "2026-06-24T10:00:01Z",
    "finished_at": null
}
```

| Field | Type | Description |
|-------|------|-------------|
| `id` | string | Job id (UUID) |
| `job_type` | string | `noop` \| `encrypt` \| `decrypt` \| `migrate-db` |
| `status` | string | `pending` \| `running` \| `paused` \| `completed` \| `failed` \| `cancelled` |
| `mode` | string | `live` (zero-downtime) \| `maintenance` (drains the S3 API on the node) |
| `params` | object | Job-type-specific parameters |
| `total` / `done` | integer | Progress counters |
| `rate` | number | Current throughput (items/sec) |
| `last_error` | string \| null | Error message if the job failed |
| `created_at` / `updated_at` / `started_at` / `finished_at` | string \| null | ISO 8601 timestamps |

### Create a Job

```
POST /admin/maintenance/jobs
```

**Auth**: SigV4 (admin)

**Request body**:

```json
{
    "type": "encrypt",
    "mode": "live",
    "params": { "bucket": "my-bucket", "prefix": "logs/", "rate_bytes_per_sec": 10485760 }
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `type` | string | Yes | One of the known job types |
| `mode` | string | No | `live` (default) or `maintenance` |
| `params` | object | No | Job-type parameters (see below) |

Job-type parameters:

- **`encrypt` / `decrypt`**: `bucket` (optional, all buckets if omitted), `prefix` (optional, all keys if omitted), `rate_bytes_per_sec` (optional, live mode only, `0` = unlimited).
- **`migrate-db`**: `target` (`"sqlite"` \| `"postgres"`, required), `force` (boolean, replace a non-empty destination).
- **`noop`**: `n` (steps), `delay_ms` (optional per-step sleep) — a test job.

**Response** `201`: the created job object.

**Response** `400`: unknown job type or invalid mode.

**Response** `409`: a maintenance job is already active.

### List Jobs

```
GET /admin/maintenance/jobs
```

**Auth**: SigV4 (admin)

**Query parameters**: `limit` — history page size (default 50, max 500).

**Response** `200`:

```json
{
    "active": { "...": "the in-flight job, or null" },
    "jobs":   [ "...recent history, newest first..." ]
}
```

### Get a Job (with logs)

```
GET /admin/maintenance/jobs/{id}
```

**Auth**: SigV4 (admin)

**Response** `200`:

```json
{
    "job":  { "...": "the job object" },
    "logs": [ { "level": "info", "message": "job started" } ]
}
```

**Response** `404`: no such job.

### Pause / Resume / Cancel

```
POST   /admin/maintenance/jobs/{id}/pause     # running|pending  -> paused
POST   /admin/maintenance/jobs/{id}/resume    # paused           -> running
DELETE /admin/maintenance/jobs/{id}           # any non-terminal -> cancelled
```

**Auth**: SigV4 (admin)

Each returns `200` with the updated job object.

**Response** `404`: no such job.

**Response** `409`: the job is not in a state from which the transition is allowed.
