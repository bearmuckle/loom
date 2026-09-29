use super::*;

pub struct DeterministicProvider {
    pub descriptor: ModelDescriptor,
    pub steps: Vec<Vec<ModelStreamEvent>>,
    pub cursor: usize,
}

impl DeterministicProvider {
    pub fn demo() -> Self {
        Self {
            descriptor: deterministic_descriptor(),
            steps: vec![
                vec![
                    ModelStreamEvent::TextDelta {
                        text: "I'll inspect the workspace before making changes.\n".to_owned(),
                    },
                    ModelStreamEvent::ToolCallDelta {
                        call: ToolCall {
                            id: ToolCallId::new(),
                            name: "list_files".to_owned(),
                            arguments: serde_json::json!({"path": "."}),
                        },
                    },
                    ModelStreamEvent::Completed {
                        reason: FinishReason::ToolCall,
                    },
                ],
                vec![
                    ModelStreamEvent::TextDelta {
                        text: "I found the workspace. I'll apply a focused change next.\n"
                            .to_owned(),
                    },
                    ModelStreamEvent::ToolCallDelta {
                        call: ToolCall {
                            id: ToolCallId::new(),
                            name: "apply_patch".to_owned(),
                            arguments: serde_json::json!({
                                "path": "loom-m1-demo.txt",
                                "old_text": "",
                                "new_text": "Loom M1 deterministic demo\n"
                            }),
                        },
                    },
                    ModelStreamEvent::Completed {
                        reason: FinishReason::ToolCall,
                    },
                ],
                vec![
                    ModelStreamEvent::TextDelta {
                        text: "The change is applied. I'll run validation now.\n".to_owned(),
                    },
                    ModelStreamEvent::ToolCallDelta {
                        call: ToolCall {
                            id: ToolCallId::new(),
                            name: "run_command".to_owned(),
                            arguments: serde_json::json!({
                                "command": "rustc",
                                "args": ["--version"]
                            }),
                        },
                    },
                    ModelStreamEvent::Completed {
                        reason: FinishReason::ToolCall,
                    },
                ],
                vec![
                    ModelStreamEvent::TextDelta {
                        text: "The task is complete: the workspace change was applied and validation ran successfully."
                            .to_owned(),
                    },
                    ModelStreamEvent::Usage {
                        usage: TokenUsage {
                            input_tokens: 240,
                            output_tokens: 52,
                            cached_input_tokens: 0,
                        },
                    },
                    ModelStreamEvent::Completed {
                        reason: FinishReason::Stop,
                    },
                ],
            ],
            cursor: 0,
        }
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn with_cursor(mut self, cursor: usize) -> Self {
        self.cursor = cursor.min(self.steps.len());
        self
    }
}

impl ModelProvider for DeterministicProvider {
    fn descriptor(&self) -> &ModelDescriptor {
        &self.descriptor
    }

    fn stream(
        &mut self,
        _request: &ModelRequest,
        cancel: &CancellationToken,
        sink: &mut dyn ModelStreamSink,
    ) -> Result<()> {
        let events = self.steps.get(self.cursor).cloned().unwrap_or_else(|| {
            vec![ModelStreamEvent::Completed {
                reason: FinishReason::Stop,
            }]
        });
        self.cursor = self.cursor.saturating_add(1);
        for event in events {
            cancel.check()?;
            if sink.emit(event)? == StreamFlow::Stop {
                break;
            }
        }
        Ok(())
    }

    fn reset(&mut self) {
        self.cursor = 0;
    }
}
