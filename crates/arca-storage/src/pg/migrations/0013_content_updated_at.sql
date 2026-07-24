-- Phase 30 (TD-021): re-encryption rewrites a row's blob/algorithm/key in place
-- WITHOUT bumping last_modified (the plaintext is unchanged), exactly like a
-- lock-state change. Reusing lock_updated_at as the tie dimension made the two
-- clobber each other during anti-entropy: a re-encryption could revert a newer
-- lock state and vice versa. This column is the content register's own
-- timestamp so apply_remote_object can merge the lock columns (by
-- lock_updated_at) and the content columns (by content_updated_at)
-- independently. NULL = never re-encrypted; existing rows stay NULL (any
-- post-upgrade re-encryption beats them).
ALTER TABLE objects ADD COLUMN content_updated_at TIMESTAMPTZ;
