use axum::{
    extract::State,
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};

use crate::state::AppState;

#[derive(Deserialize)]
struct SkillSourceRequest {
    source: String,
}

#[derive(Deserialize)]
struct SkillInstallRequest {
    source: String,
    #[serde(default)]
    allow_high_risk: bool,
}

#[derive(Deserialize)]
struct SkillFrontmatter {
    name: String,
    description: String,
    #[serde(default)]
    permissions: Option<serde_yaml::Value>,
}

#[derive(Serialize)]
struct AnalyzeFindingResponse {
    severity: String,
    file: String,
    line: usize,
    pattern: String,
    reason: String,
}

#[derive(Serialize)]
struct AnalyzeSkillResponse {
    skill_name: String,
    description: String,
    findings: Vec<AnalyzeFindingResponse>,
    has_high_risk: bool,
    rendered_report: String,
}

#[derive(Serialize)]
struct InstallSkillResponse {
    skill_name: String,
    target_path: String,
    findings_count: usize,
    high_risk: bool,
}

#[derive(Serialize)]
struct InstalledSkillSummary {
    name: String,
    description: String,
    has_permissions: bool,
    path: String,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list_skills))
        .route("/analyze", post(analyze_skill))
        .route("/install", post(install_skill))
}

async fn analyze_skill(
    State(_state): State<AppState>,
    Json(body): Json<SkillSourceRequest>,
) -> Result<Json<AnalyzeSkillResponse>, StatusCode> {
    let resolved = clawhive_core::skill_install::resolve_skill_source(&body.source)
        .await
        .map_err(map_resolve_error)?;
    let report = clawhive_core::skill_install::analyze_skill_source(resolved.local_path())
        .map_err(map_analysis_error)?;

    let findings = report
        .findings
        .iter()
        .map(|finding| AnalyzeFindingResponse {
            severity: finding.severity.to_string(),
            file: finding.file.display().to_string(),
            line: finding.line,
            pattern: finding.pattern.to_string(),
            reason: finding.reason.to_string(),
        })
        .collect::<Vec<_>>();

    Ok(Json(AnalyzeSkillResponse {
        skill_name: report.skill_name.clone(),
        description: report.description.clone(),
        findings,
        has_high_risk: clawhive_core::skill_install::has_high_risk_findings(&report),
        rendered_report: clawhive_core::skill_install::render_skill_analysis(&report),
    }))
}

async fn install_skill(
    State(state): State<AppState>,
    Json(body): Json<SkillInstallRequest>,
) -> Result<Json<InstallSkillResponse>, StatusCode> {
    let resolved = clawhive_core::skill_install::resolve_skill_source(&body.source)
        .await
        .map_err(map_resolve_error)?;
    let report = clawhive_core::skill_install::analyze_skill_source(resolved.local_path())
        .map_err(map_analysis_error)?;
    let install = clawhive_core::skill_install::install_skill_from_analysis(
        &state.root,
        &state.root.join("skills"),
        resolved.local_path(),
        &report,
        body.allow_high_risk,
    )
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(InstallSkillResponse {
        skill_name: report.skill_name,
        target_path: install.target.display().to_string(),
        findings_count: report.findings.len(),
        high_risk: install.high_risk,
    }))
}

async fn list_skills(
    State(state): State<AppState>,
) -> Result<Json<Vec<InstalledSkillSummary>>, StatusCode> {
    let skills_root = state.root.join("skills");
    if !skills_root.exists() {
        return Ok(Json(Vec::new()));
    }

    let mut skills = Vec::new();
    let entries = std::fs::read_dir(&skills_root).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    for entry in entries {
        let entry = entry.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let skill_md = path.join("SKILL.md");
        if !skill_md.exists() {
            continue;
        }

        let raw =
            std::fs::read_to_string(&skill_md).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        let frontmatter = parse_skill_frontmatter(&raw).map_err(|_| StatusCode::BAD_REQUEST)?;
        skills.push(InstalledSkillSummary {
            name: frontmatter.name,
            description: frontmatter.description,
            has_permissions: frontmatter.permissions.is_some(),
            path: path.display().to_string(),
        });
    }

    skills.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(Json(skills))
}

fn map_resolve_error(err: anyhow::Error) -> StatusCode {
    let msg = err.to_string().to_lowercase();
    if msg.contains("does not exist") || msg.contains("http 404") || msg.contains("not found") {
        StatusCode::NOT_FOUND
    } else {
        StatusCode::BAD_REQUEST
    }
}

fn map_analysis_error(err: anyhow::Error) -> StatusCode {
    let msg = err.to_string().to_lowercase();
    if msg.contains("invalid frontmatter")
        || msg.contains("must start with yaml frontmatter")
        || msg.contains("no closing --- for frontmatter")
        || msg.contains("missing skill.md")
    {
        StatusCode::BAD_REQUEST
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    }
}

fn parse_skill_frontmatter(raw: &str) -> anyhow::Result<SkillFrontmatter> {
    let trimmed = raw.trim_start();
    if !trimmed.starts_with("---") {
        anyhow::bail!("SKILL.md must start with YAML frontmatter (---)");
    }

    let after_first = &trimmed[3..];
    let end = after_first
        .find("---")
        .ok_or_else(|| anyhow::anyhow!("no closing --- for frontmatter"))?;
    let yaml_str = &after_first[..end];
    serde_yaml::from_str(yaml_str).map_err(|e| anyhow::anyhow!("invalid frontmatter: {e}"))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::{
        body::Body,
        http::{Request, StatusCode},
        Router,
    };
    use tower::util::ServiceExt;

    use crate::state::AppState;

    fn setup_test_root() -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "clawhive-server-skills-test-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(root.join("skills")).expect("create skills dir");
        root
    }

    fn setup_test_app(root: std::path::PathBuf) -> Router {
        let state = AppState {
            root,
            bus: Arc::new(clawhive_bus::EventBus::new(16)),
            gateway: None,
            web_password_hash: Arc::new(std::sync::RwLock::new(None)),
            session_store: Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
            pending_openai_oauth: Arc::new(
                std::sync::RwLock::new(std::collections::HashMap::new()),
            ),
            openai_oauth_config: crate::state::default_openai_oauth_config(),
            enable_openai_oauth_callback_listener: true,
            daemon_mode: false,
            port: 3000,
            webhook_config: Arc::new(std::sync::RwLock::new(None)),
            routing_config: Arc::new(std::sync::RwLock::new(None)),
        };
        Router::new()
            .nest("/api/skills", super::router())
            .with_state(state)
    }

    #[tokio::test]
    async fn analyze_returns_not_found_for_missing_source() {
        let root = setup_test_root();
        let app = setup_test_app(root);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/skills/analyze")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"source":"/definitely/missing/path"}"#))
                    .expect("build request"),
            )
            .await
            .expect("send request");

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn install_returns_success_for_valid_skill_source() {
        let root = setup_test_root();
        let src = root.join("source-skill");
        std::fs::create_dir_all(&src).expect("create source dir");
        std::fs::write(
            src.join("SKILL.md"),
            "---\nname: hello-skill\ndescription: test\n---\n\nBody",
        )
        .expect("write skill");

        let app = setup_test_app(root.clone());
        let body = serde_json::json!({
            "source": src,
            "allow_high_risk": false,
        });

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/skills/install")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .expect("build request"),
            )
            .await
            .expect("send request");

        assert_eq!(response.status(), StatusCode::OK);
        assert!(root.join("skills/hello-skill/SKILL.md").exists());
    }

    #[tokio::test]
    async fn list_skills_returns_bad_request_for_invalid_frontmatter() {
        let root = setup_test_root();
        let skill_dir = root.join("skills/broken");
        std::fs::create_dir_all(&skill_dir).expect("create skill dir");
        std::fs::write(skill_dir.join("SKILL.md"), "name: broken").expect("write broken skill");

        let app = setup_test_app(root);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/skills")
                    .body(Body::empty())
                    .expect("build request"),
            )
            .await
            .expect("send request");

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
}
