-- Add connector_type column to notification_events for modular connector support.
ALTER TABLE notification_events ADD COLUMN connector_type TEXT NOT NULL DEFAULT 'webhook';
