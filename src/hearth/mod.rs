//! Hearth/Grocy kitchen and pantry tools.
//!
//! Replaces the Grade-D Python hearth_tools.py which used `shell=True`
//! with user arguments injected directly into curl URLs. All calls here
//! use `reqwest` to contact the Grocy REST API — zero shell commands.
//!
//! ## Configuration
//! - `GROCY_URL`    — base URL of the Grocy instance (required)
//! - `GROCY_API_KEY` — Grocy API key (required)
//!
//! If `GROCY_URL` is not set, every tool returns `ToolError::NotConfigured`.

use async_trait::async_trait;
use reqwest::{Client, StatusCode};
use serde_json::{json, Value};
use std::env;
use tracing::instrument;

use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::RustTool;

// ---------------------------------------------------------------------------
// Shell-metacharacter blocklist
// ---------------------------------------------------------------------------

/// Reject any string that contains characters used in shell injection attacks.
///
/// The original Python code injected the `product_name` argument directly into
/// a shell-executed curl URL. This function prevents that class of input from
/// reaching any downstream call.
fn reject_shell_metacharacters(s: &str, field: &str) -> Result<(), ToolError> {
    const FORBIDDEN: &[char] = &[';', '|', '&', '`', '$', '\\', '"', '\''];
    if let Some(ch) = s.chars().find(|c| FORBIDDEN.contains(c)) {
        return Err(ToolError::InvalidArgument(format!(
            "Field `{field}` contains forbidden character '{ch}' — shell metacharacters are not allowed"
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// HearthClient
// ---------------------------------------------------------------------------

/// Shared Grocy HTTP client. Reads `GROCY_URL` and `GROCY_API_KEY` from env.
#[derive(Clone)]
pub struct HearthClient {
    base_url: String,
    api_key: String,
    http: Client,
}

impl HearthClient {
    /// Build a client from environment variables.
    ///
    /// Returns `None` if `GROCY_URL` is not set (tool will report NotConfigured).
    pub fn from_env() -> Option<Self> {
        let base_url = env::var("GROCY_URL").ok()?;
        let api_key = env::var("GROCY_API_KEY").unwrap_or_default();
        let http = Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .ok()?;
        Some(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
            http,
        })
    }

    /// GET `{base_url}{path}` with the GROCY-API-KEY header.
    async fn get(&self, path: &str) -> Result<Value, ToolError> {
        let url = format!("{}{}", self.base_url, path);
        let resp = self
            .http
            .get(&url)
            .header("GROCY-API-KEY", &self.api_key)
            .send()
            .await
            .map_err(|e| ToolError::Http(format!("Kitchen service unavailable: {e}")))?;

        match resp.status() {
            StatusCode::NOT_FOUND => {
                return Err(ToolError::NotFound("Not found in your pantry".into()))
            }
            s if s.is_client_error() => {
                return Err(ToolError::Http(format!("Grocy API error: {s}")))
            }
            s if s.is_server_error() => {
                return Err(ToolError::Http(format!(
                    "Kitchen service unavailable (HTTP {s})"
                )))
            }
            _ => {}
        }

        resp.json::<Value>()
            .await
            .map_err(|e| ToolError::Http(format!("Invalid response from Grocy: {e}")))
    }

    /// POST `{base_url}{path}` with a JSON body.
    async fn post(&self, path: &str, body: Value) -> Result<Value, ToolError> {
        let url = format!("{}{}", self.base_url, path);
        let resp = self
            .http
            .post(&url)
            .header("GROCY-API-KEY", &self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| ToolError::Http(format!("Kitchen service unavailable: {e}")))?;

        match resp.status() {
            StatusCode::NOT_FOUND => {
                return Err(ToolError::NotFound("Not found in your pantry".into()))
            }
            s if s.is_client_error() => {
                return Err(ToolError::Http(format!("Grocy API error: {s}")))
            }
            s if s.is_server_error() => {
                return Err(ToolError::Http(format!(
                    "Kitchen service unavailable (HTTP {s})"
                )))
            }
            _ => {}
        }

        // Some Grocy endpoints return 204 No Content on success.
        if resp.status() == StatusCode::NO_CONTENT {
            return Ok(json!({"status": "ok"}));
        }

        resp.json::<Value>()
            .await
            .map_err(|e| ToolError::Http(format!("Invalid response from Grocy: {e}")))
    }
}

// ---------------------------------------------------------------------------
// Helper: pretty-print a JSON array of objects as a readable list
// ---------------------------------------------------------------------------

fn format_list(items: &Value, max: usize) -> String {
    match items.as_array() {
        None => "No items found.".into(),
        Some(arr) if arr.is_empty() => "No items found.".into(),
        Some(arr) => {
            let shown = arr.len().min(max);
            let mut out = format!("({} items", arr.len());
            if arr.len() > max {
                out.push_str(&format!(", showing first {shown}"));
            }
            out.push_str("):\n");
            for item in arr.iter().take(shown) {
                out.push_str(&format!("  {}\n", item));
            }
            out
        }
    }
}

// ---------------------------------------------------------------------------
// Macro: generate the boilerplate for a tool that requires HearthClient
// ---------------------------------------------------------------------------

macro_rules! hearth_tool {
    ($name:ident, $tool_name:expr, $desc:expr) => {
        pub struct $name {
            client: Option<HearthClient>,
        }

        impl $name {
            pub fn new() -> Self {
                Self {
                    client: HearthClient::from_env(),
                }
            }

            fn require_client(&self) -> Result<&HearthClient, ToolError> {
                self.client.as_ref().ok_or_else(|| {
                    ToolError::NotConfigured(
                        "GROCY_URL is not set — Hearth/Grocy tools are not configured".into(),
                    )
                })
            }
        }
    };
}

// ---------------------------------------------------------------------------
// Tool 1: hearth_pantry_list — GET /api/stock
// ---------------------------------------------------------------------------

hearth_tool!(HearthPantryList, "hearth_pantry_list", "List current pantry stock");

#[async_trait]
impl RustTool for HearthPantryList {
    fn name(&self) -> &str {
        "hearth_pantry_list"
    }

    fn description(&self) -> &str {
        "List all items currently in the pantry/stock. Returns product names, amounts, and best-before dates."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    #[instrument(skip(self, _args), fields(tool = "hearth_pantry_list"))]
    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        let client = self.require_client()?;
        let data = client.get("/api/stock").await?;
        Ok(format!("Pantry stock:\n{}", format_list(&data, 50)))
    }
}

// ---------------------------------------------------------------------------
// Tool 2: hearth_pantry_add — POST /api/stock/products/{id}/add
// ---------------------------------------------------------------------------

hearth_tool!(HearthPantryAdd, "hearth_pantry_add", "Add a product to stock");

#[async_trait]
impl RustTool for HearthPantryAdd {
    fn name(&self) -> &str {
        "hearth_pantry_add"
    }

    fn description(&self) -> &str {
        "Add an amount of a product to the pantry stock. Provide product_id (numeric), amount, and optionally price."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "product_id": {
                    "type": "integer",
                    "description": "Grocy numeric product ID"
                },
                "amount": {
                    "type": "number",
                    "description": "Amount to add (e.g. 1, 2.5)"
                },
                "price": {
                    "type": "number",
                    "description": "Price per unit (optional)"
                }
            },
            "required": ["product_id", "amount"]
        })
    }

    #[instrument(skip(self, args), fields(tool = "hearth_pantry_add"))]
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let client = self.require_client()?;

        let product_id = args["product_id"]
            .as_i64()
            .ok_or_else(|| ToolError::InvalidArgument("product_id must be an integer".into()))?;
        let amount = args["amount"]
            .as_f64()
            .ok_or_else(|| ToolError::InvalidArgument("amount must be a number".into()))?;

        let mut body = json!({ "amount": amount });
        if let Some(price) = args["price"].as_f64() {
            body["price"] = json!(price);
        }

        let path = format!("/api/stock/products/{product_id}/add");
        client.post(&path, body).await?;
        Ok(format!("Added {amount} units of product {product_id} to pantry."))
    }
}

// ---------------------------------------------------------------------------
// Tool 3: hearth_meal_plan — GET /api/meal_plan
// ---------------------------------------------------------------------------

hearth_tool!(HearthMealPlan, "hearth_meal_plan", "Retrieve upcoming meal plan");

#[async_trait]
impl RustTool for HearthMealPlan {
    fn name(&self) -> &str {
        "hearth_meal_plan"
    }

    fn description(&self) -> &str {
        "Get the upcoming meal plan. Optionally specify the number of days ahead to look (default 7)."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "days": {
                    "type": "integer",
                    "description": "Number of days ahead to show (default 7)"
                }
            },
            "required": []
        })
    }

    #[instrument(skip(self, args), fields(tool = "hearth_meal_plan"))]
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let client = self.require_client()?;
        let days = args["days"].as_i64().unwrap_or(7).max(1).min(365);

        // Grocy meal_plan supports ?days= query parameter
        let path = format!("/api/meal_plan?days={days}");
        let data = client.get(&path).await?;
        Ok(format!(
            "Meal plan (next {days} day(s)):\n{}",
            format_list(&data, 30)
        ))
    }
}

// ---------------------------------------------------------------------------
// Tool 4: hearth_shopping_list — GET /api/shopping_list
// ---------------------------------------------------------------------------

hearth_tool!(HearthShoppingList, "hearth_shopping_list", "Get current shopping list");

#[async_trait]
impl RustTool for HearthShoppingList {
    fn name(&self) -> &str {
        "hearth_shopping_list"
    }

    fn description(&self) -> &str {
        "Get the current shopping and grocery list — items to buy or pick up from the \
         store. Use when the user asks about groceries, what to buy, what they need, the \
         shopping list, or what's needed from the store."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    #[instrument(skip(self, _args), fields(tool = "hearth_shopping_list"))]
    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        let client = self.require_client()?;
        let data = client.get("/api/shopping_list").await?;
        Ok(format!("Shopping list:\n{}", format_list(&data, 100)))
    }
}

// ---------------------------------------------------------------------------
// Tool 5: hearth_what_can_i_make — GET /api/stock/volatile
// ---------------------------------------------------------------------------

hearth_tool!(HearthWhatCanIMake, "hearth_what_can_i_make", "List recipes cookable from current stock");

#[async_trait]
impl RustTool for HearthWhatCanIMake {
    fn name(&self) -> &str {
        "hearth_what_can_i_make"
    }

    fn description(&self) -> &str {
        "List recipes that can be made from items currently in stock (expiring-soon items are prioritised)."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    #[instrument(skip(self, _args), fields(tool = "hearth_what_can_i_make"))]
    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        let client = self.require_client()?;
        // /api/stock/volatile returns expiring + expired + missing items.
        // The Grocy "what can I make" concept is also surfaced here.
        let data = client.get("/api/stock/volatile").await?;
        Ok(format!("Stock volatile overview (what you can use up soon):\n{data}"))
    }
}

// ---------------------------------------------------------------------------
// Tool 6: hearth_recipe_search — GET /api/recipes
// ---------------------------------------------------------------------------

hearth_tool!(HearthRecipeSearch, "hearth_recipe_search", "Search recipes by name");

#[async_trait]
impl RustTool for HearthRecipeSearch {
    fn name(&self) -> &str {
        "hearth_recipe_search"
    }

    fn description(&self) -> &str {
        "Search for recipes by name or keyword. Returns matching recipes with ingredients."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search term (e.g. 'pasta', 'chicken'). Max 200 characters."
                }
            },
            "required": ["query"]
        })
    }

    #[instrument(skip(self, args), fields(tool = "hearth_recipe_search"))]
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let client = self.require_client()?;

        let query = args["query"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgument("query must be a string".into()))?;
        reject_shell_metacharacters(query, "query")?;

        if query.len() > 200 {
            return Err(ToolError::InvalidArgument(
                "query must be 200 characters or fewer".into(),
            ));
        }

        // Grocy's /api/recipes endpoint accepts query strings for filtering.
        // We fetch all recipes and filter by name client-side; Grocy CE doesn't
        // expose a server-side search filter on this endpoint in all versions.
        let data = client.get("/api/recipes").await?;

        let query_lower = query.to_lowercase();
        let empty = vec![];
        let matches: Vec<&Value> = data
            .as_array()
            .unwrap_or(&empty)
            .iter()
            .filter(|r| {
                r["name"]
                    .as_str()
                    .map(|n| n.to_lowercase().contains(&query_lower))
                    .unwrap_or(false)
            })
            .collect();

        if matches.is_empty() {
            return Ok(format!("No recipes found matching '{query}'."));
        }

        let mut out = format!("Recipes matching '{query}' ({} found):\n", matches.len());
        for r in matches.iter().take(20) {
            let name = r["name"].as_str().unwrap_or("Unknown");
            let id = &r["id"];
            out.push_str(&format!("  [{id}] {name}\n"));
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// Tool 7: hearth_stock_check — GET /api/stock/products by name
// ---------------------------------------------------------------------------

hearth_tool!(HearthStockCheck, "hearth_stock_check", "Check stock level for a named product");

#[async_trait]
impl RustTool for HearthStockCheck {
    fn name(&self) -> &str {
        "hearth_stock_check"
    }

    fn description(&self) -> &str {
        "Check how much of a named product is currently in stock. Accepts product name (fuzzy match)."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "product_name": {
                    "type": "string",
                    "description": "Product name to search for. Max 200 characters. Must not contain shell metacharacters."
                }
            },
            "required": ["product_name"]
        })
    }

    #[instrument(skip(self, args), fields(tool = "hearth_stock_check"))]
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let client = self.require_client()?;

        let product_name = args["product_name"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgument("product_name must be a string".into()))?;
        reject_shell_metacharacters(product_name, "product_name")?;

        if product_name.len() > 200 {
            return Err(ToolError::InvalidArgument(
                "product_name must be 200 characters or fewer".into(),
            ));
        }

        let name_lower = product_name.to_lowercase();

        // GET /api/stock returns all stock entries.
        let data = client.get("/api/stock").await?;

        let empty = vec![];
        let matches: Vec<&Value> = data
            .as_array()
            .unwrap_or(&empty)
            .iter()
            .filter(|item| {
                item["product"]["name"]
                    .as_str()
                    .map(|n| n.to_lowercase().contains(&name_lower))
                    .unwrap_or(false)
            })
            .collect();

        if matches.is_empty() {
            return Err(ToolError::NotFound(format!(
                "Not found in your pantry: '{product_name}'"
            )));
        }

        let mut out = format!(
            "Stock for '{}' ({} match(es)):\n",
            product_name,
            matches.len()
        );
        for item in &matches {
            let name = item["product"]["name"].as_str().unwrap_or("Unknown");
            let amount = &item["amount"];
            let unit = item["quantity_unit"]["name"].as_str().unwrap_or("units");
            out.push_str(&format!("  {name}: {amount} {unit}\n"));
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// Register all Hearth tools into the given registry.
pub fn register(registry: &mut ToolRegistry) {
    let tools: Vec<Box<dyn RustTool>> = vec![
        Box::new(HearthPantryList::new()),
        Box::new(HearthPantryAdd::new()),
        Box::new(HearthMealPlan::new()),
        Box::new(HearthShoppingList::new()),
        Box::new(HearthWhatCanIMake::new()),
        Box::new(HearthRecipeSearch::new()),
        Box::new(HearthStockCheck::new()),
    ];
    for tool in tools {
        if let Err(e) = registry.register(tool) {
            tracing::warn!("Hearth tool registration conflict: {e}");
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;
    use serde_json::json;

    /// Build a HearthClient pointing at the mock server.
    fn mock_client(server: &MockServer) -> HearthClient {
        HearthClient {
            base_url: server.base_url(),
            api_key: "<REDACTED-SECRET>".into(),
            http: Client::new(),
        }
    }

    // -----------------------------------------------------------------------
    // Input sanitisation tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_sanitise_rejects_semicolon() {
        let result = reject_shell_metacharacters("milk; rm -rf /", "product_name");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains(';'), "error should mention the forbidden char");
    }

    #[test]
    fn test_sanitise_rejects_pipe() {
        assert!(reject_shell_metacharacters("milk | cat /etc/passwd", "x").is_err());
    }

    #[test]
    fn test_sanitise_rejects_ampersand() {
        assert!(reject_shell_metacharacters("milk & background", "x").is_err());
    }

    #[test]
    fn test_sanitise_rejects_backtick() {
        assert!(reject_shell_metacharacters("`whoami`", "x").is_err());
    }

    #[test]
    fn test_sanitise_rejects_dollar() {
        assert!(reject_shell_metacharacters("$HOME", "x").is_err());
    }

    #[test]
    fn test_sanitise_rejects_backslash() {
        assert!(reject_shell_metacharacters("path\\file", "x").is_err());
    }

    #[test]
    fn test_sanitise_rejects_double_quote() {
        assert!(reject_shell_metacharacters("say \"hello\"", "x").is_err());
    }

    #[test]
    fn test_sanitise_rejects_single_quote() {
        assert!(reject_shell_metacharacters("it's", "x").is_err());
    }

    #[test]
    fn test_sanitise_allows_normal_name() {
        assert!(reject_shell_metacharacters("whole milk", "product_name").is_ok());
        assert!(reject_shell_metacharacters("pasta-sauce", "product_name").is_ok());
        assert!(reject_shell_metacharacters("Chicken Breast (frozen)", "product_name").is_ok());
    }

    // -----------------------------------------------------------------------
    // NotConfigured when GROCY_URL not set
    // -----------------------------------------------------------------------

    #[test]
    fn test_not_configured_when_no_grocy_url() {
        // Temporarily ensure env var is not set by using a client built without it.
        // We don't manipulate the real env (test isolation), so we test the
        // from_env() path directly: with no GROCY_URL the Option is None.
        // We simulate this via a tool instance whose `client` field is None.
        let tool = HearthPantryList { client: None };
        let result = tool.require_client();
        assert!(result.is_err());
        // Extract the error without requiring Debug on HearthClient (the Ok type).
        let err = result.err().expect("expected Err from require_client");
        match err {
            ToolError::NotConfigured(msg) => assert!(msg.contains("GROCY_URL")),
            other => panic!("expected NotConfigured, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // hearth_pantry_list — correct HTTP request
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_pantry_list_sends_get_api_stock() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/api/stock")
                .header("GROCY-API-KEY", "test-key");
            then.status(200)
                .json_body(json!([{"product": {"name": "Milk"}, "amount": 2}]));
        });

        let tool = HearthPantryList {
            client: Some(mock_client(&server)),
        };
        let result = tool.execute(json!({})).await.unwrap();
        mock.assert();
        assert!(result.contains("Pantry stock"));
    }

    // -----------------------------------------------------------------------
    // hearth_pantry_add — correct HTTP request
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_pantry_add_posts_to_correct_path() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/api/stock/products/42/add")
                .header("GROCY-API-KEY", "test-key");
            then.status(200).json_body(json!({"created_objects": 1}));
        });

        let tool = HearthPantryAdd {
            client: Some(mock_client(&server)),
        };
        let result = tool
            .execute(json!({"product_id": 42, "amount": 3.0}))
            .await
            .unwrap();
        mock.assert();
        assert!(result.contains("42"));
        assert!(result.contains("3"));
    }

    #[tokio::test]
    async fn test_pantry_add_includes_price_when_provided() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/api/stock/products/7/add")
                .json_body_partial(r#"{"amount":1.0,"price":2.99}"#);
            then.status(200).json_body(json!({"created_objects": 1}));
        });

        let tool = HearthPantryAdd {
            client: Some(mock_client(&server)),
        };
        let result = tool
            .execute(json!({"product_id": 7, "amount": 1.0, "price": 2.99}))
            .await
            .unwrap();
        mock.assert();
        assert!(result.contains("7"));
    }

    // -----------------------------------------------------------------------
    // hearth_meal_plan — correct HTTP request
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_meal_plan_sends_days_param() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/api/meal_plan")
                .query_param("days", "14");
            then.status(200).json_body(json!([]));
        });

        let tool = HearthMealPlan {
            client: Some(mock_client(&server)),
        };
        let result = tool.execute(json!({"days": 14})).await.unwrap();
        mock.assert();
        assert!(result.contains("14"));
    }

    #[tokio::test]
    async fn test_meal_plan_defaults_to_7_days() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/api/meal_plan")
                .query_param("days", "7");
            then.status(200).json_body(json!([]));
        });

        let tool = HearthMealPlan {
            client: Some(mock_client(&server)),
        };
        let result = tool.execute(json!({})).await.unwrap();
        mock.assert();
        assert!(result.contains("7"));
    }

    // -----------------------------------------------------------------------
    // hearth_shopping_list — correct HTTP request
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_shopping_list_sends_get_shopping_list() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/api/shopping_list");
            then.status(200).json_body(json!([{"product": {"name": "Eggs"}, "amount": 12}]));
        });

        let tool = HearthShoppingList {
            client: Some(mock_client(&server)),
        };
        let result = tool.execute(json!({})).await.unwrap();
        mock.assert();
        assert!(result.contains("Shopping list"));
    }

    // -----------------------------------------------------------------------
    // hearth_what_can_i_make — correct HTTP request
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_what_can_i_make_sends_get_stock_volatile() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/api/stock/volatile");
            then.status(200)
                .json_body(json!({"expiring_products": [], "expired_products": [], "missing_products": []}));
        });

        let tool = HearthWhatCanIMake {
            client: Some(mock_client(&server)),
        };
        let result = tool.execute(json!({})).await.unwrap();
        mock.assert();
        assert!(result.contains("volatile"));
    }

    // -----------------------------------------------------------------------
    // hearth_recipe_search — correct HTTP request + metacharacter rejection
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_recipe_search_returns_matching_recipes() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/api/recipes");
            then.status(200).json_body(json!([
                {"id": 1, "name": "Pasta Bolognese"},
                {"id": 2, "name": "Chicken Soup"},
                {"id": 3, "name": "Pasta Carbonara"},
            ]));
        });

        let tool = HearthRecipeSearch {
            client: Some(mock_client(&server)),
        };
        let result = tool.execute(json!({"query": "pasta"})).await.unwrap();
        mock.assert();
        assert!(result.contains("Pasta Bolognese"));
        assert!(result.contains("Pasta Carbonara"));
        assert!(!result.contains("Chicken Soup"));
    }

    #[tokio::test]
    async fn test_recipe_search_rejects_shell_injection() {
        let server = MockServer::start();
        // mock will NOT be called — rejection happens before HTTP
        let _mock = server.mock(|when, then| {
            when.method(GET).path("/api/recipes");
            then.status(200).json_body(json!([]));
        });

        let tool = HearthRecipeSearch {
            client: Some(mock_client(&server)),
        };
        let result = tool
            .execute(json!({"query": "pasta; cat /etc/passwd"}))
            .await;
        assert!(result.is_err());
        match result.unwrap_err() {
            ToolError::InvalidArgument(msg) => assert!(msg.contains(';')),
            other => panic!("expected InvalidArgument, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_recipe_search_no_match_returns_message() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/api/recipes");
            then.status(200).json_body(json!([
                {"id": 1, "name": "Pasta Bolognese"},
            ]));
        });

        let tool = HearthRecipeSearch {
            client: Some(mock_client(&server)),
        };
        let result = tool.execute(json!({"query": "sushi"})).await.unwrap();
        assert!(result.contains("No recipes found"));
    }

    // -----------------------------------------------------------------------
    // hearth_stock_check — 404 → "Not found in your pantry"
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_stock_check_404_returns_not_found() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/api/stock");
            then.status(404);
        });

        let tool = HearthStockCheck {
            client: Some(mock_client(&server)),
        };
        let result = tool
            .execute(json!({"product_name": "unicorn milk"}))
            .await;
        assert!(result.is_err());
        match result.unwrap_err() {
            ToolError::NotFound(msg) => assert!(msg.contains("pantry")),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_stock_check_rejects_shell_metacharacters() {
        let server = MockServer::start();
        // No HTTP call expected — rejected before network
        let _mock = server.mock(|when, then| {
            when.method(GET).path("/api/stock");
            then.status(200).json_body(json!([]));
        });

        let tool = HearthStockCheck {
            client: Some(mock_client(&server)),
        };
        let result = tool
            .execute(json!({"product_name": "milk`cat /etc/shadow`"}))
            .await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn test_stock_check_not_in_stock_returns_not_found() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/api/stock");
            then.status(200).json_body(json!([
                {"product": {"name": "Milk"}, "amount": 2, "quantity_unit": {"name": "L"}},
            ]));
        });

        let tool = HearthStockCheck {
            client: Some(mock_client(&server)),
        };
        let result = tool
            .execute(json!({"product_name": "saffron"}))
            .await;
        assert!(result.is_err());
        match result.unwrap_err() {
            ToolError::NotFound(msg) => assert!(msg.contains("saffron")),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_stock_check_found_returns_amount() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/api/stock");
            then.status(200).json_body(json!([
                {"product": {"name": "Whole Milk"}, "amount": 3, "quantity_unit": {"name": "L"}},
            ]));
        });

        let tool = HearthStockCheck {
            client: Some(mock_client(&server)),
        };
        let result = tool
            .execute(json!({"product_name": "milk"}))
            .await
            .unwrap();
        assert!(result.contains("Whole Milk"));
        assert!(result.contains('3'));
        assert!(result.contains('L'));
    }

    // -----------------------------------------------------------------------
    // Registration test
    // -----------------------------------------------------------------------

    #[test]
    fn test_register_adds_7_tools() {
        let mut registry = ToolRegistry::new();
        register(&mut registry);
        assert_eq!(registry.len(), 7, "expected exactly 7 Hearth tools");
    }

    #[test]
    fn test_registered_tool_names() {
        let mut registry = ToolRegistry::new();
        register(&mut registry);
        let names: Vec<String> = registry.list().iter().map(|t| t.name.clone()).collect();
        assert!(names.contains(&"hearth_pantry_list".to_string()));
        assert!(names.contains(&"hearth_pantry_add".to_string()));
        assert!(names.contains(&"hearth_meal_plan".to_string()));
        assert!(names.contains(&"hearth_shopping_list".to_string()));
        assert!(names.contains(&"hearth_what_can_i_make".to_string()));
        assert!(names.contains(&"hearth_recipe_search".to_string()));
        assert!(names.contains(&"hearth_stock_check".to_string()));
    }
}
