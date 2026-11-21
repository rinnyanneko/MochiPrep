-- Agent templates: saved agent form settings a user picks when creating an agent. The built-in
-- presets live in code (mochi-store/src/templates.rs) so a release can improve them; this table
-- holds the user's own templates, and agent_template_hidden the built-ins the user deleted, so
-- "restore built-ins" can bring them back. A template only pre-fills the agent form: the agent is
-- still created and validated through the agent routes and every order still meets the risk engine.
CREATE TABLE agent_templates (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL,
  description TEXT NOT NULL,
  strategy TEXT NOT NULL,
  timeframe TEXT NOT NULL,
  interval_sec INTEGER NOT NULL,
  execution_mode TEXT NOT NULL,
  risk TEXT NOT NULL,
  use_news INTEGER NOT NULL,
  allow_trailing_stop INTEGER NOT NULL,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);

CREATE TABLE agent_template_hidden (
  id TEXT PRIMARY KEY
);
