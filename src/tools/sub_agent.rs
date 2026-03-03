use serde_json::Value;

pub fn definition() -> Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": "sub_agent",
            "description": "Launch a sub-agent to research a specific topic autonomously. The sub-agent runs its own agentic loop with read-only tools (read_file, glob, grep, list_directory, web_search, web_fetch) and returns its findings. Use this when a task requires deep exploration across many files, tracing code paths, or investigating complex modules. The sub-agent has its own context window so it won't clutter the main conversation. Only available in PLAN mode.",
            "parameters": {
                "type": "object",
                "properties": {
                    "task": {
                        "type": "string",
                        "description": "A detailed description of the research task for the sub-agent. Be specific about what to investigate and what kind of answer you expect."
                    },
                    "max_turns": {
                        "type": "number",
                        "description": "Maximum number of agentic loop iterations (default: 10, max: 20). Use higher values for complex exploration tasks."
                    }
                },
                "required": ["task"]
            }
        }
    })
}
