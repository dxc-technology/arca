# Admin API

JSON-based administration API for managing the Arca server. All endpoints live under `/admin/*` on the same port as the S3 API (default 9000).

## Authentication

All endpoints except `/admin/health` require **AWS SigV4** authentication — the same mechanism used for S3 requests. Only credentials with the **admin** flag can access admin endpoints. Non-admin credentials receive a `403 AccessDenied` response but can still use the S3 API normally.

## Error Responses

Admin API errors are returned as JSON (not S3 XML):

```json
{
    "error": "ErrorCode",
    "message": "Human-readable description"
}
```

---

## Endpoints

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
    "total_size_bytes": 1073741824
}
```

---

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

### Delete Credential

```
DELETE /admin/credentials/{access_key_id}
```

**Auth**: SigV4

**Response** `204`: Credential deleted successfully (no body).

**Response** `404`: Credential not found.

**Response** `409`: Cannot delete the last active credential or last admin credential (prevents lockout).
