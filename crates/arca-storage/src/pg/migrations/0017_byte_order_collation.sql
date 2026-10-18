-- Byte order for every name S3 lists in order.
--
-- S3 orders keys, version ids, upload ids and bucket names by their bytes. A
-- database initialised with a glibc locale (en_US.UTF-8, the default of the
-- Debian-based postgres image and of most distribution packages) sorts and
-- compares TEXT by that locale instead, which ignores case and punctuation at
-- first: listings came back out of S3 order, and the delimiter grouping of
-- ListObjects, which compares keys in Rust, could skip or repeat entries
-- across pages. COLLATE "C" compares bytes whatever the database locale.
--
-- Changing only the collation keeps the rows as they are; PostgreSQL rebuilds
-- the indexes on these columns.
ALTER TABLE buckets
    ALTER COLUMN name TYPE TEXT COLLATE "C";
ALTER TABLE objects
    ALTER COLUMN key TYPE TEXT COLLATE "C",
    ALTER COLUMN version_id TYPE TEXT COLLATE "C";
ALTER TABLE multipart_uploads
    ALTER COLUMN key TYPE TEXT COLLATE "C",
    ALTER COLUMN upload_id TYPE TEXT COLLATE "C";
