-- Plan Pipeline (Sub-Agent + Skill Architecture phase 5): link a work_item to the plan
-- artifact the Plan Pipeline produced for it, so the Build pipeline (P6) and the GUI can
-- resolve "the plan for this work item" without re-deriving the slug.
--
-- Purely additive: existing rows get plan_path = NULL (no plan linked yet). The value is a
-- workspace-relative path to the rendered `plan.md` (e.g. `.agents/<slug>/plan.md`), NOT an
-- absolute host path — a plan is an in-workspace artifact reverted by the worktree
-- compensator like any other workspace write.
ALTER TABLE work_items ADD COLUMN plan_path TEXT;
