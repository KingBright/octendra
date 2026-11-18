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

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SemanticElement {
    pub ref_id: String,
    pub role: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// Pixel [left, top, right, bottom] in raw frame_size coordinates.
    pub bounds: [u32; 4],
    pub interactive: bool,
    pub enabled: bool,
    #[serde(default)]
    pub focused: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected: Option<bool>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Selector {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
}
impl Selector {
    pub fn valid(&self) -> bool {
        let ok = |value: &Option<String>| {
            value
                .as_ref()
                .is_none_or(|v| !v.trim().is_empty() && v.len() <= 256)
        };
        (self.app.is_some() || self.window.is_some() || self.instance.is_some())
            && ok(&self.app)
            && ok(&self.window)
            && ok(&self.instance)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Query {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default)]
    pub interactive_only: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_elements: Option<usize>,
}
impl Query {
    pub fn valid(&self) -> bool {
        self.text.as_ref().is_none_or(|s| s.len() <= 256)
            && self
                .role
                .as_ref()
                .is_none_or(|s| !s.trim().is_empty() && s.len() <= 128)
            && self
                .max_elements
                .is_none_or(|n| (1..=1000).contains(&n))
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Expect {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<Target>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value_equals: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_contains: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_equals: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub element_gone: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visual_changed: Option<bool>,
}
impl Expect {
    pub fn valid(&self) -> bool {
        let has_condition = self.value_equals.is_some()
            || self.text_contains.is_some()
            || self.checked_equals.is_some()
            || self.element_gone.is_some()
            || self.visual_changed.is_some();
        has_condition
            && self.target.as_ref().is_none_or(Target::valid)
            && self.value_equals.as_ref().is_none_or(|s| s.len() <= 8192)
            && self
                .text_contains
                .as_ref()
                .is_none_or(|s| !s.is_empty() && s.len() <= 1024)
    }
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
    TripleClick {
        target: Target,
    },
    SecondaryClick {
        target: Target,
    },
    LongPress {
        target: Target,
    },
    SetChecked {
        target: Target,
        checked: bool,
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
            Self::TripleClick { .. } => "triple_click",
            Self::SecondaryClick { .. } => "secondary_click",
            Self::LongPress { .. } => "long_press",
            Self::SetChecked { .. } => "set_checked",
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
            | Self::TripleClick { target }
            | Self::SecondaryClick { target }
            | Self::LongPress { target }
            | Self::SetChecked { target, .. } => target.valid(),
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
        let triple: Action =
            serde_json::from_value(json!({"type":"triple_click","target":{"ref":"n1"}})).unwrap();
        let checked: Action = serde_json::from_value(
            json!({"type":"set_checked","target":{"ref":"n2"},"checked":true}),
        )
        .unwrap();
        assert!(insert.valid() && fill.valid() && triple.valid() && checked.valid());
        assert_eq!(triple.name(), "triple_click");
        assert_eq!(checked.name(), "set_checked");
        assert_ne!(insert.name(), fill.name());
        assert!(
            serde_json::from_value::<Action>(
                json!({"type":"click","target":{"ref":"n1"},"command":"bad"})
            )
            .is_err()
        );
        assert!(
            serde_json::from_value::<Selector>(json!({"app":"Notes"}))
                .unwrap()
                .valid()
        );
        assert!(!Selector::default().valid());
        assert!(
            serde_json::from_value::<Query>(json!({"interactive_only":true,"max_elements":20}))
                .unwrap()
                .valid()
        );
        assert!(
            serde_json::from_value::<Expect>(json!({"checked_equals":true,"visual_changed":true}))
                .unwrap()
                .valid()
        );
        assert!(!Expect::default().valid());
    }
}
