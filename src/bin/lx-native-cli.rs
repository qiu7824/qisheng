//! JSON command entry point that does not initialize GPUI or a native window.
use anyhow::{Context, Result, bail, ensure};
use lx_gpui_player::{
    backend::{
        BackendAudioMode, BackendCommand, BackendConfig, BackendEvent, BackendHandle, BackendQuery,
        BackendQueryResult, CommandEnvelope, LocalTagEditPatch, ReceiptState, TagEditField,
        default_data_dir,
    },
    repository::ProfileGuard,
    system_control::instance_relay::RelayRequest,
    worker,
};
use serde_json::{Value, json};
use std::{
    io::{self, BufRead, Read, Write},
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};

const MAX_COMMAND_BYTES: usize = 2 * 1024 * 1024;
const TAG_COMMANDS: [&str; 7] = [
    "read_local_tags",
    "approve_tag_write",
    "revoke_tag_write",
    "approve_tag_recovery",
    "edit_local_tags",
    "recover_tag_edit",
    "cancel_tag_edit",
];
const TAG_QUERIES: [&str; 4] = ["local_tags", "tag_write_approval", "tag_edit", "tag_edits"];
#[cfg(windows)]
use lx_gpui_player::{
    repository::ProfileGuardFailure, system_control::instance_relay::RelayDisposition,
};
#[path = "lx-native-cli/control.rs"]
mod cli_control;
#[path = "lx-native-cli/sync_control.rs"]
mod cli_sync_control;
fn main() {
    if worker::worker_main() {
        return;
    }
    if let Err(error) = run() {
        eprintln!("{}", json!({"error":format!("{error:#}")}));
        std::process::exit(1);
    }
}
fn run() -> Result<()> {
    let mut arguments = std::env::args().skip(1);
    let mut data_dir = None;
    let mut portable = false;
    let mut mode = BackendAudioMode::Memory;
    let mut command = None;
    let mut json_lines = false;
    let mut instance_relay = false;
    let mut positional = Vec::new();
    let mut provider_proxy = None;
    let mut provider_timeout = Duration::from_secs(20);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--data-dir" => {
                data_dir = Some(PathBuf::from(
                    arguments.next().context("--data-dir requires a path")?,
                ))
            }
            "--portable" => portable = true,
            "--provider-proxy" => {
                provider_proxy = Some(
                    arguments
                        .next()
                        .context("--provider-proxy requires a URL")?,
                )
            }
            "--provider-timeout-ms" => {
                let milliseconds: u64 = arguments
                    .next()
                    .context("--provider-timeout-ms requires milliseconds")?
                    .parse()?;
                ensure!(
                    (1..=120000).contains(&milliseconds),
                    "Provider timeout must be 1..=120000 milliseconds"
                );
                provider_timeout = Duration::from_millis(milliseconds);
            }
            "--audio" => {
                mode = match arguments.next().as_deref() {
                    Some("device") => BackendAudioMode::Device,
                    Some("memory") => BackendAudioMode::Memory,
                    _ => bail!("--audio requires device or memory"),
                }
            }
            "--command" => {
                command = Some(
                    arguments
                        .next()
                        .context("--command requires a JSON object")?,
                )
            }
            "--json-lines" => json_lines = true,
            "--instance-relay" => instance_relay = true,
            "--help" | "-h" => {
                println!(
                    "LX Native backend {}\nUsage: lx-native-cli [--data-dir PATH | --portable] [--audio memory|device] [--instance-relay] COMMAND\nCommands: status, capabilities, search [TEXT], search-online PLATFORM TEXT, deeplink URL, import-local PATH..., cache-stats, clear-cache, preload, approve-preload OPERATION URL, cancel-preload OPERATION, --command JSON, --json-lines\nProvider options: --provider-proxy URL, --provider-timeout-ms 1..120000\nInstance relay: Windows opt-in host with --instance-relay --json-lines. A second deeplink command forwards to that same profile and acknowledges acceptance; CAS envelopes cannot be forwarded.\nJSON queries: {{\"query\":\"status\"}}, {{\"query\":\"tracks\",\"text\":\"...\",\"offset\":0,\"limit\":100}}, {{\"query\":\"events\",\"cursor\":0}}, {{\"query\":\"provider_result\",\"operation_id\":\"id\"}}\nJSON Lines endpoint services: open_api_start, open_api_rotate, open_api_stop; query: open_api. Command envelopes require request_id and command. Memory audio renders real decoded PCM without opening a device.",
                    env!("CARGO_PKG_VERSION")
                );
                print_tag_help();
                println!(
                    "Effects: import-impulse PATH, effects, impulse-resources. JSON commands: import_impulse_resource requires path and file_read_approved=true; set_effects, reset_effects, recover_impulse_import, retire_impulse_resource, recover_impulse_retirement, cancel_effects_operation. effects_state reports requested and actually presented sequences separately."
                );
                return Ok(());
            }
            argument => positional.push(argument.to_owned()),
        }
    }
    ensure!(
        !json_lines || (command.is_none() && positional.is_empty()),
        "--json-lines cannot be combined with a one-shot command"
    );
    let one_shot = if json_lines {
        None
    } else {
        Some(one_shot_value(command, &positional)?)
    };
    // Validate the entire envelope and its bounded deep link before acquiring
    // or creating a profile, including for a prospective first instance.
    let relay_envelope = one_shot.as_ref().map(relay_envelope).transpose()?.flatten();
    let data_dir = match data_dir {
        Some(path) => path,
        None => default_data_dir(portable)?,
    };
    let guard = match ProfileGuard::acquire(&data_dir) {
        Ok(guard) => guard,
        Err(failure) => {
            #[cfg(windows)]
            if profile_is_busy(&failure)
                && let Some(envelope) = relay_envelope
            {
                ensure!(
                    envelope.expected_revision.is_none(),
                    "A revision-constrained envelope cannot be forwarded to another instance"
                );
                let guard = failure
                    .owner
                    .as_ref()
                    .context("Busy profile has no retained descriptor")?;
                ensure!(
                    !guard.is_locked(),
                    "Busy profile unexpectedly owns a writer lock"
                );
                return forward(envelope, guard);
            }
            return Err(anyhow::Error::new(failure));
        }
    };
    #[cfg(not(windows))]
    let _ = relay_envelope;
    let mut config = BackendConfig::new(data_dir);
    config.audio_mode = mode;
    config.provider_timeout = provider_timeout;
    if let Some(proxy) = provider_proxy {
        let transport = lx_gpui_player::platforms::ReqwestTransport::new(
            lx_gpui_player::platforms::TransportConfig {
                proxy: Some(proxy),
                ..Default::default()
            },
        )?;
        config.provider_registry = Some(std::sync::Arc::new(
            lx_gpui_player::platforms::ProviderRegistry::new(std::sync::Arc::new(transport)),
        ));
    }
    let backend = if instance_relay {
        BackendHandle::start_with_guard_and_instance_relay(config, guard)?
    } else {
        BackendHandle::start_with_guard(config, guard)?
    };
    let mut services = cli_control::Services::new(&backend, json_lines);
    let result = (|| -> Result<()> {
        if json_lines {
            let stdin = io::stdin();
            let mut input = stdin.lock();
            let mut output = io::stdout().lock();
            loop {
                let mut bytes = Vec::new();
                let read = input
                    .by_ref()
                    .take((MAX_COMMAND_BYTES + 1) as u64)
                    .read_until(b'\n', &mut bytes)?;
                if read == 0 {
                    break;
                }
                ensure!(
                    bytes.len() <= MAX_COMMAND_BYTES,
                    "JSON command line exceeds 2 MiB"
                );
                let response = match serde_json::from_slice::<Value>(&bytes)
                    .context("Invalid command JSON")
                    .and_then(|value| match services.dispatch(&value)? {
                        Some(result) => Ok(result),
                        None => dispatch(&backend, value, false),
                    }) {
                    Ok(value) => json!({"ok":true,"result":value}),
                    Err(error) => json!({"ok":false,"error":format!("{error:#}")}),
                };
                writeln!(output, "{response}")?;
                output.flush()?;
            }
        } else {
            let value = one_shot.context("Missing one-shot command")?;
            let result = match services.dispatch(&value)? {
                Some(result) => result,
                None => dispatch(&backend, value, true)?,
            };
            let success = result
                .get("provider_result")
                .and_then(|result| result.get("state"))
                .and_then(Value::as_str)
                != Some("failed")
                && result.get("success").and_then(Value::as_bool) != Some(false);
            println!("{}", json!({"ok":success,"result":result}));
            ensure!(success, "Provider operation failed");
        }
        Ok(())
    })();
    let shutdown_deadline = Instant::now() + Duration::from_secs(5);
    let services_stopped =
        services.shutdown(shutdown_deadline.saturating_duration_since(Instant::now()));
    let stopped = backend.shutdown(shutdown_deadline.saturating_duration_since(Instant::now()));
    result?;
    services_stopped?;
    stopped?;
    Ok(())
}
fn one_shot_value(command: Option<String>, positional: &[String]) -> Result<Value> {
    if let Some(command) = command {
        ensure!(command.len() <= MAX_COMMAND_BYTES, "Command exceeds 2 MiB");
        let value = serde_json::from_str(&command)?;
        validate_tag_request(&value)?;
        return Ok(value);
    }
    Ok(
        match positional.first().map(String::as_str).unwrap_or("status") {
            "status" => json!({"query":"status"}),
            "capabilities" => json!({"query":"capabilities"}),
            "tag-edits" => json!({"query":"tag_edits"}),
            "local-tags" | "tag-edit" | "tag-write-approval" => {
                ensure!(
                    positional.len() == 2,
                    "Tag query requires its operation or approval ID"
                );
                match positional[0].as_str() {
                    "local-tags" => json!({"query":"local_tags","operation_id":positional[1]}),
                    "tag-edit" => json!({"query":"tag_edit","operation_id":positional[1]}),
                    _ => json!({"query":"tag_write_approval","approval_id":positional[1]}),
                }
            }
            "read-local-tags" => {
                ensure!(
                    positional.len() == 3,
                    "read-local-tags requires imported asset ID and tag operation ID"
                );
                json!({"request_id":tag_request_id()?,"command":"read_local_tags","asset_id":positional[1],"tag_operation_id":positional[2]})
            }
            "lists" => json!({"query":"list_infos"}),
            "queue" => json!({"query":"queue","offset":0,"limit":100}),
            "play-download" => {
                ensure!(
                    positional.len() == 2,
                    "play-download requires a download task ID"
                );
                json!({"request_id":tag_request_id()?,"command":"play_download_task","task_id":positional[1]})
            }
            "cache-stats" => json!({"query":"audio_cache_stats"}),
            "preload" => json!({"query":"preload"}),
            "approve-preload" => {
                ensure!(
                    positional.len() == 3,
                    "approve-preload requires operation ID and exact URL"
                );
                json!({"request_id":"cli-approve-preload","command":"approve_preload_url","operation_id":positional[1],"url":positional[2]})
            }
            "cancel-preload" => {
                ensure!(
                    positional.len() == 2,
                    "cancel-preload requires operation ID"
                );
                json!({"request_id":"cli-cancel-preload","command":"cancel_preload","operation_id":positional[1]})
            }
            "clear-cache" => json!({"request_id":"cli-clear-cache","command":"clear_audio_cache"}),
            "import-impulse" => {
                ensure!(
                    positional.len() == 2,
                    "import-impulse requires one selected WAV or FLAC path"
                );
                json!({"request_id":tag_request_id()?,"command":"import_impulse_resource","path":positional[1],"file_read_approved":true})
            }
            "effects" => json!({"query":"effects_state"}),
            "effects-presets" => json!({"query":"bundled_impulse_presets"}),
            "apply-effect-preset" => {
                ensure!(
                    positional.len() == 2,
                    "apply-effect-preset requires a preset ID"
                );
                json!({"request_id":tag_request_id()?,"command":"apply_bundled_impulse_preset","id":positional[1]})
            }
            "impulse-resources" => json!({"query":"impulse_resources"}),
            "list" => {
                ensure!(positional.len() == 2, "list requires a list ID");
                json!({"query":"list_tracks","list_id":positional[1],"offset":0,"limit":100})
            }
            "play-list" => {
                ensure!(
                    positional.len() == 3,
                    "play-list requires list ID and music ID"
                );
                json!({"request_id":"cli-play-list","command":"play_list_track","list_id":positional[1],"music_id":positional[2]})
            }
            "search" => json!({"query":"tracks","text":positional[1..].join(" "),"limit":100}),
            "deeplink" => {
                ensure!(positional.len() == 2, "deeplink requires one lxmusic URL");
                let mut nonce = [0u8; 16];
                getrandom::fill(&mut nonce).map_err(|error| {
                    anyhow::anyhow!("Cannot create deep-link request identifier: {error}")
                })?;
                let suffix: String = nonce.iter().map(|byte| format!("{byte:02x}")).collect();
                json!({"request_id":format!("cli-link-{}-{suffix}",std::process::id()),"command":"dispatch_deep_link","url":positional[1]})
            }
            "import-local" => {
                ensure!(
                    positional.len() > 1,
                    "import-local requires audio file paths"
                );
                json!({"request_id":"cli-import-local","command":"import_audio","paths":positional[1..]})
            }
            "search-online" => {
                ensure!(
                    positional.len() > 2,
                    "search-online requires platform and keyword"
                );
                json!({"request_id":"cli-search-online","command":"provider_request","platform":positional[1],"request":{"operation":"search","keyword":positional[2..].join(" "),"page":1,"limit":30}})
            }
            other => bail!("Unknown command: {other}"),
        },
    )
}
fn relay_envelope(value: &Value) -> Result<Option<CommandEnvelope>> {
    if value.get("command").and_then(Value::as_str) != Some("dispatch_deep_link") {
        return Ok(None);
    }
    let envelope: CommandEnvelope = serde_json::from_value(value.clone())?;
    ensure!(
        !envelope.origin.is_empty()
            && envelope.origin.len() <= 128
            && !envelope.origin.chars().any(char::is_control),
        "Invalid deep-link origin"
    );
    let BackendCommand::DispatchDeepLink { url } = &envelope.command else {
        bail!("Invalid deep-link command");
    };
    RelayRequest::new(envelope.request_id.clone(), url.clone())?;
    Ok(Some(envelope))
}
#[cfg(windows)]
fn profile_is_busy(failure: &ProfileGuardFailure) -> bool {
    failure.error.error.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<std::fs::TryLockError>(),
            Some(std::fs::TryLockError::WouldBlock)
        )
    })
}
#[cfg(windows)]
fn forward(envelope: CommandEnvelope, guard: &ProfileGuard) -> Result<()> {
    let BackendCommand::DispatchDeepLink { url } = envelope.command else {
        bail!("Only deep links can be forwarded");
    };
    let acknowledgement = match lx_gpui_player::backend::forward_deeplink_to_existing_profile(
        guard,
        envelope.request_id,
        url,
        Duration::from_secs(5),
    ) {
        Ok(acknowledgement) => acknowledgement,
        Err(mut failure) => {
            if let Some(owner) = failure.owner.as_mut() {
                loop {
                    match owner.shutdown(Duration::from_millis(100)) {
                        Ok(true) => break,
                        Ok(false) => continue,
                        // The typed error retains the actual owner if physical
                        // retirement cannot be confirmed. It is never replaced
                        // with an effect-free retry or another profile writer.
                        Err(_) => return Err(anyhow::Error::new(failure)),
                    }
                }
            }
            return Err(anyhow::Error::new(failure.error));
        }
    };
    let success = matches!(
        acknowledgement.disposition,
        RelayDisposition::Applied
            | RelayDisposition::Committed
            | RelayDisposition::Accepted
            | RelayDisposition::Completed
    );
    let completed = matches!(
        acknowledgement.disposition,
        RelayDisposition::Applied | RelayDisposition::Committed | RelayDisposition::Completed
    );
    println!(
        "{}",
        json!({"ok":success,"result":{"relay":acknowledgement,"completed":completed}})
    );
    ensure!(success, "Existing instance rejected the deep link");
    Ok(())
}
fn dispatch(backend: &BackendHandle, value: Value, wait_for_completion: bool) -> Result<Value> {
    validate_tag_request(&value)?;
    if let Some(query) = value.get("query").and_then(Value::as_str) {
        return match query {
            "status" => Ok(serde_json::to_value(backend.snapshot())?),
            "local_tags"
            | "tag_write_approval"
            | "tag_edit"
            | "tag_edits"
            | "library_roots"
            | "system_control_settings"
            | "system_control_state"
            | "audio_supply"
            | "audio_output_render_timing"
            | "deep_link_result"
            | "lx_backup_preview"
            | "lx_backup_import_result"
            | "native_backup_preview"
            | "native_backup_import_result"
            | "native_backup_archive"
            | "backup_export"
            | "physical_snapshot"
            | "physical_snapshots"
            | "download_settings"
            | "download_cleanup"
            | "download_destinations"
            | "download_tasks"
            | "download_task"
            | "download_pending_url"
            | "library_watch"
            | "library_watches"
            | "library_asset_binding"
            | "library_root_binding"
            | "library_relink_candidates"
            | "library_relink_result"
            | "library_root"
            | "library_root_health"
            | "library_operations"
            | "library_operation"
            | "library_assets"
            | "list_infos"
            | "list_tracks"
            | "list_track"
            | "list_positions"
            | "list_update_infos"
            | "dislike_rules"
            | "search_history"
            | "queue"
            | "playback_checkpoint"
            | "audio_cache_stats"
            | "audio_settings"
            | "equalizer_settings"
            | "playback_processing_settings"
            | "playback_processing_state"
            | "equalizer_presets"
            | "equalizer_state"
            | "audio_output_state"
            | "audio_devices"
            | "preload"
            | "media_cache_stats"
            | "music_matches"
            | "picture_info"
            | "picture_operations"
            | "picture_cache_info"
            | "picture_cached_bytes"
            | "image_cache_stats"
            | "spatial_settings"
            | "spatial_state"
            | "effects_settings"
            | "effects_state"
            | "bundled_impulse_presets"
            | "impulse_resources"
            | "effects_operations"
            | "timeout_stop"
            | "timeout_stop_preferences"
            | "audio_analysis_enabled"
            | "audio_analysis"
            | "rendered_pcm" => Ok(serde_json::to_value(
                backend.query(serde_json::from_value::<BackendQuery>(value.clone())?)?,
            )?),
            "capabilities" => Ok(tag_capabilities(
                json!({"protocol_version":lx_gpui_player::backend::PROTOCOL_VERSION,"headless":true,"services":["open_api_start","open_api_rotate","open_api_stop","sync_server_start","sync_client_start","sync_stop","sync_revoke_peer","sync_choose_mode"],"service_queries":["open_api","sync","sync_peers"],"commands":["set_system_control_settings","retry_system_control","dispatch_deep_link","preview_lx_backup","commit_lx_backup","preview_native_backup","commit_native_backup","export_backup","export_physical_snapshot","recover_physical_snapshot","retry_backup_export","cleanup_backup_export","add_download_destination","revoke_download_destination","set_download_settings","add_download_tasks","remove_download_task","resume_download_task","retry_download_postprocessing","pause_download_task","recover_download_commit","recover_download_cleanup","cancel_download_cleanup","approve_download_url","set_library_watch","relink_local_asset","confirm_library_relink","rebind_library_root","import_source","import_source_text","import_audio","add_library_root","remove_library_root","scan_library_root","add_demo_source","approve_source","enable_source","disable_source","delete_source","play","pause","resume","toggle","previous","next","stop","seek","volume","clear_error","approve_playback_url","cancel_playback_url","cancel_operation","load_lyrics","save_lyrics","restore_lyrics","clear_raw_lyrics","set_lyric_offset","provider_request","list_action","dislike_action","select_list","play_list_track","save_provider_tracks","enqueue","remove_queue_entry","clear_temporary_queue","set_play_mode","set_playback_quality","set_auto_skip_on_error","set_auto_switch_source","find_music_matches","refresh_playback_url","clear_media_cache","dislike_current","set_list_position","set_list_update_info","record_search","remove_search_history","clear_search_history","clear_audio_cache","approve_preload_url","cancel_preload","resolve_picture","approve_picture_url","cancel_picture","clear_image_cache","set_audio_output_device","refresh_audio_devices","set_mute","set_pause_on_device_change","start_timeout_stop","cancel_timeout_stop","set_timeout_stop_preferences","set_spatial_settings","set_audio_analysis_enabled","set_equalizer","set_equalizer_preset","reset_equalizer","set_playback_processing","set_playback_rate","set_preserves_pitch","set_pitch_ratio","reset_playback_processing"],"queries":["system_control_settings","system_control_state","audio_supply","audio_output_render_timing","deep_link_result","lx_backup_preview","lx_backup_import_result","native_backup_preview","native_backup_import_result","native_backup_archive","backup_export","physical_snapshot","physical_snapshots","download_settings","download_cleanup","download_destinations","download_tasks","download_task","download_pending_url","library_watch","library_watches","library_asset_binding","library_root_binding","library_relink_candidates","library_relink_result","library_roots","library_root","library_root_health","library_operations","library_operation","library_assets","status","tracks","events","event_cursor","snapshot_cursor","lyrics","lyrics_export","provider_result","list_infos","list_tracks","list_track","list_positions","list_update_infos","dislike_rules","search_history","queue","playback_checkpoint","rendered_pcm","audio_analysis","audio_analysis_enabled","spatial_settings","spatial_state","timeout_stop","timeout_stop_preferences","audio_cache_stats","media_cache_stats","music_matches","preload","picture_info","picture_operations","picture_cache_info","picture_cached_bytes","image_cache_stats","audio_settings","audio_output_state","audio_devices","equalizer_settings","equalizer_presets","equalizer_state","playback_processing_settings","playback_processing_state"],"online_providers":["kw","kg","tx","wy","mg"],"play_modes":["listLoop","random","list","singleLoop","none"],"persisted_lists":true,"queue_checkpoint":true,"audio_output":["memory","device"],"complete_lx_backend":false}),
            )),
            "tracks" => Ok(serde_json::to_value(backend.tracks(
                value.get("text").and_then(Value::as_str).unwrap_or(""),
                value.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize,
                value.get("limit").and_then(Value::as_u64).unwrap_or(100) as usize,
            )?)?),
            "events" => Ok(serde_json::to_value(backend.events_after(
                value.get("cursor").and_then(Value::as_u64).unwrap_or(0),
            )?)?),
            "event_cursor" => Ok(json!({"cursor":backend.event_cursor()})),
            "snapshot_cursor" => {
                let (snapshot, cursor) = backend.snapshot_with_cursor()?;
                Ok(json!({"snapshot":snapshot,"cursor":cursor}))
            }
            "lyrics" => {
                let id = value
                    .get("id")
                    .and_then(Value::as_str)
                    .context("lyrics query requires id")?;
                let origin = value
                    .get("origin")
                    .filter(|v| !v.is_null())
                    .cloned()
                    .map(serde_json::from_value)
                    .transpose()?;
                Ok(serde_json::to_value(backend.lyrics(id, origin)?)?)
            }
            "lyrics_export" => {
                let id = value
                    .get("id")
                    .and_then(Value::as_str)
                    .context("lyrics_export query requires id")?;
                let format = value
                    .get("format")
                    .cloned()
                    .map(serde_json::from_value)
                    .transpose()?
                    .unwrap_or(lx_gpui_player::lyrics::service::LyricExportFormat::BundleJson);
                Ok(serde_json::to_value(backend.export_lyrics(id, format)?)?)
            }
            "provider_result" => Ok(serde_json::to_value(
                backend.provider_result(
                    value
                        .get("operation_id")
                        .and_then(Value::as_str)
                        .context("provider_result requires operation_id")?,
                )?,
            )?),
            _ => bail!("Unknown query: {query}"),
        };
    }
    let envelope: CommandEnvelope = serde_json::from_value(value)?;
    let tag_query = tag_completion_query(backend, &envelope)?;
    let effects_command = matches!(
        &envelope.command,
        BackendCommand::ImportImpulseResource { .. }
            | BackendCommand::RecoverImpulseImport { .. }
            | BackendCommand::RecoverImpulseNamespace { .. }
            | BackendCommand::AbortImpulseImport { .. }
            | BackendCommand::SetEffects { .. }
            | BackendCommand::ApplyBundledImpulsePreset { .. }
            | BackendCommand::ResetEffects
            | BackendCommand::RetireImpulseResource { .. }
            | BackendCommand::RecoverImpulseRetirement { .. }
    );
    let provider_request = matches!(&envelope.command, BackendCommand::ProviderRequest { .. });
    let deep_link = matches!(&envelope.command, BackendCommand::DispatchDeepLink { .. });
    let backup_export_id = match &envelope.command {
        BackendCommand::ExportBackup { .. } => Some(envelope.request_id.clone()),
        BackendCommand::RetryBackupExport {
            export_operation_id,
        }
        | BackendCommand::CleanupBackupExport {
            export_operation_id,
            ..
        } => Some(export_operation_id.clone()),
        _ => None,
    };
    let physical_snapshot_id = match &envelope.command {
        BackendCommand::ExportPhysicalSnapshot { .. } => Some(envelope.request_id.clone()),
        BackendCommand::RecoverPhysicalSnapshot {
            snapshot_operation_id,
            ..
        } => Some(snapshot_operation_id.clone()),
        _ => None,
    };
    let clear_audio_cache = matches!(&envelope.command, BackendCommand::ClearAudioCache);
    if wait_for_completion && effects_command {
        wait_for_effect_startup(backend)?;
    }
    let cursor = backend.event_cursor();
    let receipt = backend.execute(envelope)?;
    if wait_for_completion && receipt.state == ReceiptState::Accepted {
        let deadline = Instant::now()
            + Duration::from_secs(
                if provider_request
                    || deep_link
                    || backup_export_id.is_some()
                    || physical_snapshot_id.is_some()
                    || effects_command
                {
                    130
                } else {
                    80
                },
            );
        let mut cursor = cursor;
        let mut cache_statistics = None;
        loop {
            for event in backend.events_after(cursor)? {
                cursor = event.event_id;
                if let BackendEvent::AudioCacheCleared {
                    operation_id,
                    stats,
                } = &event.payload
                    && operation_id == &receipt.operation_id
                {
                    cache_statistics = Some(*stats);
                }
                if let BackendEvent::OperationFinished {
                    operation_id,
                    success,
                    error,
                } = event.payload
                    && operation_id == receipt.operation_id
                {
                    if effects_command {
                        return Ok(
                            json!({"receipt":receipt,"completed":true,"success":success,"error":error,
                            "effects":backend.query(BackendQuery::EffectsState)?,
                            "impulse_resources":backend.query(BackendQuery::ImpulseResources)?}),
                        );
                    }
                    if let Some(query) = &tag_query {
                        let result = backend.query(query.clone())?;
                        return Ok(
                            json!({"receipt":receipt,"completed":true,"success":success,"error":error,"tag_result":result}),
                        );
                    }
                    if let Some(snapshot_id) = &physical_snapshot_id {
                        let result = backend.query(BackendQuery::PhysicalSnapshot {
                            operation_id: snapshot_id.clone(),
                        })?;
                        return Ok(json!({"receipt":receipt,"completed":true,"success":success,
                            "error":error,"physical_snapshot":result}));
                    }
                    if let Some(export_id) = &backup_export_id {
                        let result = backend.query(BackendQuery::BackupExport {
                            operation_id: export_id.clone(),
                        })?;
                        return Ok(
                            json!({"receipt":receipt,"completed":true,"success":success,"error":error,"backup_export":result}),
                        );
                    }
                    if provider_request {
                        return Ok(
                            json!({"receipt":receipt,"completed":true,"provider_result":backend.provider_result(&receipt.operation_id)?}),
                        );
                    }
                    if deep_link {
                        let page = backend
                            .query(BackendQuery::DeepLinkResult {
                                operation_id: receipt.operation_id.clone(),
                                offset: 0,
                                limit: 100,
                            })
                            .ok();
                        ensure!(
                            success,
                            "Deep-link operation failed: {}",
                            error.as_ref().map(ToString::to_string).unwrap_or_default()
                        );
                        return Ok(
                            json!({"receipt":receipt,"completed":true,"deep_link_result":page,"snapshot":backend.snapshot()}),
                        );
                    }
                    ensure!(
                        success,
                        "Operation failed: {}",
                        error.map(|e| e.to_string()).unwrap_or_default()
                    );
                    if clear_audio_cache {
                        return Ok(
                            json!({"receipt":receipt,"completed":true,"audio_cache_stats":cache_statistics}),
                        );
                    }
                    return Ok(
                        json!({"receipt":receipt,"completed":true,"snapshot":backend.snapshot()}),
                    );
                }
            }
            ensure!(
                Instant::now() < deadline,
                "Operation completion deadline exceeded"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }
    if provider_request {
        return Ok(
            json!({"receipt":receipt,"provider_result":backend.provider_result(&receipt.operation_id)?}),
        );
    }
    if let Some(query) = tag_query {
        return Ok(json!({"receipt":receipt,"tag_result":backend.query(query)?}));
    }
    Ok(json!({"receipt":receipt,"snapshot":backend.snapshot()}))
}

fn tag_request_id() -> Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|error| anyhow::anyhow!("Cannot allocate tag read request ID: {error}"))?;
    Ok(format!(
        "cli-tag-read-{}-{}",
        std::process::id(),
        bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    ))
}
fn wait_for_effect_startup(backend: &BackendHandle) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(35);
    loop {
        let lx_gpui_player::backend::BackendQueryResult::EffectsState(state) =
            backend.query(BackendQuery::EffectsState)?
        else {
            bail!("Unexpected effect state response");
        };
        if !state.resource_preparing {
            return Ok(());
        }
        ensure!(
            Instant::now() < deadline,
            "Effect startup did not retire before command admission"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn print_tag_help() {
    println!(
        "Local tags: read-local-tags ASSET_ID TAG_OPERATION_ID, local-tags READ_REQUEST_ID, tag-edit TAG_OPERATION_ID, tag-edits, tag-write-approval APPROVAL_ID. Import a selected path with import-local PATH before reading its imported asset; the reader returns the actual file selection.\nJSON tag commands: read_local_tags, approve_tag_write, revoke_tag_write, approve_tag_recovery, edit_local_tags, recover_tag_edit, cancel_tag_edit. Write approval requires the selection returned by local_tags; read consent does not grant write consent. approve_tag_recovery requires the durable tag_operation_id and a fresh current selection for that operation.\nedit_local_tags requires approval_id, expected_approval_epoch, budget, patch. Patch fields title/artists/album/lyrics/cover use {{\"action\":\"keep\"}}, {{\"action\":\"remove\"}}, or {{\"action\":\"set\",\"value\":...}}; cover value is a byte array.\nrecover_tag_edit actions: resume_publication, restore_original, retire_artifacts. Cold recovery requires both fresh approval_id and expected_approval_epoch. Queries: local_tags(operation_id=read request ID), tag_write_approval(approval_id), tag_edit(operation_id=durable tag ID), tag_edits. A command acknowledgement is not a completed tag installation. JSON requests are limited to 2 MiB."
    );
}
fn tag_capabilities(mut value: Value) -> Value {
    value["commands"].as_array_mut().unwrap().extend(
        [
            "play_download_task",
            "set_effects",
            "apply_bundled_impulse_preset",
            "reset_effects",
            "import_impulse_resource",
            "recover_impulse_import",
            "recover_impulse_namespace",
            "abort_impulse_import",
            "retire_impulse_resource",
            "recover_impulse_retirement",
            "cancel_effects_operation",
        ]
        .into_iter()
        .map(Value::from),
    );
    value["queries"].as_array_mut().unwrap().extend(
        [
            "effects_settings",
            "effects_state",
            "bundled_impulse_presets",
            "impulse_resources",
            "effects_operations",
        ]
        .into_iter()
        .map(Value::from),
    );
    value["effects_protocol"] = json!({"import_formats":["wav","flac"],"max_encoded_bytes":8388608,
        "max_duration_seconds":4,"max_vault_bytes":67108864,"file_read_approval_required":true,
        "resource_identifiers_grant_file_access":false,"application_query":"effects_state"});
    value["commands"]
        .as_array_mut()
        .unwrap()
        .extend(TAG_COMMANDS.into_iter().map(Value::from));
    value["queries"]
        .as_array_mut()
        .unwrap()
        .extend(TAG_QUERIES.into_iter().map(Value::from));
    value["max_command_bytes"] = MAX_COMMAND_BYTES.into();
    value["local_tag_protocol"] = json!({"write_formats":["mp3","flac"],"read_requires_library_authorization":true,"write_requires_independent_approval":true,"cold_recovery_requires_bound_approval":true,"read_query":{"name":"local_tags","id_field":"operation_id","id_source":"read_request_id"},"edit_query":{"name":"tag_edit","id_field":"operation_id","id_source":"durable_tag_operation_id"},"field_actions":["keep","remove","set"],"cover_wire_format":"byte_array"});
    value
}
fn tag_completion_query(
    backend: &BackendHandle,
    envelope: &CommandEnvelope,
) -> Result<Option<BackendQuery>> {
    Ok(match &envelope.command {
        BackendCommand::ReadLocalTags { .. } => Some(BackendQuery::LocalTags {
            operation_id: envelope.request_id.clone(),
        }),
        BackendCommand::ApproveTagWrite { approval_id, .. }
        | BackendCommand::RevokeTagWrite { approval_id, .. }
        | BackendCommand::ApproveTagRecovery { approval_id, .. } => {
            Some(BackendQuery::TagWriteApproval {
                approval_id: approval_id.clone(),
            })
        }
        BackendCommand::EditLocalTags { approval_id, .. } => {
            match backend.query(BackendQuery::TagWriteApproval {
                approval_id: approval_id.clone(),
            })? {
                BackendQueryResult::TagWriteApproval(Some(approval)) => {
                    Some(BackendQuery::TagEdit {
                        operation_id: approval.selection.operation_id,
                    })
                }
                _ => None,
            }
        }
        BackendCommand::RecoverTagEdit {
            tag_operation_id, ..
        }
        | BackendCommand::CancelTagEdit { tag_operation_id } => Some(BackendQuery::TagEdit {
            operation_id: tag_operation_id.clone(),
        }),
        _ => None,
    })
}
fn validate_tag_request(value: &Value) -> Result<()> {
    validate_effect_request(value)?;
    if value.get("command").and_then(Value::as_str) == Some("play_download_task") {
        ensure!(
            value
                .as_object()
                .is_some_and(|object| object.keys().all(|key| [
                    "request_id",
                    "command",
                    "origin",
                    "expected_revision",
                    "task_id"
                ]
                .contains(&key.as_str()))),
            "Unknown download playback command field"
        );
        let task_id = value
            .get("task_id")
            .and_then(Value::as_str)
            .context("Download task ID missing")?;
        lx_gpui_player::domain::library::validate_library_id(task_id)?;
    }
    use lx_gpui_player::domain::library::{validate_library_id, validate_library_path};
    let command = value.get("command").and_then(Value::as_str);
    let query = value.get("query").and_then(Value::as_str);
    if let Some(query) = query.filter(|name| TAG_QUERIES.contains(name)) {
        let fields: &[&str] = match query {
            "tag_edits" => &["query"],
            "tag_write_approval" => &["query", "approval_id"],
            _ => &["query", "operation_id"],
        };
        ensure!(
            value
                .as_object()
                .unwrap()
                .keys()
                .all(|key| fields.contains(&key.as_str())),
            "Unknown local-tag query field"
        );
        let query: BackendQuery = serde_json::from_value(value.clone())?;
        match query {
            BackendQuery::LocalTags { operation_id } | BackendQuery::TagEdit { operation_id } => {
                validate_library_id(&operation_id)?
            }
            BackendQuery::TagWriteApproval { approval_id } => validate_library_id(&approval_id)?,
            _ => {}
        }
    }
    if let Some(command) = command.filter(|name| TAG_COMMANDS.contains(name)) {
        let fields: &[&str] = match command {
            "read_local_tags" => &["asset_id", "tag_operation_id"],
            "approve_tag_write" => &["approval_id", "selection"],
            "approve_tag_recovery" => &["approval_id", "tag_operation_id", "selection"],
            "revoke_tag_write" => &["approval_id", "expected_approval_epoch"],
            "edit_local_tags" => &["approval_id", "expected_approval_epoch", "budget", "patch"],
            "recover_tag_edit" => &[
                "tag_operation_id",
                "action",
                "approval_id",
                "expected_approval_epoch",
            ],
            _ => &["tag_operation_id"],
        };
        ensure!(
            value.as_object().unwrap().keys().all(|key| [
                "request_id",
                "command",
                "origin",
                "expected_revision"
            ]
            .contains(&key.as_str())
                || fields.contains(&key.as_str())),
            "Unknown local-tag command field"
        );
        let envelope: CommandEnvelope = serde_json::from_value(value.clone())?;
        match envelope.command {
            BackendCommand::ReadLocalTags {
                asset_id,
                tag_operation_id,
            } => {
                validate_library_id(&asset_id)?;
                validate_library_id(&tag_operation_id)?;
            }
            BackendCommand::ApproveTagWrite {
                approval_id,
                selection,
            }
            | BackendCommand::ApproveTagRecovery {
                approval_id,
                selection,
                ..
            } => {
                validate_library_id(&approval_id)?;
                validate_library_id(&selection.operation_id)?;
                validate_library_id(&selection.asset_id)?;
                validate_library_path(&selection.path)?;
                selection.expected_identity.validate()?;
                ensure!(
                    selection
                        .expected_body_sha256
                        .as_ref()
                        .is_some_and(|hash| hash.len() == 64
                            && hash.bytes().all(|byte| byte.is_ascii_hexdigit())),
                    "Tag approval requires the actual selected full body SHA256"
                );
                if command == "approve_tag_recovery" {
                    ensure!(
                        value["tag_operation_id"].as_str() == Some(selection.operation_id.as_str()),
                        "Recovery approval must bind the selected durable tag operation"
                    );
                }
            }
            BackendCommand::RevokeTagWrite {
                approval_id,
                expected_approval_epoch,
            } => {
                validate_library_id(&approval_id)?;
                ensure!(
                    expected_approval_epoch > 0,
                    "Approval epoch must be positive"
                );
            }
            BackendCommand::EditLocalTags {
                approval_id,
                expected_approval_epoch,
                budget,
                patch,
            } => {
                validate_library_id(&approval_id)?;
                ensure!(
                    expected_approval_epoch > 0,
                    "Approval epoch must be positive"
                );
                budget.validate()?;
                validate_tag_patch(&patch)?;
            }
            BackendCommand::RecoverTagEdit {
                tag_operation_id,
                approval_id,
                expected_approval_epoch,
                ..
            } => {
                validate_library_id(&tag_operation_id)?;
                ensure!(
                    approval_id.is_some() == expected_approval_epoch.is_some(),
                    "Recovery approval ID and epoch must be provided together"
                );
                if let Some(id) = approval_id {
                    validate_library_id(&id)?;
                    ensure!(
                        expected_approval_epoch.is_some_and(|epoch| epoch > 0),
                        "Recovery approval epoch must be positive"
                    );
                }
            }
            BackendCommand::CancelTagEdit { tag_operation_id } => {
                validate_library_id(&tag_operation_id)?
            }
            _ => {}
        }
    }
    Ok(())
}

fn validate_effect_request(value: &Value) -> Result<()> {
    let query = value.get("query").and_then(Value::as_str);
    if query.is_some_and(|query| {
        [
            "effects_settings",
            "effects_state",
            "bundled_impulse_presets",
            "impulse_resources",
            "effects_operations",
        ]
        .contains(&query)
    }) {
        ensure!(
            value.as_object().is_some_and(|fields| fields.len() == 1),
            "Unknown effect query field"
        );
        return Ok(());
    }
    let Some(command) = value.get("command").and_then(Value::as_str) else {
        return Ok(());
    };
    let fields: &[&str] = match command {
        "set_effects" => &["settings"],
        "apply_bundled_impulse_preset" => &["id"],
        "reset_effects" => &[],
        "import_impulse_resource" => &["path", "file_read_approved", "timeout_ms"],
        "recover_impulse_import" => &["token", "timeout_ms"],
        "recover_impulse_namespace" => &["selection", "timeout_ms"],
        "abort_impulse_import" => &["token", "selection"],
        "retire_impulse_resource" => &["resource_id"],
        "recover_impulse_retirement" => &["token"],
        "cancel_effects_operation" => &["operation_id"],
        _ => return Ok(()),
    };
    ensure!(
        value
            .as_object()
            .is_some_and(|object| object.keys().all(|key| [
                "request_id",
                "command",
                "origin",
                "expected_revision"
            ]
            .contains(&key.as_str())
                || fields.contains(&key.as_str()))),
        "Unknown effect command field"
    );
    let envelope: CommandEnvelope = serde_json::from_value(value.clone())?;
    match envelope.command {
        BackendCommand::ImportImpulseResource {
            path,
            file_read_approved,
            timeout_ms,
        } => {
            ensure!(
                file_read_approved,
                "Impulse import requires explicit file-read approval"
            );
            lx_gpui_player::domain::library::validate_library_path(&path)?;
            ensure!(
                path.extension()
                    .and_then(|extension| extension.to_str())
                    .is_some_and(|extension| matches!(
                        extension.to_ascii_lowercase().as_str(),
                        "wav" | "flac"
                    )),
                "Unsupported impulse format"
            );
            ensure!(
                timeout_ms.is_none_or(|timeout| (1..=120000).contains(&timeout)),
                "Invalid impulse timeout"
            );
        }
        BackendCommand::SetEffects { settings } => settings.validate()?,
        BackendCommand::ApplyBundledImpulsePreset { id } => {
            lx_gpui_player::audio::bundled_impulses::preset(&id)?;
        }
        BackendCommand::RetireImpulseResource { resource_id } => resource_id.validate()?,
        BackendCommand::RecoverImpulseImport { token, timeout_ms } => {
            validate_impulse_token(&token)?;
            ensure!(
                timeout_ms.is_none_or(|timeout| (1..=120000).contains(&timeout)),
                "Invalid impulse timeout"
            );
        }
        BackendCommand::RecoverImpulseRetirement { token } => validate_impulse_token(&token)?,
        BackendCommand::RecoverImpulseNamespace {
            selection,
            timeout_ms,
        } => {
            selection.validate()?;
            ensure!(
                timeout_ms.is_none_or(|timeout| (1..=120000).contains(&timeout)),
                "Invalid impulse timeout"
            );
        }
        BackendCommand::AbortImpulseImport { token, selection } => {
            validate_impulse_token(&token)?;
            if let Some(selection) = selection {
                selection.validate()?;
            }
        }
        BackendCommand::CancelEffectsOperation { operation_id } => {
            lx_gpui_player::domain::library::validate_library_id(&operation_id)?
        }
        _ => {}
    }
    Ok(())
}
fn validate_impulse_token(token: &str) -> Result<()> {
    ensure!(
        token.len() == 32
            && token
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "Invalid impulse inventory identity"
    );
    Ok(())
}
fn validate_tag_patch(patch: &LocalTagEditPatch) -> Result<()> {
    patch.validate()?;
    let mut bytes = 0usize;
    for field in [&patch.title, &patch.album, &patch.lyrics] {
        if let TagEditField::Set(text) = field {
            ensure!(
                !text.is_empty(),
                "Set tag text must be nonempty; use remove to clear it"
            );
            bytes = bytes
                .checked_add(text.len())
                .context("Tag text length overflow")?;
        }
    }
    if let TagEditField::Set(artists) = &patch.artists {
        ensure!(
            !artists.is_empty() && artists.iter().all(|artist| !artist.is_empty()),
            "Set artists must be nonempty; use remove to clear them"
        );
        for artist in artists {
            bytes = bytes
                .checked_add(artist.len())
                .context("Tag text length overflow")?;
        }
    }
    ensure!(bytes <= 1024 * 1024, "Native tag text exceeds 1 MiB");
    Ok(())
}
