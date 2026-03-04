use openaction::*;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, OnceLock};
use std::sync::atomic::{AtomicU16, Ordering};
use std::path::Path;
use tokio::sync::Mutex;
use tokio::time::{sleep, Duration};
use tokio::fs;
use tokio_tungstenite::connect_async;
use futures_util::SinkExt;
use chrono::Timelike;


const VERSION: &str = "1.4.0";

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------
#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct SleepSettings {
    // Day settings
    pub normal_brightness: u8,
    pub dim_brightness:    u8,
    pub dim_after_secs:    u64,
    pub off_after_secs:    u64,
    pub no_off:            bool,

    // Night mode
    pub night_enabled:            bool,
    pub night_start_hour:         u8,
    pub night_start_min:          u8,
    pub night_end_hour:           u8,
    pub night_end_min:            u8,
    pub night_normal_brightness:  u8,
    pub night_dim_brightness:     u8,
    pub night_dim_after_secs:     u64,
    pub night_off_after_secs:     u64,
    pub night_no_off:             bool,
}

impl Default for SleepSettings {
    fn default() -> Self {
        Self {
            normal_brightness:           70,
            dim_brightness:              20,
            dim_after_secs:              60,
            off_after_secs:             120,
            no_off:                    false,
            night_enabled:             false,
            night_start_hour:             22,
            night_start_min:               0,
            night_end_hour:                7,
            night_end_min:                 0,
            night_normal_brightness:      30,
            night_dim_brightness:          5,
            night_dim_after_secs:         30,
            night_off_after_secs:         60,
            night_no_off:              false,
        }
    }
}

// ---------------------------------------------------------------------------
// Global state
// ---------------------------------------------------------------------------
#[derive(Clone, PartialEq, Debug)]
enum Phase { Awake, Dim, Off }

struct SleepState {
    last_activity:   std::time::Instant,
    phase:           Phase,
    settings:        SleepSettings,
    timer_started:   bool,
    enabled:         bool,
    context:         String,
    last_brightness: u8,   // track last applied value to detect day↔night transitions
}

static SLEEP_STATE: OnceLock<Arc<Mutex<SleepState>>> = OnceLock::new();
static WS_PORT:     AtomicU16                        = AtomicU16::new(0);
static HID_PATH:    OnceLock<String>                 = OnceLock::new();

fn global_state() -> Arc<Mutex<SleepState>> {
    SLEEP_STATE.get_or_init(|| {
        Arc::new(Mutex::new(SleepState {
            last_activity:   std::time::Instant::now(),
            phase:           Phase::Awake,
            settings:        SleepSettings::default(),
            timer_started:   false,
            enabled:         true,
            context:         String::new(),
            last_brightness: 255, // sentinel → force first apply
        }))
    }).clone()
}

// ---------------------------------------------------------------------------
// Night-time helper
// ---------------------------------------------------------------------------
fn is_night_time(start_h: u8, start_m: u8, end_h: u8, end_m: u8) -> bool {
    let now  = chrono::Local::now();
    let cur  = now.hour() as u16 * 60 + now.minute() as u16;
    let sta  = start_h as u16 * 60 + start_m as u16;
    let end  = end_h   as u16 * 60 + end_m   as u16;
    if sta <= end {
        cur >= sta && cur < end
    } else {
        // crosses midnight, e.g. 22:30 → 07:15
        cur >= sta || cur < end
    }
}

// ---------------------------------------------------------------------------
// Effective settings for current time of day
// ---------------------------------------------------------------------------
struct EffSettings {
    normal_br: u8,
    dim_br:    u8,
    dim_at:    u64,
    off_at:    u64,
    no_off:    bool,
    is_night:  bool,
}

fn effective(s: &SleepSettings) -> EffSettings {
    let is_night = s.night_enabled && is_night_time(
        s.night_start_hour, s.night_start_min, s.night_end_hour, s.night_end_min);
    if is_night {
        EffSettings {
            normal_br: s.night_normal_brightness,
            dim_br:    s.night_dim_brightness,
            dim_at:    s.night_dim_after_secs,
            off_at:    s.night_dim_after_secs + s.night_off_after_secs,
            no_off:    s.night_no_off,
            is_night:  true,
        }
    } else {
        EffSettings {
            normal_br: s.normal_brightness,
            dim_br:    s.dim_brightness,
            dim_at:    s.dim_after_secs,
            off_at:    s.dim_after_secs + s.off_after_secs,
            no_off:    s.no_off,
            is_night:  false,
        }
    }
}

// ---------------------------------------------------------------------------
// Brightness via spoofed UUID (whitelist bypass)
// ---------------------------------------------------------------------------
async fn set_brightness(value: u8) {
    let port = WS_PORT.load(Ordering::Relaxed);
    if port == 0 { return; }

    let url = format!("ws://127.0.0.1:{}", port);
    if let Ok((mut ws, _)) = connect_async(&url).await {
        let reg = serde_json::json!({
            "event": "registerPlugin",
            "uuid":  "opendeck_alternative_elgato_implementation"
        });
        let _ = ws.send(tungstenite::Message::Text(reg.to_string().into())).await;
        sleep(Duration::from_millis(50)).await;
        let cmd = serde_json::json!({
            "event":  "deviceBrightness",
            "action": "set",
            "value":  value
        });
        let _ = ws.send(tungstenite::Message::Text(cmd.to_string().into())).await;
        let _ = ws.close(None).await;
    }
}

// ---------------------------------------------------------------------------
// HID sleep: sends the "HAN" command directly to the raw device
// This actually powers off the display (unlike brightness=0 which only dims)
// ---------------------------------------------------------------------------
async fn send_hid_sleep() {
    use tokio::io::AsyncWriteExt;
    let path = match HID_PATH.get() { Some(p) => p.as_str(), None => return };
    // 513-byte buffer: report ID 0x00 + "CRT\x00\x00HAN" + zero padding
    let mut buf = vec![0u8; 513];
    buf[1] = 0x43; // C
    buf[2] = 0x52; // R
    buf[3] = 0x54; // T
    buf[6] = 0x48; // H
    buf[7] = 0x41; // A
    buf[8] = 0x4E; // N
    if let Ok(mut f) = tokio::fs::OpenOptions::new().write(true).open(path).await {
        let _ = f.write_all(&buf).await;
    }
}

// ---------------------------------------------------------------------------
// Debug status file
// ---------------------------------------------------------------------------
fn fmt_mmss(secs: u64) -> String {
    let m = secs / 60;
    let s = secs % 60;
    if m > 0 { format!("{m}m {s:02}s") } else { format!("{s}s") }
}

fn write_status(phase: &Phase, enabled: bool, no_off: bool, is_night: bool,
                elapsed: u64, dim_at: u64, off_at: u64) {
    let phase_str   = match phase { Phase::Awake => "Awake", Phase::Dim => "Dimmed", Phase::Off => "Off" };
    let enabled_str = if enabled { "ON" } else { "OFF" };
    let mode_str    = if is_night { " [Night]" } else { "" };
    let until_dim   = if elapsed < dim_at { dim_at - elapsed } else { 0 };
    let until_off   = if elapsed < off_at { off_at - elapsed } else { 0 };
    let off_str     = if no_off { "–".into() } else { fmt_mmss(until_off) };

    let _ = std::fs::write("/tmp/sleepmode_status", format!(
        "Sleep Mode: {enabled_str}{mode_str}  |  Phase: {phase_str}  |  Idle: {}\n\
         Dim in: {}  |  Off in: {}  |  v{VERSION}\n",
        fmt_mmss(elapsed), fmt_mmss(until_dim), off_str,
    ));
}

// ---------------------------------------------------------------------------
// Update button title + state
// ---------------------------------------------------------------------------
async fn update_button(phase: &Phase, enabled: bool) {
    let ctx = { global_state().lock().await.context.clone() };
    if ctx.is_empty() { return; }

    let (title, state): (&str, u16) = match (enabled, phase) {
        (false, _)           => ("Inactive", 0),
        (true, Phase::Awake) => ("Active",   0),
        (true, Phase::Dim)   => ("Dim",      1),
        (true, Phase::Off)   => ("Off",      2),
    };

    let _ = send_arbitrary_json(serde_json::json!({
        "event": "setTitle", "context": ctx,
        "payload": { "title": title, "target": 0 }
    })).await;
    let _ = send_arbitrary_json(serde_json::json!({
        "event": "setState", "context": ctx,
        "payload": { "state": state }
    })).await;
}

// ---------------------------------------------------------------------------
// Send status to Property Inspector
// ---------------------------------------------------------------------------
async fn send_pi_status(phase: &Phase, enabled: bool) {
    let (ctx, elapsed, eff) = {
        let arc = global_state();
        let s   = arc.lock().await;
        let eff = effective(&s.settings);
        (s.context.clone(), s.last_activity.elapsed().as_secs(), eff)
    };
    if ctx.is_empty() { return; }

    let status = match (enabled, phase) {
        (false, _)           => "Disabled",
        (true, Phase::Awake) => "Active",
        (true, Phase::Dim)   => "Dimmed",
        (true, Phase::Off)   => "Display off",
    };
    let until_dim = if enabled && elapsed < eff.dim_at { fmt_mmss(eff.dim_at - elapsed) } else { "–".into() };
    let until_off = if enabled && !eff.no_off && elapsed < eff.off_at { fmt_mmss(eff.off_at - elapsed) } else { "–".into() };

    let _ = send_arbitrary_json(serde_json::json!({
        "event":   "sendToPropertyInspector",
        "context": ctx,
        "action":  "com.rainer.sleepmode.toggle",
        "payload": {
            "status":     status,
            "until_dim":  until_dim,
            "until_off":  until_off,
            "night_active": eff.is_night,
            "version":    VERSION
        }
    })).await;
}

// ---------------------------------------------------------------------------
// Background timer
// ---------------------------------------------------------------------------
async fn timer_task() {
    loop {
        sleep(Duration::from_secs(1)).await;

        let (phase, enabled, elapsed, eff, last_br) = {
            let arc = global_state();
            let s   = arc.lock().await;
            let elapsed = s.last_activity.elapsed().as_secs();
            let eff     = effective(&s.settings);
            (s.phase.clone(), s.enabled, elapsed, eff, s.last_brightness)
        };

        write_status(&phase, enabled, eff.no_off, eff.is_night, elapsed, eff.dim_at, eff.off_at);
        send_pi_status(&phase, enabled).await;

        if !enabled { continue; }

        let target = if !eff.no_off && elapsed >= eff.off_at {
            Phase::Off
        } else if elapsed >= eff.dim_at {
            Phase::Dim
        } else {
            Phase::Awake
        };

        let target_br = match &target {
            Phase::Awake => eff.normal_br,
            Phase::Dim   => eff.dim_br,
            Phase::Off   => 0,
        };

        // Apply if phase changed OR brightness changed (e.g. day→night transition)
        if target != phase || target_br != last_br {
            {
                let arc = global_state();
                let mut s = arc.lock().await;
                s.phase          = target.clone();
                s.last_brightness = target_br;
            }
            if target == Phase::Off {
                send_hid_sleep().await;
            } else {
                set_brightness(target_br).await;
            }
            update_button(&target, true).await;
        }
    }
}

// ---------------------------------------------------------------------------
// Find Stream Deck: Enumerate devices and find the stream deck via USB
// ---------------------------------------------------------------------------
async fn find_deck_device() -> Option<String> {
    let by_id_path = "/dev/input/by-id";
    let mut entries = fs::read_dir(by_id_path).await.ok()?;

    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        if name_str.contains("Elgato_Stream_Deck") && name_str.contains("hidraw") {
            let full_path = Path::new(by_id_path).join(&name);
            if let Ok(resolved) = fs::canonicalize(&full_path).await {
                let resolved_str = resolved.to_string_lossy().into_owned();
                let _ = HID_PATH.set(resolved_str.clone());
                return Some(resolved_str);
            }
        }
    }

    None
}

// ---------------------------------------------------------------------------
// HID monitor: reset timer on any deck button press
// ---------------------------------------------------------------------------
async fn hid_watcher_task() {
    use tokio::io::AsyncReadExt;

    let device_path = match find_deck_device().await {
        Some(p) => p,
        None => {
            eprintln!("No Deck found in /dev/input/by-id");
            return;
        }
    };

    let mut f = match tokio::fs::OpenOptions::new()
    .read(true)
    .open(&device_path)
    .await
    {
        Ok(f) => f,
        Err(e) => {
            eprintln!("Failed to open {device_path}: {e}");
            return;
        }
    };

    let mut buf = [0u8; 64];
    loop {
        match f.read(&mut buf).await {
            Ok(n) if n > 0 => {
                let arc = global_state();
                let mut s = arc.lock().await;
                if s.enabled {
                    s.last_activity = std::time::Instant::now();
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
}

// ---------------------------------------------------------------------------
// Action
// ---------------------------------------------------------------------------
pub struct SleepModeAction;

#[async_trait]
impl Action for SleepModeAction {
    const UUID: &'static str = "com.rainer.sleepmode.toggle";
    type Settings = SleepSettings;

    async fn will_appear(&self, instance: &Instance, settings: &Self::Settings) -> OpenActionResult<()> {
        let arc = global_state();
        let mut s = arc.lock().await;
        s.context  = instance.instance_id.to_string();
        s.settings = settings.clone();
        let (phase, enabled) = (s.phase.clone(), s.enabled);
        drop(s);
        update_button(&phase, enabled).await;
        send_pi_status(&phase, enabled).await;
        Ok(())
    }

    async fn did_receive_settings(&self, _instance: &Instance, settings: &Self::Settings) -> OpenActionResult<()> {
        let arc = global_state();
        let mut s = arc.lock().await;
        s.settings = settings.clone();
        s.last_brightness = 255; // force re-apply on next tick
        let (phase, enabled) = (s.phase.clone(), s.enabled);
        drop(s);
        if enabled {
            let eff = effective(settings);
            let br = match &phase { Phase::Awake => eff.normal_br, Phase::Dim => eff.dim_br, Phase::Off => 0 };
            set_brightness(br).await;
            global_state().lock().await.last_brightness = br;
        }
        send_pi_status(&phase, enabled).await;
        Ok(())
    }

    async fn property_inspector_did_appear(&self, instance: &Instance, _settings: &Self::Settings) -> OpenActionResult<()> {
        let arc = global_state();
        let mut s = arc.lock().await;
        s.context = instance.instance_id.to_string();
        let (phase, enabled) = (s.phase.clone(), s.enabled);
        drop(s);
        send_pi_status(&phase, enabled).await;
        Ok(())
    }

    async fn key_up(&self, _instance: &Instance, settings: &Self::Settings) -> OpenActionResult<()> {
        let arc = global_state();
        let mut s = arc.lock().await;
        s.settings = settings.clone();

        if s.enabled && s.phase != Phase::Awake {
            s.last_activity  = std::time::Instant::now();
            s.phase          = Phase::Awake;
            let eff          = effective(settings);
            s.last_brightness = eff.normal_br;
            drop(s);
            set_brightness(eff.normal_br).await;
            update_button(&Phase::Awake, true).await;
            send_pi_status(&Phase::Awake, true).await;
        } else {
            s.enabled = !s.enabled;
            if s.enabled {
                s.last_activity  = std::time::Instant::now();
                s.phase          = Phase::Awake;
                let eff          = effective(settings);
                s.last_brightness = eff.normal_br;
                drop(s);
                set_brightness(eff.normal_br).await;
                update_button(&Phase::Awake, true).await;
                send_pi_status(&Phase::Awake, true).await;
            } else {
                let eff = effective(settings);
                s.last_brightness = eff.normal_br;
                drop(s);
                set_brightness(eff.normal_br).await;
                update_button(&Phase::Awake, false).await;
                send_pi_status(&Phase::Awake, false).await;
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------
#[tokio::main]
async fn main() -> OpenActionResult<()> {
    let args: Vec<String> = std::env::args().collect();

    if let Some(pos) = args.iter().position(|a| a == "-port") {
        if let Some(port_str) = args.get(pos + 1) {
            if let Ok(port) = port_str.parse::<u16>() {
                WS_PORT.store(port, Ordering::Relaxed);
            }
        }
    }

    {
        let arc = global_state();
        let mut s = arc.lock().await;
        if !s.timer_started {
            s.timer_started = true;
            drop(s);
            tokio::spawn(async {
                sleep(Duration::from_millis(500)).await;
                let eff = effective(&SleepSettings::default());
                set_brightness(eff.normal_br).await;
            });
            tokio::spawn(timer_task());
            tokio::spawn(hid_watcher_task());
        }
    }

    register_action(SleepModeAction).await;
    run(args).await
}
