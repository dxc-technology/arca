-- Who started a multipart upload (TD-042).
--
-- ListParts and ListMultipartUploads report the initiator as `Owner` and
-- `Initiator`. Uploads in flight at upgrade time keep the `root` default:
-- who started them was never recorded.
ALTER TABLE multipart_uploads ADD COLUMN initiator TEXT NOT NULL DEFAULT 'root';
