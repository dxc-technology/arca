# Admin API

JSON-based administration API for managing the Arca server. All endpoints live under `/admin/*` on the same port as the S3 API (default 9000).

## Authentication

All endpoints except `/admin/health` require **AWS SigV4** authentication — the same mechanism used for S3 requests. Use the same credentials you configured for S3 access.

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
    "version": "0.1.0",
    "uptime_seconds": 3600
}
```

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
        "active": true
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
    "description": "CI/CD pipeline"
}
```

The `description` field is optional (defaults to empty string).

**Response** `201`:

```json
{
    "access_key_id": "AKXYZ123456789ABCDEF",
    "secret_access_key": "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
    "description": "CI/CD pipeline",
    "created_at": "2025-06-01T12:00:00+00:00",
    "active": true
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

**Response** `409`: Cannot delete the last active credential (prevents lockout).
