//! Semantic UI intents shared by computer and phone adapters.
//! Coordinates are pixels in the returned observation image, never OS coordinates.
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Target {
    #[serde(rename = "ref", default, skip_serializing_if = "Option::is_none")]
    pub element: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub y: Option<f64>,
}
impl Target {
    pub fn valid(&self) -> bool {
        match (&self.element, self.x, self.y) {
            (Some(id), None, None) => !id.is_empty() && id.len() <= 256,
            (None, Some(x), Some(y)) => x.is_finite() && y.is_finite() && x >= 0.0 && y >= 0.0,
            _ => false,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Up,
    Down,
    Left,
    Right,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    Click {
        target: Target,
    },
    DoubleClick {
        target: Target,
    },
    SecondaryClick {
        target: Target,
    },
    LongPress {
        target: Target,
    },
    TypeText {
        text: String,
        #[serde(default)]
        target: Option<Target>,
    },
    Fill {
        text: String,
        #[serde(default)]
        target: Option<Target>,
    },
    Scroll {
        direction: Direction,
        #[serde(default)]
        target: Option<Target>,
    },
    Drag {
        from: Target,
        to: Target,
    },
    Key {
        key: String,
        #[serde(default)]
        modifiers: Vec<String>,
    },
    LaunchApp {
        app: String,
    },
    Pinch {
        center: Target,
        scale: f64,
    },
    NavigateBack {},
    GoHome {},
    AppSwitcher {},
}
impl Action {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Click { .. } => "click",
            Self::DoubleClick { .. } => "double_click",
            Self::SecondaryClick { .. } => "secondary_click",
            Self::LongPress { .. } => "long_press",
            Self::TypeText { .. } => "type_text",
            Self::Fill { .. } => "fill",
            Self::Scroll { .. } => "scroll",
            Self::Drag { .. } => "drag",
            Self::Key { .. } => "key",
            Self::LaunchApp { .. } => "launch_app",
            Self::Pinch { .. } => "pinch",
            Self::NavigateBack {} => "navigate_back",
            Self::GoHome {} => "go_home",
            Self::AppSwitcher {} => "app_switcher",
        }
    }
    pub fn valid(&self) -> bool {
        let optional = |t: &Option<Target>| t.as_ref().is_none_or(Target::valid);
        match self {
            Self::Click { target }
            | Self::DoubleClick { target }
            | Self::SecondaryClick { target }
            | Self::LongPress { target } => target.valid(),
            Self::TypeText { text, target } | Self::Fill { text, target } => {
                text.len() <= 8192 && optional(target)
            }
            Self::Scroll { target, .. } => optional(target),
            Self::Drag { from, to } => from.valid() && to.valid(),
            Self::Key { key, modifiers } => {
                !key.is_empty()
                    && key.len() <= 64
                    && modifiers.len() <= 4
                    && modifiers
                        .iter()
                        .all(|m| matches!(m.as_str(), "shift" | "ctrl" | "alt" | "meta"))
            }
            Self::LaunchApp { app } => !app.is_empty() && app.len() <= 256,
            Self::Pinch { center, scale } => center.valid() && scale.is_finite() && *scale > 0.0,
            Self::NavigateBack {} | Self::GoHome {} | Self::AppSwitcher {} => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn targets_are_unambiguous_and_actions_are_strict() {
        for target in [
            json!({}),
            json!({"x":1}),
            json!({"x":-1,"y":0}),
            json!({"ref":"n1","x":1,"y":2}),
        ] {
            assert!(!serde_json::from_value::<Target>(target).unwrap().valid());
        }
        let insert: Action =
            serde_json::from_value(json!({"type":"type_text","text":"中文🙂"})).unwrap();
        let fill: Action = serde_json::from_value(json!({"type":"fill","text":""})).unwrap();
        assert!(insert.valid() && fill.valid());
        assert_ne!(insert.name(), fill.name());
        assert!(
            serde_json::from_value::<Action>(
                json!({"type":"click","target":{"ref":"n1"},"command":"bad"})
            )
            .is_err()
        );
    }
}
