# API Reference

Arca implements 19 S3 operations in its MVP. All operations follow the standard [AWS S3 REST API](https://docs.aws.amazon.com/AmazonS3/latest/API/Welcome.html) specification.

## Authentication

All requests must be signed with AWS Signature V4. Arca supports both the `Authorization` header and query string authentication methods.

## Bucket Operations

| Operation | Method | Path | Description |
|-----------|--------|------|-------------|
| [ListBuckets](#listbuckets) | `GET` | `/` | List all buckets |
| [CreateBucket](#createbucket) | `PUT` | `/{bucket}` | Create a new bucket |
| [HeadBucket](#headbucket) | `HEAD` | `/{bucket}` | Check if a bucket exists |
| [DeleteBucket](#deletebucket) | `DELETE` | `/{bucket}` | Delete an empty bucket |
| [GetBucketLocation](#getbucketlocation) | `GET` | `/{bucket}?location` | Get bucket region |

## Object Operations

| Operation | Method | Path | Query Params | Description |
|-----------|--------|------|--------------|-------------|
| [PutObject](#putobject) | `PUT` | `/{bucket}/{key+}` | — | Upload an object |
| [GetObject](#getobject) | `GET` | `/{bucket}/{key+}` | — | Download an object |
| [HeadObject](#headobject) | `HEAD` | `/{bucket}/{key+}` | — | Get object metadata |
| [DeleteObject](#deleteobject) | `DELETE` | `/{bucket}/{key+}` | — | Delete an object |
| [DeleteObjects](#deleteobjects) | `POST` | `/{bucket}` | `delete` | Delete multiple objects in one request |
| [CopyObject](#copyobject) | `PUT` | `/{bucket}/{key+}` | — | Copy an object (uses `x-amz-copy-source` header) |

## Listing Operations

| Operation | Method | Path | Query Params | Description |
|-----------|--------|------|--------------|-------------|
| [ListObjectsV1](#listobjectsv1) | `GET` | `/{bucket}` | — | List objects in a bucket (legacy) |
| [ListObjectsV2](#listobjectsv2) | `GET` | `/{bucket}` | `list-type=2` | List objects in a bucket |

## Multipart Upload Operations

| Operation | Method | Path | Query Params | Description |
|-----------|--------|------|--------------|-------------|
| [CreateMultipartUpload](#createmultipartupload) | `POST` | `/{bucket}/{key+}` | `uploads` | Initiate a multipart upload |
| [UploadPart](#uploadpart) | `PUT` | `/{bucket}/{key+}` | `partNumber`, `uploadId` | Upload a part |
| [CompleteMultipartUpload](#completemultipartupload) | `POST` | `/{bucket}/{key+}` | `uploadId` | Complete a multipart upload |
| [AbortMultipartUpload](#abortmultipartupload) | `DELETE` | `/{bucket}/{key+}` | `uploadId` | Abort a multipart upload |

---

## Operation Details

### ListBuckets

List all buckets owned by the authenticated user.

```
GET / HTTP/1.1
```

Returns `ListAllMyBucketsResult` XML with bucket names and creation dates.

### CreateBucket

Create a new bucket.

```
PUT /{bucket} HTTP/1.1
```

Bucket names must follow [S3 naming rules](https://docs.aws.amazon.com/AmazonS3/latest/userguide/bucketnamingrules.html): 3-63 characters, lowercase letters, numbers, hyphens, no consecutive periods or IP-like names.

### HeadBucket

Check whether a bucket exists and you have permission to access it.

```
HEAD /{bucket} HTTP/1.1
```

Returns `200 OK` if the bucket exists, `404 Not Found` otherwise.

### DeleteBucket

Delete a bucket. The bucket must be empty.

```
DELETE /{bucket} HTTP/1.1
```

Returns `204 No Content` on success, `409 BucketNotEmpty` if the bucket contains objects.

### GetBucketLocation

Get the region of a bucket.

```
GET /{bucket}?location HTTP/1.1
```

Returns a `LocationConstraint` XML element. Arca always returns an empty location constraint (equivalent to `us-east-1`).

### PutObject

Upload an object. The request body is streamed directly to storage — never buffered in memory.

```
PUT /{bucket}/{key+} HTTP/1.1
Content-Type: application/octet-stream
```

ETag is computed as the hex-encoded MD5 of the object content.

### GetObject

Download an object. Supports `Range` header for partial content retrieval.

```
GET /{bucket}/{key+} HTTP/1.1
```

Returns the object body with `Content-Type`, `Content-Length`, `ETag`, and `Last-Modified` headers.

### HeadObject

Retrieve object metadata without downloading the body.

```
HEAD /{bucket}/{key+} HTTP/1.1
```

Returns the same headers as GetObject, but with no response body.

### DeleteObject

Delete an object.

```
DELETE /{bucket}/{key+} HTTP/1.1
```

Returns `204 No Content` on success. Deleting a non-existent key is not an error.

!!! tip "Directory Markers"
    S3 uses zero-byte objects with keys ending in `/` as directory markers (e.g., `photos/`). Arca preserves trailing slashes in object keys, so you can create, list, and delete directory markers just like any other object. When listing with a delimiter, directory markers whose key matches the listing prefix are excluded from `Contents` (they appear only as `CommonPrefixes`).

### DeleteObjects

Delete multiple objects in a single request.

```xml
POST /{bucket}?delete HTTP/1.1

<Delete>
  <Quiet>true</Quiet>
  <Object><Key>file1.txt</Key></Object>
  <Object><Key>file2.txt</Key></Object>
  <Object><Key>photos/</Key></Object>
</Delete>
```

When `Quiet` is `true`, the response only includes errors. When `false` (default), the response includes both successfully deleted keys and errors.

Returns `DeleteResult` XML. Deleting non-existent keys is not an error.

### CopyObject

Copy an object within or across buckets. The source is specified via the `x-amz-copy-source` header.

```
PUT /{bucket}/{key+} HTTP/1.1
x-amz-copy-source: /{source-bucket}/{source-key}
```

Returns `CopyObjectResult` XML with the new ETag and last modified timestamp.

### ListObjectsV1

List objects in a bucket (legacy API). Prefer ListObjectsV2 for new applications.

```
GET /{bucket} HTTP/1.1
```

| Parameter | Description |
|-----------|-------------|
| `prefix` | Limits results to keys beginning with the specified prefix |
| `delimiter` | Groups keys that share a common prefix (typically `/`) |
| `max-keys` | Maximum number of keys to return (default 1000) |
| `marker` | Start listing after this key |

Returns `ListBucketResult` XML with `Contents`, `CommonPrefixes`, `IsTruncated`, and `NextMarker`.

### ListObjectsV2

List objects in a bucket with optional prefix and delimiter filtering.

```
GET /{bucket}?list-type=2 HTTP/1.1
```

| Parameter | Description |
|-----------|-------------|
| `prefix` | Limits results to keys beginning with the specified prefix |
| `delimiter` | Groups keys that share a common prefix (typically `/`) |
| `max-keys` | Maximum number of keys to return (default 1000) |
| `continuation-token` | Token from a previous truncated response |
| `start-after` | Start listing after this key |

Returns `ListBucketResult` XML with `Contents`, `CommonPrefixes`, `IsTruncated`, and `NextContinuationToken`.

### CreateMultipartUpload

Initiate a multipart upload and obtain an upload ID.

```
POST /{bucket}/{key+}?uploads HTTP/1.1
```

Returns `InitiateMultipartUploadResult` XML containing the `UploadId`.

### UploadPart

Upload a part for a multipart upload.

```
PUT /{bucket}/{key+}?partNumber={n}&uploadId={id} HTTP/1.1
```

Part numbers range from 1 to 10000. Each part (except the last) must be at least 5 MB.

### CompleteMultipartUpload

Complete a multipart upload by assembling previously uploaded parts.

```xml
POST /{bucket}/{key+}?uploadId={id} HTTP/1.1

<CompleteMultipartUpload>
  <Part>
    <PartNumber>1</PartNumber>
    <ETag>"etag1"</ETag>
  </Part>
  <Part>
    <PartNumber>2</PartNumber>
    <ETag>"etag2"</ETag>
  </Part>
</CompleteMultipartUpload>
```

The resulting ETag is a composite: `hex(MD5(binary_MD5(part1) || binary_MD5(part2) || ...))-{count}`.

### AbortMultipartUpload

Abort a multipart upload and delete all uploaded parts.

```
DELETE /{bucket}/{key+}?uploadId={id} HTTP/1.1
```

Returns `204 No Content` on success.

## Intentional divergences from AWS S3

Arca targets drop-in AWS S3 compatibility: what AWS accepts, Arca accepts; what AWS rejects, Arca rejects — with the following deliberate, documented exceptions.

### Object Lock on existing empty buckets

AWS only allows Object Lock to be enabled at bucket creation (`x-amz-bucket-object-lock-enabled: true`); `PutObjectLockConfiguration` on a bucket that was not created with Object Lock returns `409 InvalidBucketState`.

Arca additionally accepts `PutObjectLockConfiguration` on an **existing bucket that is still empty** (auto-enabling versioning, as AWS does at creation). A late enable on an empty bucket cannot retroactively weaken WORM protection on any object, so it is safe; non-empty buckets are still rejected with `409 InvalidBucketState`. This is a console-UX convenience: enable Object Lock from a bucket's settings page without recreating the bucket.

Implication: the drop-in direction (AWS-targeted clients running against Arca) is unaffected, but code that relies on this relaxation is not portable to AWS, which would reject it. This divergence is also why the Ceph s3-test `test_object_lock_put_obj_lock_invalid_bucket` is expected to fail.
