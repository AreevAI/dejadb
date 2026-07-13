use super::grain::{Grain, GrainCommon, GrainType};

/// A State grain — agent state snapshot (portable save point).
#[derive(Debug, Clone)]
pub struct State {
    pub context_data: serde_json::Value,
    pub common: GrainCommon,
}

impl State {
    pub fn new(context: serde_json::Value) -> Self {
        State {
            context_data: context,
            common: GrainCommon {
                confidence: 1.0,
                ..Default::default()
            },
        }
    }
}

impl Grain for State {
    fn grain_type(&self) -> GrainType {
        GrainType::State
    }

    fn common(&self) -> &GrainCommon {
        &self.common
    }

    fn common_mut(&mut self) -> &mut GrainCommon {
        &mut self.common
    }

    fn text(&self) -> String {
        if let Some(obj) = self.context_data.as_object() {
            for key in &["label", "description", "title", "name"] {
                if let Some(v) = obj.get(*key).and_then(|v| v.as_str()) {
                    if !v.trim().is_empty() {
                        return v.to_string();
                    }
                }
            }
        }
        String::new()
    }
}
