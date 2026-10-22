use crate::auth::{norm_email, now_ms, AuthUser};
use crate::{err, ApiError, AppState, Res};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::PgConnection;

type NoContent = Result<StatusCode, ApiError>;

fn pid(s: &str) -> Result<i64, ApiError> {
    s.parse().map_err(|_| err(404, "Not found"))
}

fn rank(role: &str) -> u8 {
    match role {
        "admin" => 3,
        "editor" => 2,
        _ => 1,
    }
}

// Team and workspace membership share identical semantics, so one set of helpers is parameterised by table.
struct Kind {
    members: &'static str,
    invites: &'static str,
    col: &'static str,
    roles: &'static [&'static str],
}

const TEAM: Kind = Kind { members: "team_members", invites: "team_invites", col: "team_id", roles: &["admin", "member"] };
const WS: Kind =
    Kind { members: "workspace_members", invites: "workspace_invites", col: "workspace_id", roles: &["admin", "editor", "viewer"] };

impl Kind {
    async fn role_of(&self, c: &mut PgConnection, id: i64, uid: i64) -> sqlx::Result<Option<String>> {
        sqlx::query_scalar(&format!("SELECT role FROM {} WHERE {}=$1 AND user_id=$2", self.members, self.col))
            .bind(id)
            .bind(uid)
            .fetch_optional(c)
            .await
    }

    fn check_role<'a>(&self, role: &'a str) -> Result<&'a str, ApiError> {
        self.roles.contains(&role).then_some(role).ok_or_else(|| err(400, "Invalid role"))
    }

    // Locking every admin row serialises concurrent demotions, so two requests can't each see "another admin remains".
    async fn guard_last_admin(&self, c: &mut PgConnection, id: i64, uid: i64, new_role: &str) -> Result<(), ApiError> {
        if new_role == "admin" {
            return Ok(());
        }
        let admins: Vec<i64> = sqlx::query_scalar(&format!(
            "SELECT user_id FROM {} WHERE {}=$1 AND role='admin' FOR UPDATE",
            self.members, self.col
        ))
        .bind(id)
        .fetch_all(c)
        .await?;
        if admins.contains(&uid) && admins.len() <= 1 {
            return Err(err(400, "Cannot remove or demote the last admin"));
        }
        Ok(())
    }

    async fn list(&self, c: &mut PgConnection, id: i64) -> Res {
        let members: Vec<(i64, String, String, String)> = sqlx::query_as(&format!(
            "SELECT u.id,u.email,COALESCE(u.name,u.email),m.role FROM {} m JOIN users u ON u.id=m.user_id WHERE m.{}=$1 ORDER BY u.email",
            self.members, self.col
        ))
        .bind(id)
        .fetch_all(&mut *c)
        .await?;
        let invites: Vec<(String, String)> =
            sqlx::query_as(&format!("SELECT email,role FROM {} WHERE {}=$1 ORDER BY email", self.invites, self.col))
                .bind(id)
                .fetch_all(&mut *c)
                .await?;
        let members: Vec<Value> = members
            .into_iter()
            .map(|(uid, email, name, role)| json!({ "user_id": uid.to_string(), "email": email, "name": name, "role": role }))
            .collect();
        let invites: Vec<Value> = invites.into_iter().map(|(email, role)| json!({ "email": email, "role": role })).collect();
        Ok(Json(json!({ "members": members, "invites": invites })))
    }

    async fn add(&self, c: &mut PgConnection, id: i64, b: &MemberBody) -> Res {
        let role = self.check_role(&b.role)?;
        let email = norm_email(&b.email).ok_or_else(|| err(400, "Invalid email address"))?;
        // Unverified accounts may be squatters, so they only get an invite that applies once verified.
        let user: Option<i64> = sqlx::query_scalar("SELECT id FROM users WHERE email=$1 AND email_verified")
            .bind(&email)
            .fetch_optional(&mut *c)
            .await?;
        let status = match user {
            Some(uid) => {
                self.guard_last_admin(c, id, uid, role).await?;
                sqlx::query(&format!(
                    "INSERT INTO {}({},user_id,role) VALUES($1,$2,$3) ON CONFLICT({},user_id) DO UPDATE SET role=EXCLUDED.role",
                    self.members, self.col, self.col
                ))
                .bind(id)
                .bind(uid)
                .bind(role)
                .execute(&mut *c)
                .await?;
                sqlx::query(&format!("DELETE FROM {} WHERE {}=$1 AND email=$2", self.invites, self.col))
                    .bind(id)
                    .bind(&email)
                    .execute(&mut *c)
                    .await?;
                "added"
            }
            None => {
                sqlx::query(&format!(
                    "INSERT INTO {}({},email,role) VALUES($1,$2,$3) ON CONFLICT({},email) DO UPDATE SET role=EXCLUDED.role",
                    self.invites, self.col, self.col
                ))
                .bind(id)
                .bind(&email)
                .bind(role)
                .execute(&mut *c)
                .await?;
                "invited"
            }
        };
        Ok(Json(json!({ "status": status })))
    }

    async fn patch(&self, c: &mut PgConnection, id: i64, uid: i64, role: &str) -> NoContent {
        let role = self.check_role(role)?;
        if self.role_of(c, id, uid).await?.is_none() {
            return Err(err(404, "Member not found"));
        }
        self.guard_last_admin(c, id, uid, role).await?;
        sqlx::query(&format!("UPDATE {} SET role=$1 WHERE {}=$2 AND user_id=$3", self.members, self.col))
            .bind(role)
            .bind(id)
            .bind(uid)
            .execute(c)
            .await?;
        Ok(StatusCode::NO_CONTENT)
    }

    async fn remove(&self, c: &mut PgConnection, id: i64, uid: i64) -> NoContent {
        if self.role_of(c, id, uid).await?.is_none() {
            return Err(err(404, "Member not found"));
        }
        self.guard_last_admin(c, id, uid, "").await?;
        sqlx::query(&format!("DELETE FROM {} WHERE {}=$1 AND user_id=$2", self.members, self.col))
            .bind(id)
            .bind(uid)
            .execute(c)
            .await?;
        Ok(StatusCode::NO_CONTENT)
    }

    async fn remove_invite(&self, c: &mut PgConnection, id: i64, email: &str) -> NoContent {
        let n = sqlx::query(&format!("DELETE FROM {} WHERE {}=$1 AND email=$2", self.invites, self.col))
            .bind(id)
            .bind(email.trim().to_lowercase())
            .execute(c)
            .await?;
        if n.rows_affected() == 0 {
            return Err(err(404, "Invite not found"));
        }
        Ok(StatusCode::NO_CONTENT)
    }
}

#[derive(Deserialize)]
pub struct MemberBody {
    email: String,
    role: String,
}

#[derive(Deserialize)]
pub struct RoleBody {
    role: String,
}

#[derive(Deserialize)]
pub struct NameBody {
    name: String,
}

fn clean_name(n: &str) -> Result<&str, ApiError> {
    let n = n.trim();
    if n.is_empty() || n.len() > 200 {
        return Err(err(400, "Name must be 1-200 characters"));
    }
    Ok(n)
}

// Non-members get 404 so existence isn't leaked; members lacking the needed role get 403.
async fn need_team(c: &mut PgConnection, id: i64, uid: i64, admin: bool) -> Result<(), ApiError> {
    match TEAM.role_of(c, id, uid).await?.as_deref() {
        None => Err(err(404, "Team not found")),
        Some(r) if admin && r != "admin" => Err(err(403, "Team admin access required")),
        _ => Ok(()),
    }
}

pub async fn create_team(State(st): State<AppState>, u: AuthUser, Json(b): Json<NameBody>) -> Res {
    let name = clean_name(&b.name)?;
    let mut tx = st.db.begin().await?;
    let id: i64 = sqlx::query_scalar("INSERT INTO teams(name,created_at) VALUES($1,$2) RETURNING id")
        .bind(name)
        .bind(now_ms())
        .fetch_one(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO team_members(team_id,user_id,role) VALUES($1,$2,'admin')").bind(id).bind(u.id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(Json(json!({ "id": id.to_string(), "name": name, "role": "admin" })))
}

pub async fn team_members(State(st): State<AppState>, u: AuthUser, Path(id): Path<String>) -> Res {
    let (mut c, id) = (st.db.acquire().await?, pid(&id)?);
    need_team(&mut c, id, u.id, false).await?;
    TEAM.list(&mut c, id).await
}

pub async fn team_add(State(st): State<AppState>, u: AuthUser, Path(id): Path<String>, Json(b): Json<MemberBody>) -> Res {
    let (mut tx, id) = (st.db.begin().await?, pid(&id)?);
    need_team(&mut tx, id, u.id, true).await?;
    let r = TEAM.add(&mut tx, id, &b).await?;
    tx.commit().await?;
    Ok(r)
}

pub async fn team_patch(
    State(st): State<AppState>,
    u: AuthUser,
    Path((id, uid)): Path<(String, String)>,
    Json(b): Json<RoleBody>,
) -> NoContent {
    let (mut tx, id, uid) = (st.db.begin().await?, pid(&id)?, pid(&uid)?);
    need_team(&mut tx, id, u.id, true).await?;
    TEAM.patch(&mut tx, id, uid, &b.role).await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn team_remove(State(st): State<AppState>, u: AuthUser, Path((id, uid)): Path<(String, String)>) -> NoContent {
    let (mut tx, id, uid) = (st.db.begin().await?, pid(&id)?, pid(&uid)?);
    need_team(&mut tx, id, u.id, uid != u.id).await?;
    TEAM.remove(&mut tx, id, uid).await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn team_invite_delete(State(st): State<AppState>, u: AuthUser, Path((id, email)): Path<(String, String)>) -> NoContent {
    let (mut c, id) = (st.db.acquire().await?, pid(&id)?);
    need_team(&mut c, id, u.id, true).await?;
    TEAM.remove_invite(&mut c, id, &email).await
}

// A direct workspace role wins; otherwise team membership maps admin->admin, member->editor.
const WS_ROLE: &str = "COALESCE(wm.role, CASE tm.role WHEN 'admin' THEN 'admin' ELSE 'editor' END)";
const WS_FROM: &str = "FROM workspaces w LEFT JOIN teams t ON t.id=w.team_id \
    LEFT JOIN workspace_members wm ON wm.workspace_id=w.id AND wm.user_id=$1 \
    LEFT JOIN team_members tm ON tm.team_id=w.team_id AND tm.user_id=$1 \
    WHERE (wm.user_id IS NOT NULL OR tm.user_id IS NOT NULL)";

type WsRow = (i64, String, Option<i64>, Option<String>, i64, i64, String);

fn ws_cols() -> String {
    format!("w.id,w.name,w.team_id,t.name,w.version,w.updated_at,{WS_ROLE}")
}

fn ws_json((id, name, team_id, team_name, version, updated_at, role): WsRow) -> Value {
    json!({
        "id": id.to_string(), "name": name, "team_id": team_id.map(|t| t.to_string()), "team_name": team_name,
        "version": version, "updated_at": updated_at, "role": role,
    })
}

async fn need_ws(c: &mut PgConnection, id: i64, uid: i64, min: &str) -> Result<(), ApiError> {
    let role: Option<String> = sqlx::query_scalar(&format!("SELECT {WS_ROLE} {WS_FROM} AND w.id=$2"))
        .bind(uid)
        .bind(id)
        .fetch_optional(c)
        .await?;
    match role {
        None => Err(err(404, "Workspace not found")),
        Some(r) if rank(&r) < rank(min) => Err(err(403, "You don't have permission to do that in this workspace")),
        _ => Ok(()),
    }
}

pub async fn ws_list(State(st): State<AppState>, u: AuthUser) -> Res {
    let rows: Vec<WsRow> = sqlx::query_as(&format!("SELECT {} {WS_FROM} ORDER BY lower(w.name), w.id", ws_cols()))
        .bind(u.id)
        .fetch_all(&st.db)
        .await?;
    Ok(Json(Value::Array(rows.into_iter().map(ws_json).collect())))
}

#[derive(Deserialize)]
pub struct WsCreate {
    name: String,
    team_id: Option<String>,
    data: Value,
}

pub async fn ws_create(State(st): State<AppState>, u: AuthUser, Json(b): Json<WsCreate>) -> Res {
    let name = clean_name(&b.name)?;
    let team = match &b.team_id {
        Some(t) => Some(pid(t).map_err(|_| err(403, "You are not a member of that team"))?),
        None => None,
    };
    let mut tx = st.db.begin().await?;
    if let Some(t) = team {
        if TEAM.role_of(&mut tx, t, u.id).await?.is_none() {
            return Err(err(403, "You are not a member of that team"));
        }
    }
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO workspaces(name,team_id,data,version,updated_at,updated_by) VALUES($1,$2,$3,1,$4,$5) RETURNING id",
    )
    .bind(name)
    .bind(team)
    .bind(&b.data)
    .bind(now_ms())
    .bind(u.id)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query("INSERT INTO workspace_members(workspace_id,user_id,role) VALUES($1,$2,'admin')").bind(id).bind(u.id).execute(&mut *tx).await?;
    let row: WsRow = sqlx::query_as(&format!("SELECT {} {WS_FROM} AND w.id=$2", ws_cols()))
        .bind(u.id)
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(ws_json(row)))
}

pub async fn ws_get(State(st): State<AppState>, u: AuthUser, Path(id): Path<String>) -> Res {
    let row: Option<(i64, String, Option<i64>, Option<String>, i64, i64, String, Value)> =
        sqlx::query_as(&format!("SELECT {},w.data {WS_FROM} AND w.id=$2", ws_cols()))
            .bind(u.id)
            .bind(pid(&id)?)
            .fetch_optional(&st.db)
            .await?;
    let (a, b, c, d, e, f, g, data) = row.ok_or_else(|| err(404, "Workspace not found"))?;
    let mut v = ws_json((a, b, c, d, e, f, g));
    v["data"] = data;
    Ok(Json(v))
}

pub async fn ws_version(State(st): State<AppState>, u: AuthUser, Path(id): Path<String>) -> Res {
    let (mut c, id) = (st.db.acquire().await?, pid(&id)?);
    need_ws(&mut c, id, u.id, "viewer").await?;
    let version: i64 = sqlx::query_scalar("SELECT version FROM workspaces WHERE id=$1").bind(id).fetch_one(&mut *c).await?;
    Ok(Json(json!({ "version": version })))
}

#[derive(Deserialize)]
pub struct WsPut {
    base_version: i64,
    data: Value,
}

pub async fn ws_put(State(st): State<AppState>, u: AuthUser, Path(id): Path<String>, Json(b): Json<WsPut>) -> Result<Response, ApiError> {
    let (mut c, id) = (st.db.acquire().await?, pid(&id)?);
    need_ws(&mut c, id, u.id, "editor").await?;
    // Compare-and-swap in one statement so concurrent writers can't both win.
    let new: Option<i64> = sqlx::query_scalar(
        "UPDATE workspaces SET data=$1, version=version+1, updated_at=$2, updated_by=$3 WHERE id=$4 AND version=$5 RETURNING version",
    )
    .bind(&b.data)
    .bind(now_ms())
    .bind(u.id)
    .bind(id)
    .bind(b.base_version)
    .fetch_optional(&mut *c)
    .await?;
    if let Some(version) = new {
        return Ok(Json(json!({ "version": version })).into_response());
    }
    let current: Option<i64> = sqlx::query_scalar("SELECT version FROM workspaces WHERE id=$1").bind(id).fetch_optional(&mut *c).await?;
    let version = current.ok_or_else(|| err(404, "Workspace not found"))?;
    Ok((StatusCode::CONFLICT, Json(json!({ "error": "Workspace changed on the server", "version": version }))).into_response())
}

pub async fn ws_rename(State(st): State<AppState>, u: AuthUser, Path(id): Path<String>, Json(b): Json<NameBody>) -> NoContent {
    let (mut c, id) = (st.db.acquire().await?, pid(&id)?);
    need_ws(&mut c, id, u.id, "admin").await?;
    sqlx::query("UPDATE workspaces SET name=$1 WHERE id=$2").bind(clean_name(&b.name)?).bind(id).execute(&mut *c).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn ws_delete(State(st): State<AppState>, u: AuthUser, Path(id): Path<String>) -> NoContent {
    let (mut c, id) = (st.db.acquire().await?, pid(&id)?);
    need_ws(&mut c, id, u.id, "admin").await?;
    sqlx::query("DELETE FROM workspaces WHERE id=$1").bind(id).execute(&mut *c).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn ws_members(State(st): State<AppState>, u: AuthUser, Path(id): Path<String>) -> Res {
    let (mut c, id) = (st.db.acquire().await?, pid(&id)?);
    need_ws(&mut c, id, u.id, "viewer").await?;
    WS.list(&mut c, id).await
}

pub async fn ws_add(State(st): State<AppState>, u: AuthUser, Path(id): Path<String>, Json(b): Json<MemberBody>) -> Res {
    let (mut tx, id) = (st.db.begin().await?, pid(&id)?);
    need_ws(&mut tx, id, u.id, "admin").await?;
    let r = WS.add(&mut tx, id, &b).await?;
    tx.commit().await?;
    Ok(r)
}

pub async fn ws_patch(
    State(st): State<AppState>,
    u: AuthUser,
    Path((id, uid)): Path<(String, String)>,
    Json(b): Json<RoleBody>,
) -> NoContent {
    let (mut tx, id, uid) = (st.db.begin().await?, pid(&id)?, pid(&uid)?);
    need_ws(&mut tx, id, u.id, "admin").await?;
    WS.patch(&mut tx, id, uid, &b.role).await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn ws_remove(State(st): State<AppState>, u: AuthUser, Path((id, uid)): Path<(String, String)>) -> NoContent {
    let (mut tx, id, uid) = (st.db.begin().await?, pid(&id)?, pid(&uid)?);
    need_ws(&mut tx, id, u.id, "admin").await?;
    WS.remove(&mut tx, id, uid).await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn ws_invite_delete(State(st): State<AppState>, u: AuthUser, Path((id, email)): Path<(String, String)>) -> NoContent {
    let (mut c, id) = (st.db.acquire().await?, pid(&id)?);
    need_ws(&mut c, id, u.id, "admin").await?;
    WS.remove_invite(&mut c, id, &email).await
}
