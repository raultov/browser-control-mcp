use crate::handler::McpHandler;
use crate::handler::cdp_domains::cdp_target::CdpTarget;
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};
use serde_json::{Value, json};

#[macros::mcp_tool(
    name = "emulate",
    description = "Emulates, in the tab, its viewport ('width' and 'height', device_scale_factor, 'mobile' with touch), CSS media (color_scheme, reduced_motion, forced_colors, 'media'), user_agent, locale, timezone, geolocation (the page is granted the permission to read it), cpu_throttling, cpu_performance_tier (navigator.cpuPerformance) and network_throttling. Each call adds to the earlier ones (options it does not give keep their value); 'reset' removes every emulation. Returns what the page now sees. Firefox has no media, touch or throttling emulation (WebDriver BiDi): 'mobile', the media options, cpu_throttling, cpu_performance_tier and network_throttling fail there."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct EmulateTool {
    /// Instance id; omit for the default instance.
    pub instance_id: Option<String>,
    /// Tab id; omit for the active tab.
    pub tab_id: Option<String>,
    /// Viewport width in CSS pixels (with 'height'), 1 to 10000.
    pub width: Option<u32>,
    /// Viewport height in CSS pixels (with 'width'), 1 to 10000.
    pub height: Option<u32>,
    /// devicePixelRatio, 0.5 to 5.
    pub device_scale_factor: Option<f64>,
    /// Mobile viewport and touch events (Chrome only).
    pub mobile: Option<bool>,
    /// prefers-color-scheme (Chrome only).
    pub color_scheme: Option<ColorScheme>,
    /// true = prefers-reduced-motion: reduce (Chrome only).
    pub reduced_motion: Option<bool>,
    /// true = forced-colors: active (Chrome only).
    pub forced_colors: Option<bool>,
    /// CSS media type (Chrome only).
    pub media: Option<MediaType>,
    /// navigator.userAgent and the User-Agent header.
    pub user_agent: Option<String>,
    /// Locale of Intl formatting (dates, numbers), e.g. 'fr-FR'; Firefox also changes navigator.language.
    pub locale: Option<String>,
    /// IANA time zone, e.g. 'Asia/Tokyo'.
    pub timezone: Option<String>,
    /// The position navigator.geolocation reports.
    pub geolocation: Option<Geolocation>,
    /// CPU slowdown factor, 1 to 20 (4 = 4x slower, 1 = no throttling; Chrome only).
    pub cpu_throttling: Option<f64>,
    /// The tier navigator.cpuPerformance reports ('low', 'mid', 'high', 'ultra', 'unknown', or 'none' to remove; Chrome only).
    pub cpu_performance_tier: Option<CpuPerformanceTier>,
    /// Network preset ('slow-3g', 'fast-3g', '4g', or 'none' to remove; Chrome only; use set_offline for offline).
    pub network_throttling: Option<NetworkThrottling>,
    /// Remove every emulation; takes no other option.
    pub reset: Option<bool>,
}

/// `geolocation`.
#[derive(
    Debug, Clone, Copy, PartialEq, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema,
)]
pub struct Geolocation {
    /// -90 to 90.
    pub latitude: f64,
    /// -180 to 180.
    pub longitude: f64,
    /// In metres. Default: 1.
    pub accuracy: Option<f64>,
}

impl Geolocation {
    /// Whether the position is on Earth, with a sensible accuracy.
    fn check(&self) -> Result<(), String> {
        let accuracy = self.accuracy.unwrap_or(1.0);
        if !(-90.0..=90.0).contains(&self.latitude) {
            return Err(format!("latitude must be -90 to 90, got {}", self.latitude));
        }
        if !(-180.0..=180.0).contains(&self.longitude) {
            return Err(format!(
                "longitude must be -180 to 180, got {}",
                self.longitude
            ));
        }
        if accuracy < 0.0 {
            return Err(format!("accuracy must be 0 or more metres, got {accuracy}"));
        }
        Ok(())
    }
}

/// The viewport, media and throttling a tab emulates, as every call so
/// far set them: Chrome replaces its device metrics and its media features
/// as a whole, so a call that gives some of them sends the others again.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct Emulation {
    width: Option<u32>,
    height: Option<u32>,
    device_scale_factor: Option<f64>,
    mobile: Option<bool>,
    color_scheme: Option<ColorScheme>,
    reduced_motion: Option<bool>,
    forced_colors: Option<bool>,
    media: Option<MediaType>,
    cpu_throttling: Option<f64>,
    cpu_performance_tier: Option<CpuPerformanceTier>,
    network_throttling: Option<NetworkThrottling>,
}

impl Emulation {
    /// This emulation with what `args` gives (nothing after a reset; `1`
    /// for `cpu_throttling` and `"none"` for `network_throttling` clear
    /// them).
    pub(crate) fn merged(&self, args: &EmulateTool) -> Self {
        if args.resets() {
            return Self::default();
        }
        Self {
            width: args.width.or(self.width),
            height: args.height.or(self.height),
            device_scale_factor: args.device_scale_factor.or(self.device_scale_factor),
            mobile: args.mobile.or(self.mobile),
            color_scheme: args.color_scheme.or(self.color_scheme),
            reduced_motion: args.reduced_motion.or(self.reduced_motion),
            forced_colors: args.forced_colors.or(self.forced_colors),
            media: args.media.or(self.media),
            cpu_throttling: args
                .cpu_throttling
                .or(self.cpu_throttling)
                .filter(|&rate| rate > 1.0),
            cpu_performance_tier: args
                .cpu_performance_tier
                .or(self.cpu_performance_tier)
                .filter(|&tier| tier != CpuPerformanceTier::None),
            network_throttling: args
                .network_throttling
                .or(self.network_throttling)
                .filter(|&net| net != NetworkThrottling::None),
        }
    }

    /// Adds the active CPU and network throttling and CPU performance tier
    /// (or the values `args` just cleared them to) to `seen`, as the page
    /// cannot read them (it reads the tier as a number).
    pub(crate) fn annotate(&self, args: &EmulateTool, seen: &mut Value) {
        if let Some(rate) = self.cpu_throttling.or(args.cpu_throttling) {
            seen["cpu_throttling"] = json!(rate);
        }
        if let Some(tier) = self.cpu_performance_tier.or(args.cpu_performance_tier) {
            seen["cpu_performance_tier"] = json!(tier);
        }
        if let Some(net) = self.network_throttling.or(args.network_throttling) {
            seen["network_throttling"] = json!(net.as_str());
        }
    }
}

/// `network_throttling` (Chrome DevTools presets; DevTools names such as
/// `'Slow 3G'`, `'Fast 3G'` and `'Fast 4G'` are accepted as aliases).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema,
)]
pub enum NetworkThrottling {
    /// Slow 3G (2000 ms latency, 400 kbps up/down).
    #[serde(rename = "slow-3g", alias = "Slow 3G")]
    Slow3g,
    /// Fast 3G / Slow 4G (562.5 ms latency, 1.44 Mbps down, 675 kbps up).
    #[serde(
        rename = "fast-3g",
        alias = "Fast 3G",
        alias = "slow-4g",
        alias = "Slow 4G"
    )]
    Fast3g,
    /// Fast 4G (165 ms latency, 8.1 Mbps down, 1.35 Mbps up).
    #[serde(rename = "4g", alias = "fast-4g", alias = "Fast 4G")]
    FourG,
    /// No network throttling.
    #[serde(rename = "none", alias = "No throttling")]
    None,
}

impl NetworkThrottling {
    /// Its canonical name in the reply.
    fn as_str(self) -> &'static str {
        match self {
            Self::Slow3g => "slow-3g",
            Self::Fast3g => "fast-3g",
            Self::FourG => "4g",
            Self::None => "none",
        }
    }

    /// Its `Network.emulateNetworkConditions` parameters (`latency` in ms,
    /// throughput in bytes/s).
    fn cdp_conditions(self) -> Value {
        let (latency, down, up) = match self {
            Self::Slow3g => (2000.0, 50_000.0, 50_000.0),
            Self::Fast3g => (562.5, 180_000.0, 84_375.0),
            Self::FourG => (165.0, 1_012_500.0, 168_750.0),
            Self::None => (0.0, -1.0, -1.0),
        };
        json!({
            "offline": false,
            "latency": latency,
            "downloadThroughput": down,
            "uploadThroughput": up,
        })
    }
}

/// `cpu_performance_tier` (Chrome's `Emulation.setCPUPerformanceOverride`;
/// the page reads 0 for `unknown`, 1 for `low` ... 4 for `ultra`, ISSUE-61).
/// Each variant is renamed on its own: the schema derive ignores
/// `rename_all` and would list the Rust names (ISSUE-82).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema,
)]
pub enum CpuPerformanceTier {
    /// Unknown (navigator.cpuPerformance 0).
    #[serde(rename = "unknown")]
    Unknown,
    /// Low (1).
    #[serde(rename = "low")]
    Low,
    /// Mid (2).
    #[serde(rename = "mid")]
    Mid,
    /// High (3).
    #[serde(rename = "high")]
    High,
    /// Ultra (4).
    #[serde(rename = "ultra")]
    Ultra,
    /// No override: the host's tier.
    #[serde(rename = "none")]
    None,
}

impl CpuPerformanceTier {
    /// Its `Emulation.setCPUPerformanceOverride` parameters (none removes
    /// the override).
    fn cdp_override(self) -> Value {
        match self {
            Self::None => json!({}),
            // Its serde name is CDP's (`low`, `mid`...).
            tier => json!({"performanceTier": tier}),
        }
    }
}

/// The CDP command a reset sends that older Chrome versions lack
/// (experimental): its failure does not fail the reset.
const BEST_EFFORT_ON_RESET: &str = "Emulation.setCPUPerformanceOverride";

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
/// What the reply says of the position on a page paused in the debugger.
const PAUSED_POSITION: &str =
    "not read: the page is paused in the debugger (set; it reads it once resumed)";

const EMULATED: &str = "({ width: innerWidth, height: innerHeight, device_pixel_ratio: devicePixelRatio, \
    color_scheme: matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light', \
    reduced_motion: matchMedia('(prefers-reduced-motion: reduce)').matches, \
    forced_colors: matchMedia('(forced-colors: active)').matches, \
    print: matchMedia('print').matches, touch: navigator.maxTouchPoints > 0, user_agent: navigator.userAgent, \
    locale: Intl.DateTimeFormat().resolvedOptions().locale, timezone: Intl.DateTimeFormat().resolvedOptions().timeZone, \
    ...('cpuPerformance' in navigator ? { cpu_performance: navigator.cpuPerformance } : {}) })";

/// `measure` (an expression, maybe a promise) once a resize has landed: the
/// next `resize` event or two animation frames, at most 500 ms (a tab in
/// the background runs no frames).
fn after_resize(measure: &str) -> String {
    format!(
        "(async () => {{ await new Promise(done => {{ addEventListener('resize', done, {{ once: true }}); \
         requestAnimationFrame(() => requestAnimationFrame(done)); setTimeout(done, 500); }}); \
         return await ({measure}); }})()"
    )
}

/// The position the page reads (`{latitude, longitude, accuracy}`, or
/// `{error}`), at most 3 s.
const GEOLOCATED: &str = "Promise.race([new Promise(resolve => navigator.geolocation.getCurrentPosition( \
    p => resolve({ latitude: p.coords.latitude, longitude: p.coords.longitude, accuracy: p.coords.accuracy }), \
    e => resolve({ error: e.message }), { maximumAge: 0, timeout: 2000 })), \
    new Promise(resolve => setTimeout(() => resolve({ error: 'no position within 3 s' }), 3000))])";

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

    /// The user agent, locale, time zone and geolocation options given.
    fn identity_options(&self) -> Vec<&'static str> {
        [
            self.user_agent.as_ref().map(|_| "user_agent"),
            self.locale.as_ref().map(|_| "locale"),
            self.timezone.as_ref().map(|_| "timezone"),
            self.geolocation.map(|_| "geolocation"),
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
        if self.cpu_throttling.is_some() {
            options.push("cpu_throttling");
        }
        if self.cpu_performance_tier.is_some() {
            options.push("cpu_performance_tier");
        }
        if self.network_throttling.is_some() {
            options.push("network_throttling");
        }
        options
    }

    /// Checks the arguments (before any browser work).
    pub(crate) fn check_arguments(&self) -> Result<(), CallToolError> {
        let fail = |message: String| Err(CallToolError::from_message(message));
        let anything = self.sets_viewport()
            || self.height.is_some()
            || !self.media_options().is_empty()
            || !self.identity_options().is_empty()
            || self.cpu_throttling.is_some()
            || self.cpu_performance_tier.is_some()
            || self.network_throttling.is_some();
        if self.resets() {
            return if anything {
                fail("'reset' takes no other option".into())
            } else {
                Ok(())
            };
        }
        if !anything {
            return fail("emulate needs a viewport, media, user_agent, locale, timezone, geolocation, cpu_throttling, cpu_performance_tier or network_throttling option, or 'reset'".into());
        }
        self.check_texts().map_err(CallToolError::from_message)?;
        if let Some(place) = self.geolocation {
            place.check().map_err(CallToolError::from_message)?;
        }
        if let Some(rate) = self.cpu_throttling
            && !(1.0..=20.0).contains(&rate)
        {
            return fail(format!("cpu_throttling must be 1 to 20, got {rate}"));
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

    /// An empty `user_agent`, `locale` or `timezone` is refused: `reset`
    /// removes an emulation.
    fn check_texts(&self) -> Result<(), String> {
        let texts = [
            ("user_agent", &self.user_agent),
            ("locale", &self.locale),
            ("timezone", &self.timezone),
        ];
        let empty = |text: &Option<String>| text.as_ref().is_some_and(|t| t.trim().is_empty());
        match texts.iter().find(|(_, text)| empty(text)) {
            Some((name, _)) => Err(format!("{name} is empty: 'reset' removes every emulation")),
            None => Ok(()),
        }
    }

    /// `browsingContext.setViewport` for `context` (WebDriver BiDi), when
    /// the call sets or resets the viewport: absent fields stay as they are.
    pub(crate) fn bidi_viewport(&self, context: &str) -> Option<Value> {
        if !self.resets() && !self.sets_viewport() {
            return None;
        }
        let mut viewport = json!({"context": context});
        if self.resets() {
            viewport["viewport"] = Value::Null;
            viewport["devicePixelRatio"] = Value::Null;
        }
        if let (Some(width), Some(height)) = (self.width, self.height) {
            viewport["viewport"] = json!({"width": width, "height": height});
        }
        if let Some(ratio) = self.device_scale_factor {
            viewport["devicePixelRatio"] = json!(ratio);
        }
        Some(viewport)
    }

    /// The JavaScript that checks `timezone` and `locale` with the page's
    /// Intl before anything is set: Chrome takes an unknown time zone
    /// without a word and keeps the old one. `null`, or the error.
    pub(crate) fn names_check(&self) -> Option<String> {
        if self.timezone.is_none() && self.locale.is_none() {
            return None;
        }
        let literal = |text: &Option<String>| {
            text.as_deref()
                .map_or_else(|| "null".to_string(), crate::util::js::string_literal)
        };
        let (zone, locale) = (literal(&self.timezone), literal(&self.locale));
        Some(format!(
            "(() => {{ try {{ const zone = {zone}, locale = {locale}; \
             if (zone !== null) new Intl.DateTimeFormat('en', {{ timeZone: zone }}); \
             if (locale !== null) Intl.getCanonicalLocales(locale); return null; }} \
             catch (e) {{ return e.message; }} }})()"
        ))
    }

    /// The error of a failed [`Self::names_check`], if it failed.
    pub(crate) fn names_error(checked: &Value) -> Result<(), CallToolError> {
        match checked.as_str() {
            Some(message) => Err(CallToolError::from_message(format!(
                "emulate: {message} (timezone takes an IANA name such as 'Asia/Tokyo', locale a tag such as 'fr-FR'); nothing was changed."
            ))),
            None => Ok(()),
        }
    }

    /// The JavaScript that reads what the page sees (shared by every
    /// browser), with the position it reads when the call set one.
    pub(crate) fn measure(&self) -> String {
        match self.geolocation {
            Some(_) => format!(
                "(async () => Object.assign({EMULATED}, {{ geolocation: await {GEOLOCATED} }}))()"
            ),
            None => EMULATED.to_string(),
        }
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

    /// The CDP commands of the call, in order, the viewport and media ones
    /// with `state`, what the tab emulates with this call merged in.
    pub(crate) fn cdp_commands(&self, state: &Emulation) -> Vec<(&'static str, Value)> {
        if self.resets() {
            return reset_commands();
        }
        let mut commands = Vec::new();
        if self.sets_viewport() {
            let mobile = state.mobile.unwrap_or(false);
            commands.push((
                "Emulation.setDeviceMetricsOverride",
                json!({"width": state.width.unwrap_or(0), "height": state.height.unwrap_or(0),
                       "deviceScaleFactor": state.device_scale_factor.unwrap_or(0.0), "mobile": mobile}),
            ));
            commands.push((
                "Emulation.setTouchEmulationEnabled",
                json!({"enabled": mobile}),
            ));
        }
        if !self.media_options().is_empty() {
            commands.push(("Emulation.setEmulatedMedia", cdp_media(state)));
        }
        commands.extend(self.cdp_identity());
        if let Some(rate) = self.cpu_throttling {
            commands.push(("Emulation.setCPUThrottlingRate", json!({"rate": rate})));
        }
        if let Some(tier) = self.cpu_performance_tier {
            let method = "Emulation.setCPUPerformanceOverride";
            commands.push((method, tier.cdp_override()));
        }
        if let Some(net) = self.network_throttling {
            commands.push(("Network.emulateNetworkConditions", net.cdp_conditions()));
        }
        commands
    }

    /// The user agent, locale, time zone and geolocation overrides given;
    /// a position also grants the permission to read it (the page is
    /// refused it otherwise).
    fn cdp_identity(&self) -> Vec<(&'static str, Value)> {
        let mut commands = Vec::new();
        if let Some(agent) = &self.user_agent {
            commands.push((
                "Emulation.setUserAgentOverride",
                json!({"userAgent": agent}),
            ));
        }
        if let Some(locale) = &self.locale {
            commands.push(("Emulation.setLocaleOverride", json!({"locale": locale})));
        }
        if let Some(zone) = &self.timezone {
            commands.push(("Emulation.setTimezoneOverride", json!({"timezoneId": zone})));
        }
        if let Some(place) = self.geolocation {
            let accuracy = place.accuracy.unwrap_or(1.0);
            let position = json!({"latitude": place.latitude, "longitude": place.longitude, "accuracy": accuracy});
            commands.push(("Emulation.setGeolocationOverride", position));
            commands.push((
                "Browser.grantPermissions",
                json!({"permissions": ["geolocation"]}),
            ));
        }
        commands
    }

    /// The WebDriver BiDi overrides of the call for `context` (user agent,
    /// locale, time zone, geolocation), all cleared on a reset.
    pub(crate) fn bidi_overrides(&self, context: &str) -> Vec<(&'static str, Value)> {
        let reset = self.resets();
        let place = self.geolocation.map(|place| {
            json!({"latitude": place.latitude, "longitude": place.longitude, "accuracy": place.accuracy.unwrap_or(1.0)})
        });
        let overrides = [
            (
                "emulation.setUserAgentOverride",
                "userAgent",
                self.user_agent.as_ref().map(|a| json!(a)),
            ),
            (
                "emulation.setLocaleOverride",
                "locale",
                self.locale.as_ref().map(|l| json!(l)),
            ),
            (
                "emulation.setTimezoneOverride",
                "timezone",
                self.timezone.as_ref().map(|z| json!(z)),
            ),
            ("emulation.setGeolocationOverride", "coordinates", place),
        ];
        overrides
            .into_iter()
            .filter(|(_, _, value)| reset || value.is_some())
            .map(|(method, key, value)| {
                (
                    method,
                    json!({key: value.unwrap_or(Value::Null), "contexts": [context]}),
                )
            })
            .collect()
    }
}

/// The CDP commands of a reset: every emulation removed.
fn reset_commands() -> Vec<(&'static str, Value)> {
    vec![
        ("Emulation.clearDeviceMetricsOverride", json!({})),
        (
            "Emulation.setTouchEmulationEnabled",
            json!({"enabled": false}),
        ),
        (
            "Emulation.setEmulatedMedia",
            json!({"media": "", "features": []}),
        ),
        ("Emulation.setUserAgentOverride", json!({"userAgent": ""})),
        ("Emulation.setLocaleOverride", json!({})),
        ("Emulation.setTimezoneOverride", json!({"timezoneId": ""})),
        ("Emulation.clearGeolocationOverride", json!({})),
        ("Browser.resetPermissions", json!({})),
        ("Emulation.setCPUThrottlingRate", json!({"rate": 1})),
        (BEST_EFFORT_ON_RESET, json!({})),
        (
            "Network.emulateNetworkConditions",
            NetworkThrottling::None.cdp_conditions(),
        ),
    ]
}

/// `Emulation.setEmulatedMedia` for the media `state` emulates.
fn cdp_media(state: &Emulation) -> Value {
    {
        let mut features = Vec::new();
        if let Some(scheme) = state.color_scheme {
            let value = if scheme == ColorScheme::Dark {
                "dark"
            } else {
                "light"
            };
            features.push(json!({"name": "prefers-color-scheme", "value": value}));
        }
        if let Some(reduce) = state.reduced_motion {
            let value = if reduce { "reduce" } else { "no-preference" };
            features.push(json!({"name": "prefers-reduced-motion", "value": value}));
        }
        if let Some(active) = state.forced_colors {
            let value = if active { "active" } else { "none" };
            features.push(json!({"name": "forced-colors", "value": value}));
        }
        let media = match state.media {
            Some(MediaType::Print) => "print",
            Some(MediaType::Screen) => "screen",
            None => "",
        };
        json!({"media": media, "features": features})
    }
}

impl EmulateTool {
    pub async fn handle(
        params: CallToolRequestParams,
        handler: &McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        let args: EmulateTool = crate::handler::parse_args(params)?;
        args.check_arguments()?;
        let session = handler.session(args.instance_id.clone()).await?;
        let target =
            crate::handler::resolve_target(handler, args.instance_id.clone(), args.tab_id.clone())
                .await?;
        if let Some(check) = args.names_check() {
            Self::names_error(&crate::handler::snippet_value(&target, &check).await?)?;
        }
        let state = session.emulation_state(args.tab_id.clone())?;
        let merged = state.lock().await.merged(&args);
        apply(&target, &args.cdp_commands(&merged), args.resets()).await?;
        // Kept once Chrome took it all.
        *state.lock().await = merged.clone();
        // The position the page reads comes in a promise, awaited; Chrome
        // runs no microtask while the page is paused in the debugger, so it
        // is left out then (ISSUE-64).
        let paused = target.is_paused().await;
        let resized = args.sets_viewport() || args.resets();
        let (expression, awaited) = match (paused, resized) {
            (true, _) => (EMULATED.to_string(), false),
            // Chrome answers the override before the renderer resized: the
            // page is measured once it did (ISSUE-74).
            (false, true) => (after_resize(&args.measure()), true),
            (false, false) => (args.measure(), args.geolocation.is_some()),
        };
        let read =
            json!({"expression": expression, "awaitPromise": awaited, "returnByValue": true});
        let reply = crate::handler::send_or(&target, "Runtime.evaluate", read, "CDP Error").await?;
        let mut seen = crate::handler::snippet_outcome(reply.result.unwrap_or_default())?;
        merged.annotate(&args, &mut seen);
        if paused && args.geolocation.is_some() {
            seen["geolocation"] = json!(PAUSED_POSITION);
        }
        Ok(args.reply(&seen))
    }
}

/// Sends `commands` in order, failing at the first refused one, except, on
/// a reset, [`BEST_EFFORT_ON_RESET`], which an older Chrome does not know.
async fn apply(
    target: &CdpTarget,
    commands: &[(&str, Value)],
    reset: bool,
) -> Result<(), CallToolError> {
    for (method, params) in commands {
        let sent = crate::handler::send_or(target, method, params.clone(), "CDP Error").await;
        match sent {
            Err(_) if reset && *method == BEST_EFFORT_ON_RESET => {}
            other => {
                other?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(args: Value) -> EmulateTool {
        serde_json::from_value(args).unwrap()
    }

    /// ISSUE-64 (Chrome): on a page paused in the debugger, what the page
    /// sees is read without awaiting a promise (Chrome runs no microtask
    /// then, so the call waited for the 10 s timeout); the position it
    /// would read from a promise is left out, saying so.
    #[tokio::test]
    async fn paused_page_is_measured_without_awaiting() {
        use crate::handler::cdp_domains::tests::{
            call, mock_handler, reply_text, spawn_recording_mock,
        };
        // Given a tab paused in the debugger
        let mock = spawn_recording_mock().await;
        mock.respond_with(
            "Runtime.evaluate",
            |_| json!({"result": {"result": {"type": "object", "value": {"width": 800}}}}),
        );
        let handler = mock_handler(mock.http_port);
        let session = handler.session(None).await.unwrap();
        let debugger = session.debugger_state(None).unwrap();
        debugger.lock().await.paused_call_frame_id = Some("f1".into());
        // When emulate sets a position
        let args = json!({"geolocation": {"latitude": 1.0, "longitude": 2.0}});
        let reply = EmulateTool::handle(call("emulate", args), &handler)
            .await
            .unwrap();
        // Then nothing was awaited, and the reply says why the position is missing
        let reads = mock.commands_named("Runtime.evaluate");
        assert!(
            reads
                .iter()
                .all(|c| c.params["awaitPromise"] != json!(true))
        );
        let text = reply_text(&reply);
        assert!(text.contains("paused in the debugger"), "{text}");
    }

    /// ISSUE-74 (Chrome): a call that resizes the viewport (or resets it)
    /// measures what the page sees once the resize landed: Chrome answers
    /// the override before the renderer resized, so the reply could hold
    /// the previous size. Other options are measured at once.
    #[tokio::test]
    async fn a_resize_is_measured_once_it_landed() {
        use crate::handler::cdp_domains::tests::{call, mock_handler, spawn_recording_mock};
        for (args, waits) in [
            (json!({"width": 400, "height": 300}), true),
            (json!({"reset": true}), true),
            (json!({"color_scheme": "dark"}), false),
        ] {
            // Given a Chrome tab
            let mock = spawn_recording_mock().await;
            mock.respond_with(
                "Runtime.evaluate",
                |_| json!({"result": {"result": {"type": "object", "value": {"width": 400}}}}),
            );
            let handler = mock_handler(mock.http_port);
            // When emulate runs
            EmulateTool::handle(call("emulate", args.clone()), &handler)
                .await
                .unwrap();
            // Then the measure waits for the resize only when it resized
            let reads = mock.commands_named("Runtime.evaluate");
            let read = &reads.last().unwrap().params;
            let expression = read["expression"].as_str().unwrap();
            assert_eq!(
                expression.contains("requestAnimationFrame"),
                waits,
                "{args}"
            );
            assert_eq!(read["awaitPromise"] == json!(true), waits, "{args}");
        }
    }

    /// ISSUE-9: options that make no sense are refused before any browser
    /// work.
    #[test]
    fn senseless_options_are_refused() {
        let error = |args: Value| tool(args).check_arguments().unwrap_err().to_string();
        assert!(error(json!({})).contains("emulate needs a viewport, media, user_agent"));
        assert!(error(json!({"width": 400})).contains("go together"));
        assert!(error(json!({"width": 0, "height": 10})).contains("got 0"));
        assert!(error(json!({"device_scale_factor": 9.0})).contains("0.5 to 5"));
        assert!(error(json!({"reset": true, "color_scheme": "dark"})).contains("no other option"));
        let far = json!({"geolocation": {"latitude": 100.0, "longitude": 0.0}});
        assert!(error(far).contains("latitude must be -90 to 90"));
        assert!(error(json!({"user_agent": " "})).contains("'reset'"));
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
        let commands = phone.cdp_commands(&Emulation::default().merged(&phone));
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
        let reset = tool(json!({"reset": true}));
        assert_eq!(reset.cdp_commands(&Emulation::default()).len(), 11);
        assert_eq!(phone.chrome_only(), ["color_scheme", "media", "mobile"]);
    }

    /// ISSUE-43: each call adds to the earlier ones: Chrome replaces its
    /// device metrics and its media features as a whole, so they are sent
    /// again with what earlier calls set.
    #[test]
    fn calls_add_to_earlier_ones() {
        // Given a dark phone viewport
        let first = tool(
            json!({"color_scheme": "dark", "width": 390, "height": 844, "device_scale_factor": 3.0}),
        );
        let state = Emulation::default().merged(&first);
        // When only reduced motion is asked, Then the colour scheme stays
        let motion = tool(json!({"reduced_motion": true}));
        let state = state.merged(&motion);
        let commands = motion.cdp_commands(&state);
        assert_eq!(commands.len(), 1);
        let features = &commands[0].1["features"];
        assert_eq!(features.as_array().unwrap().len(), 2, "{features}");
        // When only the device scale factor is asked, Then the size stays
        let ratio = tool(json!({"device_scale_factor": 2.0}));
        let commands = ratio.cdp_commands(&state.merged(&ratio));
        assert_eq!(
            commands[0].1,
            json!({"width": 390, "height": 844, "deviceScaleFactor": 2.0, "mobile": false})
        );
    }

    /// ISSUE-43: the time zone and locale are checked by the page's Intl
    /// first (Chrome takes an unknown zone silently); the error says what
    /// both take.
    #[test]
    fn names_are_checked_first() {
        let check = tool(json!({"timezone": "Mars/Base"}))
            .names_check()
            .unwrap();
        assert!(
            check.contains("const zone = \"Mars/Base\", locale = null;"),
            "{check}"
        );
        assert!(
            check.contains("new Intl.DateTimeFormat('en', { timeZone: zone })"),
            "{check}"
        );
        assert!(
            tool(json!({"width": 1, "height": 1}))
                .names_check()
                .is_none()
        );
        let error = EmulateTool::names_error(&json!("Invalid time zone specified: Mars/Base"))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("IANA name such as 'Asia/Tokyo'")
                && error.contains("nothing was changed"),
            "{error}"
        );
        assert!(EmulateTool::names_error(&Value::Null).is_ok());
    }

    /// ISSUE-43: user agent, locale, time zone and geolocation become
    /// their Chrome overrides (geolocation also grants the permission the
    /// page needs to read it) and WebDriver BiDi's on Firefox.
    #[test]
    fn identity_and_place_overrides() {
        let args = tool(
            json!({"user_agent": "Agent/1", "locale": "fr-FR", "timezone": "Asia/Tokyo",
            "geolocation": {"latitude": 40.4, "longitude": -3.7}}),
        );
        let commands = args.cdp_commands(&Emulation::default());
        let methods: Vec<&str> = commands.iter().map(|(m, _)| *m).collect();
        assert_eq!(
            methods,
            [
                "Emulation.setUserAgentOverride",
                "Emulation.setLocaleOverride",
                "Emulation.setTimezoneOverride",
                "Emulation.setGeolocationOverride",
                "Browser.grantPermissions"
            ]
        );
        assert_eq!(
            commands[3].1,
            json!({"latitude": 40.4, "longitude": -3.7, "accuracy": 1.0})
        );
        let bidi = args.bidi_overrides("C1");
        let methods: Vec<&str> = bidi.iter().map(|(m, _)| *m).collect();
        assert_eq!(
            methods,
            [
                "emulation.setUserAgentOverride",
                "emulation.setLocaleOverride",
                "emulation.setTimezoneOverride",
                "emulation.setGeolocationOverride"
            ]
        );
        assert_eq!(
            bidi[2].1,
            json!({"timezone": "Asia/Tokyo", "contexts": ["C1"]})
        );
        let cleared = tool(json!({"reset": true})).bidi_overrides("C1");
        assert_eq!(cleared[0].1, json!({"userAgent": null, "contexts": ["C1"]}));
        assert_eq!(
            cleared[3].1,
            json!({"coordinates": null, "contexts": ["C1"]})
        );
    }

    /// ISSUE-61 (Chrome): the CPU performance tier becomes
    /// `Emulation.setCPUPerformanceOverride`, stays across calls, shows in
    /// the reply, is cleared by `none` (no tier: the override is removed) and
    /// by `reset`, and is refused on Firefox (no `navigator.cpuPerformance`).
    #[test]
    fn cpu_performance_tier() {
        let args = tool(json!({"cpu_performance_tier": "low"}));
        args.check_arguments().unwrap();
        assert_eq!(args.chrome_only(), ["cpu_performance_tier"]);
        let state = Emulation::default().merged(&args);
        assert_eq!(
            args.cdp_commands(&state),
            [(
                "Emulation.setCPUPerformanceOverride",
                json!({"performanceTier": "low"})
            )]
        );
        let later = tool(json!({"color_scheme": "dark"}));
        let state = state.merged(&later);
        let mut seen = json!({});
        state.annotate(&later, &mut seen);
        assert_eq!(seen["cpu_performance_tier"], "low");
        let clear = tool(json!({"cpu_performance_tier": "none"}));
        let cleared = state.merged(&clear);
        assert_eq!(
            clear.cdp_commands(&cleared),
            [("Emulation.setCPUPerformanceOverride", json!({}))]
        );
        let mut seen = json!({});
        cleared.annotate(&clear, &mut seen);
        assert_eq!(seen["cpu_performance_tier"], "none");
        let reset = tool(json!({"reset": true}));
        let commands = reset.cdp_commands(&Emulation::default());
        assert!(commands.contains(&("Emulation.setCPUPerformanceOverride", json!({}))));
        assert!(
            EMULATED.contains("cpu_performance: navigator.cpuPerformance"),
            "{EMULATED}"
        );
    }

    /// ISSUE-52: CPU and network throttling become their Chrome CDP
    /// commands, stay in the tab's emulation state across calls, appear in
    /// the reply, and are refused on Firefox and when out of range.
    #[test]
    fn cpu_and_network_throttling() {
        let error = |args: Value| tool(args).check_arguments().unwrap_err().to_string();
        assert!(error(json!({"cpu_throttling": 0.5})).contains("1 to 20"));
        assert!(error(json!({"cpu_throttling": 25.0})).contains("1 to 20"));
        // Given 4x CPU slowdown and Fast 3G (also accepted by its DevTools name)
        let args = tool(json!({"cpu_throttling": 4.0, "network_throttling": "Fast 3G"}));
        args.check_arguments().unwrap();
        assert_eq!(args.chrome_only(), ["cpu_throttling", "network_throttling"]);
        let state = Emulation::default().merged(&args);
        let commands = args.cdp_commands(&state);
        assert_eq!(
            commands,
            [
                ("Emulation.setCPUThrottlingRate", json!({"rate": 4.0})),
                (
                    "Network.emulateNetworkConditions",
                    json!({"offline": false, "latency": 562.5, "downloadThroughput": 180_000.0, "uploadThroughput": 84_375.0})
                )
            ]
        );
        // When a later call only changes the colour scheme, Then the reply still shows the active throttling
        let later = tool(json!({"color_scheme": "dark"}));
        let state = state.merged(&later);
        let mut seen = json!({"width": 800});
        state.annotate(&later, &mut seen);
        assert_eq!(
            (
                seen["cpu_throttling"].clone(),
                seen["network_throttling"].clone()
            ),
            (json!(4.0), json!("fast-3g"))
        );
        // And setting 1 and "none" clears them in the state while confirming them in that call's reply
        let clear = tool(json!({"cpu_throttling": 1.0, "network_throttling": "none"}));
        let state = state.merged(&clear);
        assert_eq!(
            (state.cpu_throttling, state.network_throttling),
            (None, None)
        );
        let mut seen = json!({});
        state.annotate(&clear, &mut seen);
        assert_eq!(
            (
                seen["cpu_throttling"].clone(),
                seen["network_throttling"].clone()
            ),
            (json!(1.0), json!("none"))
        );
    }
}
