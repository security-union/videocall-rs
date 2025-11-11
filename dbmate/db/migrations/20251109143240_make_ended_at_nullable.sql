-- migrate:up
ALTER TABLE meetings
ALTER COLUMN ended_at DROP NOT NULL,
ALTER COLUMN ended_at SET DEFAULT NULL;

-- migrate:down
ALTER TABLE meetings 
ALTER COLUMN ended_at SET NOT NULL;