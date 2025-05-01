use octocrab::Octocrab;
use std::collections::HashMap;
use std::env;
use serde::{Deserialize};
use serde_json::json;
use serde_json::Value;
use base64::{Engine as _, engine::general_purpose};
use reqwest::Client;
use lambda_runtime::{service_fn, Error, LambdaEvent};

#[derive(Deserialize)]
struct WebhookPayload {
    action: String,
    pull_request: PullRequest,
    repository: Repository,
}

#[derive(Deserialize)]
struct PullRequest {
    number: u64,
    head: Branch,
}

#[derive(Deserialize)]
struct Branch {
    sha: String,
    #[serde(rename = "ref")]
    ref_name: String,
}

#[derive(Deserialize)]
struct Repository {
    full_name: String,
}

#[derive(Deserialize)]
struct ClaudeResponse {
    content: Vec<ClaudeContent>,
}

#[derive(Deserialize)]
struct ClaudeContent {
    text: String,
}

async fn analyze_and_enhance_pr(
    octocrab: &Octocrab,
    http_client: &Client,
    anthropic_api_key: &str,
    webhook_payload: WebhookPayload,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let pr_number = webhook_payload.pull_request.number;
    let repo_full_name = webhook_payload.repository.full_name;
    let parts: Vec<&str> = repo_full_name.split('/').collect();
    let owner = parts[0];
    let repo = parts[1];

    // Get PR files
    let files = octocrab
        .pulls(owner, repo)
        .list_files(pr_number)
        .await?;

    let mut modified_files: HashMap<String, String> = HashMap::new();

    // Process each file
    for file in files {
        if file.filename.ends_with(".rs") {
            // Get file content
            let content = get_file_content(octocrab, owner, repo, &file.filename, &webhook_payload.pull_request.head.sha).await?;

            // Enhance with Claude direct API
            let enhanced_content = enhance_with_claude_api(http_client, anthropic_api_key, &content).await?;

            if content != enhanced_content {
                modified_files.insert(file.filename, enhanced_content);
            }
        }
    }

    if !modified_files.is_empty() {
        // Create a new branch and PR with enhanced logging
        let new_branch_name = format!("auto-logging-enhancement-pr-{}", pr_number);
        let base_sha = webhook_payload.pull_request.head.sha;

        // Create new branch
        create_branch(octocrab, owner, repo, &new_branch_name, &base_sha).await?;

        // Commit changes
        for (filename, content) in modified_files {
            commit_file(octocrab, owner, repo, &new_branch_name, &filename, &content).await?;
        }

        // Create PR
        create_enhanced_pr(octocrab, owner, repo, &new_branch_name, &webhook_payload.pull_request.head.ref_name, pr_number).await?;
    }

    Ok(())
}

async fn enhance_with_claude_api(
    http_client: &Client,
    api_key: &str,
    code: &str,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let prompt = format!(
        "You are a Rust code expert. Analyze the following code and add appropriate metrics and log messages using tracing for important operations, errors, and state changes. Add debug, info, warn, and error level logs where appropriate. Don't change any logic, just add logging. Return only the enhanced code without explanations.

```rust
{}
```",
        code
    );

    let request_body = json!({
        "model": "claude-3-sonnet-20240229",
        "max_tokens": 4000,
        "messages": [{
            "role": "user",
            "content": prompt
        }]
    });

    let response = http_client
        .post("https://api.anthropic.com/v1/messages")
        .header("anthropic-version", "2023-06-01")
        .header("x-api-key", api_key)
        .header("content-type", "application/json")
        .json(&request_body)
        .send()
        .await?;

    let claude_response: ClaudeResponse = response.json().await?;
    let enhanced_code = claude_response.content[0].text.clone();

    // Extract code from markdown if present
    let enhanced_code = if enhanced_code.contains("```rust") {
        enhanced_code
            .split("```rust")
            .nth(1)
            .and_then(|s| s.split("```").next())
            .unwrap_or(&enhanced_code)
            .trim()
            .to_string()
    } else {
        enhanced_code
    };

    Ok(enhanced_code)
}

async fn get_file_content(
    octocrab: &Octocrab,
    owner: &str,
    repo: &str,
    path: &str,
    sha: &str,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let content = octocrab
        .repos(owner, repo)
        .get_content()
        .path(path)
        .r#ref(sha)
        .send()
        .await?;

    // Decode base64 content
    let encoded_content = &content.items[0].content; // Option<String>
    let decoded = general_purpose::STANDARD.decode(
        encoded_content.as_ref().ok_or("Missing file content")?
    )?;
    Ok(String::from_utf8(decoded)?)
}

async fn create_branch(
    octocrab: &Octocrab,
    owner: &str,
    repo: &str,
    branch_name: &str,
    base_sha: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    octocrab
        .repos(owner, repo)
        .create_ref(&octocrab::params::repos::Reference::Branch(branch_name.to_string()), base_sha)
        .await?;

    Ok(())
}

async fn commit_file(
    octocrab: &Octocrab,
    owner: &str,
    repo: &str,
    branch: &str,
    path: &str,
    content: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let encoded_content = general_purpose::STANDARD.encode(content.as_bytes());

    // Get the current file to get its SHA
    let current_file = octocrab
        .repos(owner, repo)
        .get_content()
        .path(path)
        .r#ref(branch)
        .send()
        .await?;

    // Get the SHA from the first item in the list
    let file_sha = current_file.items
        .get(0)
        .map(|item| &item.sha)
        .ok_or("File not found or empty response")?;

    octocrab
        .repos(owner, repo)
        .update_file(
            path,
            "🤖 Add enhanced logging",
            &encoded_content,
            file_sha,
        )
        .branch(branch)
        .send()
        .await?;

    Ok(())
}

async fn create_enhanced_pr(
    octocrab: &Octocrab,
    owner: &str,
    repo: &str,
    head: &str,
    base: &str,
    original_pr_number: u64,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let title = format!("🤖 Enhanced Logging for PR #{}", original_pr_number);
    let body = format!(
        "This PR adds enhanced logging to the code in PR #{}.\n\nChanges include:\n- Added appropriate log levels (debug, info, warn, error)\n- Enhanced error handling with logging\n- Improved observability for key operations\n\nGenerated automatically by Claude via GitHub bot.",
        original_pr_number
    );

    octocrab
        .pulls(owner, repo)
        .create(title, head, base)
        .body(body)
        .send()
        .await?;

    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    tracing_subscriber::fmt::init();
    let func = service_fn(handler);
    lambda_runtime::run(func).await?;
    Ok(())
}

async fn handler(event: LambdaEvent<Value>) -> Result<Value, Error> {
    let payload: WebhookPayload = serde_json::from_value(event.payload)?;
    let github_token = env::var("GITHUB_TOKEN")?;
    let anthropic_api_key = env::var("ANTHROPIC_API_KEY")?;

    let octocrab = Octocrab::builder().personal_token(github_token).build()?;
    let http_client = Client::new();

    if payload.action == "opened" || payload.action == "synchronize" {
        analyze_and_enhance_pr(&octocrab, &http_client, &anthropic_api_key, payload).await?;
    }

    if let Ok(github_event_path) = env::var("GITHUB_EVENT_PATH") {
        // Running as GitHub Action
        let event_data = std::fs::read_to_string(github_event_path)?;
        let webhook_payload: WebhookPayload = serde_json::from_str(&event_data)?;

        if webhook_payload.action == "opened" || webhook_payload.action == "synchronize" {
            analyze_and_enhance_pr(&octocrab, &http_client, &anthropic_api_key, webhook_payload).await?;
        }
    }

    Ok(json!({ "statusCode": 200, "body": "OK" }))
}