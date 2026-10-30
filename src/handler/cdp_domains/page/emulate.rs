use crate::handler::McpHandler;
use crate::handler::cdp_domains::cdp_target::CdpTarget;
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};
use serde_json::{Value, json};

#[macros::mcp_tool(
    name = "emulate",
    description = "Emulates a screen and CSS media features for the tab, to test responsive layouts, dark mode or print styles. Viewport: 'width' and 'height' (CSS pixels), 'device_scale_factor' (devicePixelRatio) and 'mobile' (mobile viewport and touch, Chrome only). Media (Chrome only): 'color_scheme' (prefers-color-scheme), 'reduced_motion' (prefers-reduced-motion: reduce), 'forced_colors' (forced-colors: active) and 'media' ('print' or 'screen'); each call with media options replaces the previous media emulation. 'reset': true removes every emulation. Firefox emulates the viewport and device pixel ratio (WebDriver BiDi has no media or touch emulation yet): the other options fail there, naming them. Side effects: the page re-lays out (resize and media query listeners run); the emulation lasts until 'reset' or the tab closes. Returns: what the page now sees, measured in it: JSON {width, height, device_pixel_ratio, color_scheme, reduced_motion, forced_colors, print, touch}. Use this before a screenshot or a check of a layout. Alternatives: --window-size at launch for the window itself."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct EmulateTool {
    /// Chrome instance id from open_instance/list_instances. Omit for the default instance.
    pub instance_id: Option<String>,
    /// The Tab ID of the target tab. Omit to use the active tab.
    pub tab_id: Option<String>,
    /// Viewport width in CSS pixels (with 'height'). Constraints: 1 to 10000.
    pub width: Option<u32>,
    /// Viewport height in CSS pixels (with 'width'). Constraints: 1 to 10000.
    pub height: Option<u32>,
    /// devicePixelRatio. Constraints: 0.5 to 5.
    pub device_scale_factor: Option<f64>,
    /// Mobile viewport (meta viewport, overlay scrollbars) and touch events (Chrome only).
    pub mobile: Option<bool>,
    /// prefers-color-scheme (Chrome only).
    pub color_scheme: Option<ColorScheme>,
    /// prefers-reduced-motion: true = reduce, false = no-preference (Chrome only).
    pub reduced_motion: Option<bool>,
    /// forced-colors: true = active, false = none (Chrome only).
    pub forced_colors: Option<bool>,
    /// The CSS media type (Chrome only).
    pub media: Option<MediaType>,
    /// Remove every emulation of the tab. Not with other options.
    pub reset: Option<bool>,
}

/// `color_scheme`.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema,
)]
pub enum ColorScheme {
    /// Light.
    #[serde(rename = "light")]
    Light,
    /// Dark.
    #[serde(rename = "dark")]
    Dark,
}

/// `media`.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema,
)]
pub enum MediaType {
    /// On screen.
    #[serde(rename = "screen")]
    Screen,
    /// Printing.
    #[serde(rename = "print")]
    Print,
}

/// What the page sees after an emulation.
const EMULATED: &str = "({ width: innerWidth, height: innerHeight, device_pixel_ratio: devicePixelRatio, \
    color_scheme: matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light', \
    reduced_motion: matchMedia('(prefers-reduced-motion: reduce)').matches, \
    forced_colors: matchMedia('(forced-colors: active)').matches, \
    print: matchMedia('print').matches, touch: navigator.maxTouchPoints > 0 })";

impl EmulateTool {
    /// Whether a reset was asked for.
    pub(crate) fn resets(&self) -> bool {
        self.reset == Some(true)
    }

    /// Whether viewport options were given.
    pub(crate) fn sets_viewport(&self) -> bool {
        self.width.is_some() || self.device_scale_factor.is_some() || self.mobile.is_some()
    }

    /// The media options given (`media` and CDP media features).
    fn media_options(&self) -> Vec<&'static str> {
        [
            self.color_scheme.map(|_| "color_scheme"),
            self.reduced_motion.map(|_| "reduced_motion"),
            self.forced_colors.map(|_| "forced_colors"),
            self.media.map(|_| "media"),
        ]
        .into_iter()
        .flatten()
        .collect()
    }

    /// The options Firefox cannot emulate that were given.
    pub(crate) fn chrome_only(&self) -> Vec<&'static str> {
        let mut options = self.media_options();
        if self.mobile.is_some() {
            options.push("mobile");
        }
        options
    }

    /// Checks the arguments (before any browser work).
    pub(crate) fn check_arguments(&self) -> Result<(), CallToolError> {
        let fail = |message: String| Err(CallToolError::from_message(message));
        let anything =
            self.sets_viewport() || self.height.is_some() || !self.media_options().is_empty();
        if self.resets() {
            return if anything {
                fail("'reset' takes no other option".into())
            } else {
                Ok(())
            };
        }
        if !anything {
            return fail("emulate needs a viewport option, a media option or 'reset'".into());
        }
        if self.width.is_some() != self.height.is_some() {
            return fail("'width' and 'height' go together".into());
        }
        for side in [self.width, self.height].into_iter().flatten() {
            if !(1..=10_000).contains(&side) {
                return fail(format!("width and height must be 1 to 10000, got {side}"));
            }
        }
        match self.device_scale_factor {
            Some(f) if !(0.5..=5.0).contains(&f) => {
                fail(format!("device_scale_factor must be 0.5 to 5, got {f}"))
            }
            _ => Ok(()),
        }
    }

    /// The JavaScript that reads what the page sees (shared by every browser).
    pub(crate) fn measure() -> &'static str {
        EMULATED
    }

    /// The reply from what the page sees (shared by every browser).
    pub(crate) fn reply(&self, seen: &Value) -> CallToolResult {
        let first = if self.resets() {
            "Emulation reset; the page sees:".to_string()
        } else {
            "Emulating; the page sees:".to_string()
        };
        let body = serde_json::to_string_pretty(seen).unwrap_or_default();
        CallToolResult::text_content(vec![format!("{first}\n{body}").into()])
    }

    /// The CDP commands of the call, in order.
    pub(crate) fn cdp_commands(&self) -> Vec<(&'static str, Value)> {
        if self.resets() {
            return vec![
                ("Emulation.clearDeviceMetricsOverride", json!({})),
                (
                    "Emulation.setTouchEmulationEnabled",
                    json!({"enabled": false}),
                ),
                (
                    "Emulation.setEmulatedMedia",
                    json!({"media": "", "features": []}),
                ),
            ];
        }
        let mut commands = Vec::new();
        if self.sets_viewport() {
            let mobile = self.mobile.unwrap_or(false);
            commands.push((
                "Emulation.setDeviceMetricsOverride",
                json!({"width": self.width.unwrap_or(0), "height": self.height.unwrap_or(0),
                       "deviceScaleFactor": self.device_scale_factor.unwrap_or(0.0), "mobile": mobile}),
            ));
            commands.push((
                "Emulation.setTouchEmulationEnabled",
                json!({"enabled": mobile}),
            ));
        }
        if !self.media_options().is_empty() {
            commands.push(("Emulation.setEmulatedMedia", self.cdp_media()));
        }
        commands
    }

    fn cdp_media(&self) -> Value {
        let mut features = Vec::new();
        if let Some(scheme) = self.color_scheme {
            let value = if scheme == ColorScheme::Dark {
                "dark"
            } else {
                "light"
            };
            features.push(json!({"name": "prefers-color-scheme", "value": value}));
        }
        if let Some(reduce) = self.reduced_motion {
            let value = if reduce { "reduce" } else { "no-preference" };
            features.push(json!({"name": "prefers-reduced-motion", "value": value}));
        }
        if let Some(active) = self.forced_colors {
            let value = if active { "active" } else { "none" };
            features.push(json!({"name": "forced-colors", "value": value}));
        }
        let media = match self.media {
            Some(MediaType::Print) => "print",
            Some(MediaType::Screen) => "screen",
            None => "",
        };
        json!({"media": media, "features": features})
    }

    pub async fn handle(
        params: CallToolRequestParams,
        handler: &McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        let args: EmulateTool = crate::handler::parse_args(params)?;
        args.check_arguments()?;
        let target =
            crate::handler::resolve_target(handler, args.instance_id.clone(), args.tab_id.clone())
                .await?;
        apply(&target, &args).await?;
        let seen = crate::handler::snippet_value(&target, Self::measure()).await?;
        Ok(args.reply(&seen))
    }
}

async fn apply(target: &CdpTarget, args: &EmulateTool) -> Result<(), CallToolError> {
    for (method, params) in args.cdp_commands() {
        crate::handler::send_or(target, method, params, "CDP Error").await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(args: Value) -> EmulateTool {
        serde_json::from_value(args).unwrap()
    }

    /// ISSUE-9: options that make no sense are refused before any browser
    /// work.
    #[test]
    fn senseless_options_are_refused() {
        let error = |args: Value| tool(args).check_arguments().unwrap_err().to_string();
        assert!(error(json!({})).contains("needs a viewport option"));
        assert!(error(json!({"width": 400})).contains("go together"));
        assert!(error(json!({"width": 0, "height": 10})).contains("got 0"));
        assert!(error(json!({"device_scale_factor": 9.0})).contains("0.5 to 5"));
        assert!(error(json!({"reset": true, "color_scheme": "dark"})).contains("no other option"));
        assert!(
            tool(json!({"color_scheme": "dark"}))
                .check_arguments()
                .is_ok()
        );
    }

    /// ISSUE-9 (Chrome): a mobile viewport sets the metrics and touch; media
    /// options become one setEmulatedMedia with only the given features;
    /// reset clears all three.
    #[test]
    fn options_become_cdp_commands() {
        let phone = tool(
            json!({"width": 375, "height": 667, "device_scale_factor": 2.0, "mobile": true, "color_scheme": "dark", "media": "print"}),
        );
        let commands = phone.cdp_commands();
        let methods: Vec<&str> = commands.iter().map(|(m, _)| *m).collect();
        assert_eq!(
            methods,
            [
                "Emulation.setDeviceMetricsOverride",
                "Emulation.setTouchEmulationEnabled",
                "Emulation.setEmulatedMedia"
            ]
        );
        assert_eq!(
            commands[0].1,
            json!({"width": 375, "height": 667, "deviceScaleFactor": 2.0, "mobile": true})
        );
        assert_eq!(
            commands[2].1,
            json!({"media": "print", "features": [{"name": "prefers-color-scheme", "value": "dark"}]})
        );
        assert_eq!(tool(json!({"reset": true})).cdp_commands().len(), 3);
        assert_eq!(phone.chrome_only(), ["color_scheme", "media", "mobile"]);
    }
}
