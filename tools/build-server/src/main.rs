use axum::{
    extract::{Path, Query},
    response::IntoResponse,
    routing::{get, post},
    Router,
};
use once_cell::sync::Lazy;
use serde::Deserialize;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Stdio;
use tokio::process::Command;
use tower_http::services::ServeFile;
use tracing::{error, info, instrument};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

static CONFIG: Lazy<Config> = Lazy::new(|| {
    let builder = config::Config::builder()
        .add_source(config::File::with_name("config").required(false))
        .add_source(config::Environment::with_prefix("MCP_SERVER"));
    builder.build().unwrap().try_deserialize().unwrap()
});

#[derive(Debug, Deserialize)]
struct Config {
    project_path: String,
    target_arch: String,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "mcp_server=debug,tower_http=debug".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let app = Router::new()
        .route("/", get(root))
        .route("/build", post(build_handler))
        .route("/download/:binary_name", get(download_handler));

    let addr = SocketAddr::from(([127, 0, 0, 1], 3000));
    info!("listening on {}", addr);
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

async fn root() -> &'static str {
    "Hello, World!"
}

#[derive(Debug, Deserialize)]
struct BuildParams {
    pull: Option<bool>,
}

#[instrument(skip(params))]
async fn build_handler(params: Query<BuildParams>) {
    let pull = params.pull.unwrap_or(false);
    info!("Build triggered with pull={}", pull);

    tokio::spawn(async move {
        if let Err(e) = run_build_process(pull).await {
            error!("Build process failed: {}", e);
        }
    });
}

#[instrument]
async fn download_handler(Path(binary_name): Path<String>) -> impl IntoResponse {
    let mut path = PathBuf::from(&CONFIG.project_path);
    path.push("target");
    path.push(&CONFIG.target_arch);
    path.push("release");
    path.push(binary_name);

    if !path.exists() {
        info!("File not found: {:?}", path);
        return Err(axum::http::StatusCode::NOT_FOUND);
    }

    Ok(ServeFile::new(path))
}

#[instrument]
async fn run_build_process(pull: bool) -> anyhow::Result<()> {
    info!("Checking for unstaged changes...");
    let git_status = Command::new("git")
        .arg("-C")
        .arg(&CONFIG.project_path)
        .arg("status")
        .arg("--porcelain")
        .output()
        .await?;

    if !git_status.stdout.is_empty() {
        let stdout = String::from_utf8_lossy(&git_status.stdout);
        error!("Unstaged changes detected:\n{}", stdout);
        anyhow::bail!("Unstaged changes detected. Aborting build.");
    }
    info!("No unstaged changes detected.");

    if pull {
        info!("Running git pull...");
        let git_pull = Command::new("git")
            .arg("-C")
            .arg(&CONFIG.project_path)
            .arg("pull")
            .status()
            .await?;
        if !git_pull.success() {
            anyhow::bail!("git pull failed");
        }
        info!("git pull successful.");
    }

    info!("Running cargo build...");
    let cargo_build = Command::new("cargo")
        .arg("build")
        .arg("--release")
        .arg("--target")
        .arg(&CONFIG.target_arch)
        .current_dir(&CONFIG.project_path)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await?;

    if !cargo_build.success() {
        anyhow::bail!("cargo build failed");
    }

    info!("Finished build process.");
    Ok(())
}