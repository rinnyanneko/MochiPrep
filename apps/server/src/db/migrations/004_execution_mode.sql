-- Existing agents keep trading automatically. New ones default to SIGNAL through the API.
ALTER TABLE agents ADD COLUMN execution_mode TEXT NOT NULL DEFAULT 'AUTO';
