-- Granular S3 actions (same change as SQLite v29).
--
-- Bucket subresources and object tagging, versions, retention, ... now have
-- their own AWS actions instead of riding on ListBucket / CreateBucket /
-- GetObject. Two consequences for stored grants:
--
-- - Arca's own s3:Get/Put/DeleteBucketEncryption become AWS's
--   s3:GetEncryptionConfiguration / s3:PutEncryptionConfiguration (deleting
--   needs the Put action). Renamed in every grant, so a grant keeps doing what
--   it did and still validates.
-- - The built-in S3ReadOnlyAccess (not editable by users) gains the read
--   actions of everything it could read before.
--
-- updated_at is left alone: every node of a cluster runs the same rewrite, so
-- the copies stay identical.
UPDATE grants SET document =
    replace(replace(replace(document::text,
        '"s3:GetBucketEncryption"', '"s3:GetEncryptionConfiguration"'),
        '"s3:PutBucketEncryption"', '"s3:PutEncryptionConfiguration"'),
        '"s3:DeleteBucketEncryption"', '"s3:PutEncryptionConfiguration"')::jsonb
WHERE document::text LIKE '%BucketEncryption"%';

UPDATE grants SET document = '{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Action":["s3:ListAllMyBuckets","s3:ListBucket","s3:ListBucketVersions","s3:ListBucketMultipartUploads","s3:GetBucketLocation","s3:GetEncryptionConfiguration","s3:GetBucketVersioning","s3:GetLifecycleConfiguration","s3:GetBucketTagging","s3:GetReplicationConfiguration","s3:GetBucketNotification","s3:GetBucketObjectLockConfiguration","arca:GetBucketCompression","s3:GetObject","s3:GetObjectVersion","s3:GetObjectAttributes","s3:GetObjectVersionAttributes","s3:GetObjectTagging","s3:GetObjectVersionTagging","s3:GetObjectRetention","s3:GetObjectLegalHold","s3:ListMultipartUploadParts"],"Resource":["*"]}]}'::jsonb
WHERE grant_id = 'grant-s3-read-only';
