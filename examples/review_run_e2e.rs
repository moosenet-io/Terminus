//! Manual live end-to-end harness for the `review_run` tool.
//!
//! Not part of the automated test suite -- run explicitly against a live
//! review-daemon to observe a real dispatch:
//!
//!   REVIEW_DAEMON_TOKEN=<token> REVIEW_DAEMON_URL=http://127.0.0.1:8790 \
//!     cargo run --example review_run_e2e
use terminus_rs::registry::ToolRegistry;

#[tokio::main]
async fn main() {
    let mut registry = ToolRegistry::new();
    terminus_rs::review::register(&mut registry);

    let args = serde_json::json!({
        "structure": "single",
        "providers": ["agy"],
        "criteria": "Reply with exactly the single word PONG and nothing else.",
        "context": {"note": "review_run_e2e manual harness call"}
    });

    let result = registry.call("review_run", args).await;
    match result {
        Some(Ok(text)) => {
            println!("review_run OK:");
            let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
            println!("{}", serde_json::to_string_pretty(&parsed).unwrap());
        }
        Some(Err(e)) => println!("review_run ERROR: {e}"),
        None => println!("review_run not registered"),
    }
}
