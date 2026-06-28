-- Prevent duplicate skill names inserted by overlapping synthesis windows.
CREATE UNIQUE INDEX IF NOT EXISTS idx_skills_name ON kms_skills(name);

-- Enable efficient time-range queries for trace synthesis (was a full scan).
CREATE INDEX IF NOT EXISTS idx_traces_created ON kms_task_traces(created_at);
