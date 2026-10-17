-- Multipart checksum and part layout (TD-046).
--
-- `objects.parts` is the layout of an object assembled by CompleteMultipartUpload,
-- [{part_number, size, checksum}]; NULL for any other object and for multipart
-- objects completed earlier. `multipart_uploads.checksum_type` is COMPOSITE or
-- FULL_OBJECT as chosen at CreateMultipartUpload; NULL means the default of the
-- algorithm.
ALTER TABLE objects ADD COLUMN parts JSONB;
ALTER TABLE multipart_uploads ADD COLUMN checksum_type TEXT;
