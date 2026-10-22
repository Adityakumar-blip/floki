CREATE TABLE users (
  id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  email TEXT NOT NULL UNIQUE,
  name TEXT,
  password_hash TEXT,
  google_sub TEXT UNIQUE,
  email_verified BOOLEAN NOT NULL DEFAULT FALSE,
  created_at BIGINT NOT NULL
);

CREATE TABLE sessions (
  token_hash TEXT PRIMARY KEY,
  user_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  created_at BIGINT NOT NULL
);
CREATE INDEX sessions_user_idx ON sessions(user_id);

CREATE TABLE email_codes (
  user_id BIGINT PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
  code_hash TEXT NOT NULL,
  expires_at BIGINT NOT NULL,
  attempts INT NOT NULL DEFAULT 0
);

CREATE TABLE teams (
  id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  name TEXT NOT NULL,
  created_at BIGINT NOT NULL
);

CREATE TABLE team_members (
  team_id BIGINT NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
  user_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  role TEXT NOT NULL CHECK (role IN ('admin', 'member')),
  PRIMARY KEY (team_id, user_id)
);
CREATE INDEX team_members_user_idx ON team_members(user_id);

CREATE TABLE team_invites (
  team_id BIGINT NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
  email TEXT NOT NULL,
  role TEXT NOT NULL CHECK (role IN ('admin', 'member')),
  PRIMARY KEY (team_id, email)
);
CREATE INDEX team_invites_email_idx ON team_invites(email);

CREATE TABLE workspaces (
  id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  name TEXT NOT NULL,
  team_id BIGINT REFERENCES teams(id) ON DELETE SET NULL,
  data JSONB NOT NULL,
  version BIGINT NOT NULL DEFAULT 1,
  updated_at BIGINT NOT NULL,
  updated_by BIGINT REFERENCES users(id) ON DELETE SET NULL
);
CREATE INDEX workspaces_team_idx ON workspaces(team_id);

CREATE TABLE workspace_members (
  workspace_id BIGINT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
  user_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  role TEXT NOT NULL CHECK (role IN ('admin', 'editor', 'viewer')),
  PRIMARY KEY (workspace_id, user_id)
);
CREATE INDEX workspace_members_user_idx ON workspace_members(user_id);

CREATE TABLE workspace_invites (
  workspace_id BIGINT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
  email TEXT NOT NULL,
  role TEXT NOT NULL CHECK (role IN ('admin', 'editor', 'viewer')),
  PRIMARY KEY (workspace_id, email)
);
CREATE INDEX workspace_invites_email_idx ON workspace_invites(email);
