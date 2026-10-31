use super::*;

fn append_page(result: &Value, models: &mut Vec<Value>, default: &mut Value) -> Result<(), Error> {
    let rows = result["data"].as_array().ok_or_else(Error::invalid)?;
    for row in rows.iter().filter(|row| row["hidden"] != true) {
        let id = string(row, "model")?;
        let efforts = row["supportedReasoningEfforts"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|e| json!({"id":e["reasoningEffort"],"label":e["reasoningEffort"]}))
            .collect();
        let model = super::super::models::model(
            id,
            string(row, "displayName")?,
            efforts,
            row["defaultReasoningEffort"].as_str(),
        );
        if row["isDefault"] == true {
            *default = json!({"modelId":id});
            if model["defaultReasoningEffort"].is_string() {
                default["reasoningEffort"] = model["defaultReasoningEffort"].clone();
            }
        }
        models.push(model);
    }
    if models.len() > 512 {
        return Err(Error::invalid());
    }
    Ok(())
}
impl Codex {
    pub(super) async fn remember_model(&self, result: &Value) {
        if let (Some(id), Some(model)) = (result["thread"]["id"].as_str(), result["model"].as_str())
        {
            let mut current = json!({"modelId":model});
            if result["reasoningEffort"].is_string() {
                current["reasoningEffort"] = result["reasoningEffort"].clone();
            }
            self.selections.lock().await.insert(id.into(), current);
        }
    }
    pub(super) async fn remember_turn_model(&self, id: &str, params: &Value) {
        if params["model"].is_string() {
            let mut selected = json!({"modelId":params["model"]});
            if params["effort"].is_string() {
                selected["reasoningEffort"] = params["effort"].clone();
            }
            self.selections.lock().await.insert(id.into(), selected);
        }
    }
    pub(super) async fn turn_params(
        &self,
        bridge: &Bridge,
        args: &Value,
        input: Value,
    ) -> Result<Value, Error> {
        let mut params = json!({"threadId":args["sessionId"],"input":input});
        if let Some(summary) = self
            .config
            .codex
            .as_ref()
            .and_then(|config| config.reasoning_summary.as_ref())
        {
            params["summary"] = json!(summary);
        }
        if args.get("selection").is_none() {
            return Ok(params);
        }
        let catalog = self.models(bridge, args).await?;
        if let Some(selected) = super::super::models::selection(args, &catalog)? {
            params["model"] = selected["modelId"].clone();
            let default = catalog["models"]
                .as_array()
                .and_then(|rows| rows.iter().find(|row| row["id"] == selected["modelId"]))
                .map(|row| row["defaultReasoningEffort"].clone())
                .unwrap_or(Value::Null);
            params["effort"] = selected.get("reasoningEffort").cloned().unwrap_or(default);
        }
        Ok(params)
    }
    pub(super) async fn models(&self, bridge: &Bridge, args: &Value) -> Result<Value, Error> {
        if let Some(id) = args["sessionId"].as_str() {
            self.summary(bridge, id).await?;
        }
        let mut models = Vec::new();
        let mut cursor = Value::Null;
        let mut default = Value::Null;
        for _ in 0..16 {
            let result = bridge
                .call("model/list", json!({"cursor":cursor,"limit":100}))
                .await?;
            append_page(&result, &mut models, &mut default)?;
            cursor = result["nextCursor"].clone();
            if cursor.is_null() {
                let current = match args["sessionId"].as_str() {
                    Some(id) => self.selections.lock().await.get(id).cloned(),
                    None => None,
                };
                return Ok(json!({"models":models,"default":default,"current":current}));
            }
        }
        Err(Error::invalid())
    }
}
