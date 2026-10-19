-- Idempotent: safe to re-run.
-- The job table records every job-based endpoint, not only videos: 3D mesh
-- generation (`/v1/meshes`) works the same way, a create that returns an id
-- and follow-ups that carry only that id. `kind` names the endpoint that
-- created the job, and every lookup matches it, so a job is only ever found
-- through its own endpoint. Rows written before this column existed are
-- videos. The table keeps its original name.
alter table video_jobs add column if not exists kind text not null default 'video';
