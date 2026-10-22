//! LX settings archive and a separate, non-executing restore projection.

use crate::store::StoreData;
use anyhow::{Result, ensure};
use serde_json::{Map, Value};

#[derive(Clone, Debug, PartialEq)]
pub struct LxSettings {
    pub values: Map<String, Value>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SettingsPreview {
    /// Compatibility archive, including unknown and currently unsupported keys.
    pub archived: Map<String, Value>,
    /// Preferences only. Service activation and every execution permission require
    /// their normal explicit authorization flows after the repository transaction.
    pub preferences: Map<String, Value>,
    pub requires_activation: Vec<String>,
}

impl LxSettings {
    pub fn from_value(value: &Value, legacy: bool) -> Result<Self> {
        let mut values = value
            .as_object()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("Settings must be an object"))?;
        if legacy {
            migrate(&mut values)?;
        }
        validate(&values)?;
        Ok(Self { values })
    }

    pub fn preview(&self) -> Result<SettingsPreview> {
        validate(&self.values)?;
        let mut preferences = self.values.clone();
        preferences.insert("common.isAgreePact".into(), false.into());
        let mut requires_activation = Vec::new();
        for key in [
            "sync.enable",
            "openAPI.enable",
            "openAPI.bindLan",
            "openApi.enable",
            "api.enable",
            "download.enable",
            "player.startupAutoPlay",
        ] {
            if preferences.get(key).and_then(Value::as_bool) == Some(true) {
                requires_activation.push(key.into());
            }
            if preferences.contains_key(key) {
                preferences.insert(key.into(), false.into());
            }
        }
        // Unknown fields are archived, never delegated to a permission loader.
        for key in [
            "sourceApproval",
            "sourceApprovals",
            "urlApprovals",
            "localFileGrants",
            "downloadGrants",
            "sync.authKey",
            "sync.client.key",
            "openAPI.token",
            "openApi.token",
        ] {
            if preferences.remove(key).is_some() {
                requires_activation.push(key.into());
            }
        }
        Ok(SettingsPreview {
            archived: self.values.clone(),
            preferences,
            requires_activation,
        })
    }
}

impl SettingsPreview {
    /// Projects only implemented player preferences; unsupported LX keys remain
    /// available in `archived` for compatibility and subsequent explicit mapping.
    pub fn project_store(&self, current: &StoreData) -> Result<StoreData> {
        let mut store = current.clone();
        if let Some(value) = self.preferences.get("player.volume") {
            store.volume = value
                .as_f64()
                .ok_or_else(|| anyhow::anyhow!("Invalid player volume"))?
                as f32;
        }
        if let Some(value) = self.preferences.get("player.isMute") {
            store.audio_preferences.muted = value
                .as_bool()
                .ok_or_else(|| anyhow::anyhow!("Invalid mute preference"))?;
        }
        if let Some(value) = self.preferences.get("player.isMediaDeviceRemovedStopPlay") {
            store.audio_preferences.pause_on_device_change = value
                .as_bool()
                .ok_or_else(|| anyhow::anyhow!("Invalid device preference"))?;
        }
        if let Some(value) = self.preferences.get("player.playbackRate") {
            store.playback_processing.playback_rate = value
                .as_f64()
                .ok_or_else(|| anyhow::anyhow!("Invalid playback rate"))?;
        }
        if let Some(value) = self.preferences.get("player.preservesPitch") {
            store.playback_processing.preserves_pitch = value
                .as_bool()
                .ok_or_else(|| anyhow::anyhow!("Invalid pitch preference"))?;
        }
        if let Some(value) = self
            .preferences
            .get("player.soundEffect.pitchShifter.playbackRate")
        {
            store.playback_processing.pitch_ratio = value
                .as_f64()
                .ok_or_else(|| anyhow::anyhow!("Invalid pitch ratio"))?;
        }
        for (index, band) in [
            "hz31", "hz62", "hz125", "hz250", "hz500", "hz1000", "hz2000", "hz4000", "hz8000",
            "hz16000",
        ]
        .iter()
        .enumerate()
        {
            if let Some(value) = self
                .preferences
                .get(&format!("player.soundEffect.biquadFilter.{band}"))
            {
                store.equalizer.gains_db[index] = value
                    .as_f64()
                    .ok_or_else(|| anyhow::anyhow!("Invalid equalizer gain"))?;
            }
        }
        store.audio_preferences.validate()?;
        store.equalizer.validate()?;
        store.playback_processing.validate()?;
        if let Some(value) = self.preferences.get("player.soundEffect.panner.enable") {
            store.spatial.enabled = value
                .as_bool()
                .ok_or_else(|| anyhow::anyhow!("Invalid spatial enable preference"))?;
        }
        if let Some(value) = self.preferences.get("player.soundEffect.panner.soundR") {
            store.spatial.radius = value
                .as_f64()
                .ok_or_else(|| anyhow::anyhow!("Invalid spatial radius preference"))?
                / 10.0;
        }
        if let Some(value) = self.preferences.get("player.soundEffect.panner.speed") {
            let speed = value
                .as_f64()
                .ok_or_else(|| anyhow::anyhow!("Invalid spatial speed preference"))?;
            ensure!(
                speed.fract() == 0.0 && (1.0..=50.0).contains(&speed),
                "Invalid spatial speed preference"
            );
            store.spatial.speed = speed as u8;
        }
        store.spatial.validate()?;
        if let Some(value) = self.preferences.get("player.waitPlayEndStop") {
            store.timeout_stop.wait_for_track_end = value
                .as_bool()
                .ok_or_else(|| anyhow::anyhow!("Invalid sleep timer wait preference"))?;
        }
        if let Some(value) = self.preferences.get("player.waitPlayEndStopTime") {
            store.timeout_stop.last_seconds = timeout_seconds(value)?;
        }
        store.timeout_stop.validate()?;
        Ok(store)
    }
}

fn timeout_seconds(value: &Value) -> Result<Option<f64>> {
    let minutes = if let Some(text) = value.as_str() {
        if text.is_empty() {
            return Ok(None);
        }
        ensure!(
            text.len() <= 16 && text.bytes().all(|byte| byte.is_ascii_digit()),
            "Invalid sleep timer minute preference"
        );
        text.parse::<u64>()?
    } else {
        value
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("Invalid sleep timer minute preference"))?
    };
    ensure!(
        (1..=1440).contains(&minutes),
        "Sleep timer minutes must be between one and 1440"
    );
    Ok(Some(minutes as f64 * 60.0))
}

fn version(value: &str) -> Result<[u32; 3]> {
    let fields: Vec<_> = value.split('.').collect();
    ensure!(
        fields.len() == 3,
        "Settings version must use three numeric components"
    );
    let mut result = [0; 3];
    for (target, field) in result.iter_mut().zip(fields) {
        *target = field.parse()?;
    }
    Ok(result)
}

fn validate(values: &Map<String, Value>) -> Result<()> {
    if let Some(value) = values.get("version") {
        let parsed = version(
            value
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("Invalid settings version"))?,
        )?;
        ensure!(
            parsed <= [2, 1, 0],
            "Unsupported future LX settings version"
        );
    }
    for (key, value) in values {
        if key == "player.waitPlayEndStop" {
            ensure!(value.is_boolean(), "Invalid sleep timer wait preference");
        }
        if key == "player.waitPlayEndStopTime" {
            timeout_seconds(value)?;
        }
        if key == "player.soundEffect.panner.enable" {
            ensure!(value.is_boolean(), "Invalid spatial enable preference");
        }
        if key == "player.soundEffect.panner.soundR" {
            number(value, 1.0, 30.0)?;
        }
        if key == "player.soundEffect.panner.speed" {
            number(value, 1.0, 50.0)?;
            ensure!(
                value.as_f64().is_some_and(|speed| speed.fract() == 0.0),
                "Invalid spatial speed preference"
            );
        }
        ensure!(
            key.len() <= 1024 && !key.contains('\0'),
            "Settings key is invalid"
        );
        if key == "player.volume" {
            number(value, 0.0, 1.0)?;
        }
        if key == "player.playbackRate" {
            number(value, 0.5, 2.0)?;
        }
        if key == "player.soundEffect.pitchShifter.playbackRate" {
            number(value, 0.5, 1.5)?;
        }
        if key.starts_with("player.soundEffect.biquadFilter.") {
            number(value, -15.0, 15.0)?;
        }
        if key == "download.maxDownloadNum" {
            ensure!(
                value.as_u64().is_some_and(|v| (1..=4).contains(&v)),
                "Download concurrency is invalid"
            );
        }
        if matches!(
            key.as_str(),
            "sync.server.port" | "sync.erver.port" | "network.proxy.port" | "openAPI.port"
        ) && value != ""
            && !value.is_null()
        {
            let port = value
                .as_u64()
                .or_else(|| value.as_str().and_then(|v| v.parse().ok()));
            ensure!(
                port.is_some_and(|p| (1..=65535).contains(&p)),
                "Port is invalid"
            );
        }
        if matches!(
            key.as_str(),
            "player.isMute"
                | "player.preservesPitch"
                | "player.isMediaDeviceRemovedStopPlay"
                | "common.isAgreePact"
                | "sync.enable"
                | "download.enable"
                | "openAPI.enable"
                | "openAPI.bindLan"
                | "openApi.enable"
                | "api.enable"
                | "player.startupAutoPlay"
                | "network.proxy.enable"
        ) {
            ensure!(
                value.is_boolean(),
                "Boolean settings field is invalid: {key}"
            );
        }
    }
    super::codec::json_bytes(values)?;
    Ok(())
}
fn number(value: &Value, minimum: f64, maximum: f64) -> Result<()> {
    ensure!(
        value
            .as_f64()
            .is_some_and(|v| v.is_finite() && (minimum..=maximum).contains(&v)),
        "Numeric settings field is out of range"
    );
    Ok(())
}

fn migrate(values: &mut Map<String, Value>) -> Result<()> {
    let original = values.clone();
    let old_version = original
        .get("version")
        .map(|v| {
            v.as_str()
                .ok_or_else(|| anyhow::anyhow!("Invalid settings version"))
        })
        .transpose()?
        .unwrap_or("0.0.0");
    let mut current = version(old_version)?;
    ensure!(
        current <= [2, 1, 0],
        "Unsupported future LX settings version"
    );
    if current < [2, 0, 0] {
        for key in [
            "windowSizeId",
            "startInFullscreen",
            "langId",
            "apiSource",
            "sourceNameType",
            "font",
            "isShowAnimation",
            "randomAnimate",
            "isAgreePact",
            "controlBtnPosition",
        ] {
            if let Some(value) = original.get(key) {
                values.insert(format!("common.{key}"), value.clone());
            }
        }
        for (group, keys) in [
            (
                "player",
                "togglePlayMethod isShowTaskProgess volume isMute mediaDeviceId isMediaDeviceRemovedStopPlay isShowLyricTranslation isShowLyricRoma isS2t isPlayLxlrc isSavePlayTime audioVisualization waitPlayEndStop waitPlayEndStopTime autoSkipOnError",
            ),
            (
                "playDetail",
                "isZoomActiveLrc isShowLyricProgressSetting style.fontSize style.align",
            ),
            (
                "desktopLyric",
                "enable isLock isAlwaysOnTop isAlwaysOnTopLoop width height x y isLockScreen isDelayScroll isHoverHide style.font style.opacity style.isZoomActiveLrc",
            ),
            (
                "list",
                "isClickPlayList isShowAlbumName isShowSource isSaveScrollLocation addMusicLocationType",
            ),
            (
                "download",
                "enable savePath fileName maxDownloadNum isDownloadLrc lrcFormat isEmbedPic isEmbedLyric isUseOtherSource",
            ),
            (
                "search",
                "isShowHotSearch isShowHistorySearch isFocusSearchBox",
            ),
            ("network", "proxy.enable proxy.host proxy.port"),
            ("tray", "enable themeId"),
            ("sync", "enable port"),
            ("odc", "isAutoClearSearchInput isAutoClearSearchList"),
        ] {
            for key in keys.split_whitespace() {
                if let Some(value) = nested(&original, &format!("{group}.{key}")) {
                    values.insert(format!("{group}.{key}"), value.clone());
                }
            }
        }
        if let Some(value) = nested(&original, "player.isShowLyricTransition") {
            values.insert("player.isShowLyricTranslation".into(), value.clone());
        }
        if let Some(value) = nested(&original, "tray.isShow") {
            values.insert("tray.enable".into(), value.clone());
        }
        if let (Some(saved), Some(enabled)) = (
            nested(&original, "list.isSaveScrollLocation").and_then(Value::as_bool),
            nested(&original, "list.scroll.enable").and_then(Value::as_bool),
        ) {
            values.insert(
                "list.isSaveScrollLocation".into(),
                (saved && enabled).into(),
            );
        }
        if let Some(size) = nested(&original, "desktopLyric.style.fontSize").and_then(Value::as_f64)
        {
            values.insert(
                "desktopLyric.style.fontSize".into(),
                (size / 100.0 * 16.0).into(),
            );
        }
        let themes = [
            "green",
            "blue",
            "yellow",
            "orange",
            "red",
            "purple",
            "grey",
            "mid_autumn",
            "naruto",
            "happy_new_year",
            "pink",
            "ming",
            "blue2",
            "black",
        ];
        for key in ["id", "lightId", "darkId"] {
            let value = if key == "id" {
                original
                    .get("themeId")
                    .or_else(|| nested(&original, "theme.id"))
            } else {
                nested(&original, &format!("theme.{key}"))
            };
            if let Some(theme) = value
                .and_then(Value::as_u64)
                .and_then(|v| themes.get(v as usize))
            {
                values.insert(format!("theme.{key}"), (*theme).into());
            }
        }
        current = [2, 0, 0];
    }
    if current < [2, 1, 0] {
        // The fixed LX migration contains this spelling; archive it faithfully.
        if let Some(port) = values.get("sync.port").cloned() {
            values.insert("sync.erver.port".into(), port);
        }
    }
    values.insert("version".into(), "2.1.0".into());
    Ok(())
}
fn nested<'a>(values: &'a Map<String, Value>, path: &str) -> Option<&'a Value> {
    let mut fields = path.split('.');
    let mut value = values.get(fields.next()?)?;
    for field in fields {
        value = value.get(field)?;
    }
    Some(value)
}
