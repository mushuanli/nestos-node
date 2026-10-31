use super::*;
impl Cursor {
    pub(super) fn codex(&mut self, record: &Value) -> Result<(), Error> {
        self.codex_context(record);
        let p = &record["payload"];
        if record["type"] != "event_msg"
            || p["type"] != "token_count"
            || !p["info"]["total_token_usage"].is_object()
        {
            return Ok(());
        }
        self.codex_usage(&p["info"])
    }
    fn codex_context(&mut self, record: &Value) {
        let p = &record["payload"];
        if record["type"] == "session_meta" {
            self.provider = text(&p["model_provider"]);
        }
        if record["type"] == "event_msg" && p["type"] == "thread_settings_applied" {
            self.provider =
                text(&p["thread_settings"]["model_provider_id"]).or_else(|| self.provider.clone());
        }
        if record["type"] == "turn_context" {
            self.turn = text(&p["turn_id"]);
            self.model = text(&p["model"]);
            self.provider = text(&p["model_provider_id"])
                .or_else(|| text(&p["model_provider"]))
                .or_else(|| self.provider.clone());
        }
        if record["type"] == "event_msg" && p["type"] == "task_started" {
            self.turn = text(&p["turn_id"]);
        }
    }
    fn codex_usage(&mut self, info: &Value) -> Result<(), Error> {
        self.context_window = super::super::metrics::context(
            &info["last_token_usage"]["total_tokens"],
            &info["model_context_window"],
        );
        let raw = &info["total_token_usage"];
        if !valid_counts(
            raw,
            &[
                "cached_input_tokens",
                "cache_write_input_tokens",
                "reasoning_output_tokens",
            ],
        ) {
            self.incomplete = true;
            return Ok(());
        }
        let cumulative = tokens(raw);
        let prior = self.cumulative.clone().unwrap_or_default();
        let delta = if reset(&cumulative, &prior) {
            self.incomplete = true;
            if !valid_counts(
                &info["last_token_usage"],
                &[
                    "cached_input_tokens",
                    "cache_write_input_tokens",
                    "reasoning_output_tokens",
                ],
            ) {
                return Ok(());
            }
            tokens(&info["last_token_usage"])
        } else {
            cumulative.subtract(&prior)
        };
        self.cumulative = Some(cumulative);
        if delta == Tokens::default() {
            return Ok(());
        }
        let id = json!([self.turn, self.provider, self.model]).to_string();
        self.put(id, delta, false)
    }
}
fn reset(current: &Tokens, prior: &Tokens) -> bool {
    current.input_tokens < prior.input_tokens
        || current.output_tokens < prior.output_tokens
        || current.cache_read_tokens < prior.cache_read_tokens
        || current.cache_write_tokens < prior.cache_write_tokens
        || current.reasoning_tokens < prior.reasoning_tokens
}
fn tokens(value: &Value) -> Tokens {
    let read = count(value, "cached_input_tokens");
    let write = count(value, "cache_write_input_tokens");
    Tokens {
        input_tokens: count(value, "input_tokens")
            .saturating_sub(read)
            .saturating_sub(write),
        output_tokens: count(value, "output_tokens"),
        cache_read_tokens: read,
        cache_write_tokens: write,
        reasoning_tokens: count(value, "reasoning_output_tokens"),
    }
}
