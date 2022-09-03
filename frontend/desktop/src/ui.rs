#[macro_use]
pub mod utils;
mod config_editor;
use config_editor::Editor as ConfigEditor;

#[allow(dead_code)]
pub mod imgui_wgpu;
#[cfg(feature = "log")]
mod log;
pub mod window;

#[cfg(feature = "debug-views")]
use crate::debug_views;
use crate::{
    audio,
    config::{self, Launch},
    emu, game_db, input, triple_buffer,
    utils::{config_base, Lazy},
    DsSlotRom, FrameData,
};
#[cfg(feature = "xq-audio")]
use dust_core::{
    ds_slot::rom::Contents,
    gpu::{Framebuffer, SCREEN_HEIGHT, SCREEN_WIDTH},
    utils::zeroed_box,
};
#[cfg(feature = "log")]
use log::Log;
use rfd::FileDialog;
#[cfg(feature = "gdb-server")]
use std::net::SocketAddr;
#[cfg(feature = "xq-audio")]
use std::num::NonZeroU32;
#[cfg(feature = "discord-presence")]
use std::time::SystemTime;
use std::{
    env,
    fmt::Write,
    fs, io, panic,
    path::{Path, PathBuf},
    slice,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
};
use utils::scale_to_fit_rotated;

#[cfg(feature = "xq-audio")]
fn adjust_custom_sample_rate(sample_rate: Option<NonZeroU32>) -> Option<NonZeroU32> {
    sample_rate.map(|sample_rate| {
        NonZeroU32::new((sample_rate.get() as f64 / audio::SAMPLE_RATE_ADJUSTMENT_RATIO) as u32)
            .unwrap_or(NonZeroU32::new(1).unwrap())
    })
}

struct EmuState {
    playing: bool,
    title: String,
    game_loaded: bool,
    save_path_update: Option<emu::SavePathUpdate>,
    #[cfg(feature = "gdb-server")]
    gdb_server_addr: Option<SocketAddr>,

    thread: thread::JoinHandle<triple_buffer::Sender<FrameData>>,

    shared_state: Arc<emu::SharedState>,
    from_emu: crossbeam_channel::Receiver<emu::Notification>,
    to_emu: crossbeam_channel::Sender<emu::Message>,
}

impl EmuState {
    fn send_message(&self, msg: emu::Message) {
        self.to_emu
            .send(msg)
            .expect("couldn't send message to emulation thread");
    }
}

struct Config {
    games_base_path: Option<PathBuf>,
    config: config::Config,
    global_path: Option<PathBuf>,
    game_path: Option<PathBuf>,
    input_map: config::File<input::Map>,
}

impl Config {
    fn new() -> Self {
        let base_path = config_base();
        let games_base_path = base_path.join("games");
        let (base_path, games_base_path) = if let Err(err) = fs::create_dir_all(&games_base_path) {
            config_error!(
                "Couldn't create the configuration directory{}: {}\n\nThe default configuration \
                 will be used, new changes will not be saved.",
                location_str!(games_base_path),
                err,
            );
            (None, None)
        } else {
            (Some(base_path.to_path_buf()), Some(games_base_path))
        };

        let global = base_path
            .as_ref()
            .map(|base_path| {
                config::File::<config::Global>::read_or_show_dialog(base_path, "global_config.json")
            })
            .unwrap_or_default();

        let input_map = base_path
            .as_ref()
            .map(|base_path| {
                config::File::<input::Map>::read_or_show_dialog(base_path, "input_map.json")
            })
            .unwrap_or_default();

        Config {
            games_base_path,
            config: config::Config::from_global(&global.contents),
            global_path: global.path,
            game_path: None,
            input_map,
        }
    }
}

#[cfg(feature = "discord-presence")]
struct DiscordPresence {
    rpc_connection: discord_rpc::Rpc,
    presence: discord_rpc::Presence,
    updated: bool,
}

#[cfg(feature = "discord-presence")]
impl DiscordPresence {
    fn new() -> Self {
        DiscordPresence {
            rpc_connection: discord_rpc::Rpc::new(
                "914286657849667645".to_string(),
                Default::default(),
                false,
            ),
            presence: Default::default(),
            updated: false,
        }
    }

    fn start(&mut self, title: &str) {
        self.presence.state = Some(format!("Playing {title}"));
        self.presence.timestamps = Some(discord_rpc::Timestamps {
            start: Some(SystemTime::now()),
            end: None,
        });
        self.updated = true;
    }

    fn stop(&mut self) {
        self.presence.state = Some("Not playing anything".to_string());
        self.presence.timestamps = Some(discord_rpc::Timestamps {
            start: Some(SystemTime::now()),
            end: None,
        });
        self.updated = true;
    }

    fn flush(&mut self) {
        if !self.updated {
            return;
        }
        self.updated = false;
        self.rpc_connection.update_presence(Some(&self.presence));
    }
}

#[cfg(feature = "discord-presence")]
impl Drop for DiscordPresence {
    fn drop(&mut self) {
        self.rpc_connection.update_presence(None);
    }
}

pub struct UiState {
    game_db: Lazy<Option<game_db::Database>>,

    emu: Option<EmuState>,

    fb_texture: FbTexture,
    frame_tx: Option<triple_buffer::Sender<FrameData>>,
    frame_rx: triple_buffer::Receiver<FrameData>,
    fps_fixed: Option<u64>,

    show_menu_bar: bool,
    screen_focused: bool,

    input: input::State,
    input_map_editor: Option<input::MapEditor>,

    config_editor: Option<ConfigEditor>,

    audio_channel: Option<audio::Channel>,

    #[cfg(target_os = "windows")]
    icon_update: Option<Option<[u32; 32 * 32]>>,

    #[cfg(feature = "log")]
    log: Log,

    #[cfg(feature = "debug-views")]
    debug_views: debug_views::UiState,

    #[cfg(feature = "discord-presence")]
    discord_presence: Option<DiscordPresence>,

    editing_save_slot_i: Option<usize>,
}

static ALLOWED_ROM_EXTENSIONS: &[&str] = &["nds", "bin"];

impl UiState {
    fn play_pause(&mut self) {
        if let Some(emu) = &mut self.emu {
            emu.playing = !emu.playing;
            emu.shared_state
                .playing
                .store(emu.playing, Ordering::Relaxed);
        }
    }

    fn reset(&mut self) {
        if let Some(emu) = &mut self.emu {
            emu.send_message(emu::Message::Reset);
        }
    }
}

impl UiState {
    fn load_from_rom_path(&mut self, path: &Path, config: &mut Config, window: &window::Window) {
        if let Some(extension) = path.extension().and_then(|s| s.to_str()) {
            if !ALLOWED_ROM_EXTENSIONS.contains(&extension) {
                return;
            }
        } else {
            return;
        }

        self.stop(config, window);

        let game_title = path
            .file_stem()
            .unwrap()
            .to_str()
            .expect("non-UTF-8 ROM filename provided");

        let game_config: config::File<config::Game> = config
            .games_base_path
            .as_ref()
            .map(|base_path| {
                config::File::read_or_show_dialog(base_path, &format!("{game_title}.json"))
            })
            .unwrap_or_default();

        config.config.deserialize_game(&game_config.contents);

        let ds_slot_rom =
            DsSlotRom::new(path, config!(config.config, ds_slot_rom_in_memory_max_size))
                .expect("couldn't load the specified ROM file");

        match config::Launch::new(&config.config, false) {
            Ok((launch_config, warnings)) => {
                if !warnings.is_empty() {
                    config_warning!("{}", format_list!(warnings));
                }
                self.start(
                    config,
                    launch_config,
                    config.config.save_path(game_title),
                    game_title.to_string(),
                    Some(ds_slot_rom),
                );
                config.game_path = game_config.path;
            }

            Err(errors) => {
                config.config.unset_game();
                config_error!(
                    "Couldn't determine final configuration for game: {}",
                    format_list!(errors)
                );
            }
        }
    }

    fn load_firmware(&mut self, config: &mut Config, window: &window::Window) {
        self.stop(config, window);
        match config::Launch::new(&config.config, true) {
            Ok((launch_config, warnings)) => {
                if !warnings.is_empty() {
                    config_warning!("{}", format_list!(warnings));
                }
                self.start(config, launch_config, None, "Firmware".to_string(), None);
            }

            Err(errors) => {
                config_error!(
                    "Couldn't determine final configuration for firmware: {}",
                    format_list!(errors)
                );
            }
        }
    }

    fn start(
        &mut self,
        config: &mut Config,
        launch_config: Launch,
        save_path: Option<PathBuf>,
        title: String,
        ds_slot_rom: Option<DsSlotRom>,
    ) {
        #[cfg(feature = "discord-presence")]
        if let Some(presence) = &mut self.discord_presence {
            presence.start(&title);
        }

        let playing = !config!(config.config, pause_on_launch);
        let game_loaded = ds_slot_rom.is_some();

        #[cfg(feature = "log")]
        let logger = self.log.logger().clone();

        #[allow(unused_mut, clippy::bind_instead_of_map)]
        let ds_slot = ds_slot_rom.and_then(|mut rom| {
            #[cfg(target_os = "windows")]
            {
                use dust_core::{ds_slot, utils::Bytes};
                let mut header_bytes = Bytes::new([0; 0x170]);
                rom.read_header(&mut header_bytes);
                let header = ds_slot::rom::header::Header::new(header_bytes.as_byte_slice())?;
                let icon_title_offset = header.icon_title_offset();
                self.icon_update = Some(ds_slot::rom::icon::decode(icon_title_offset, &mut rom));
            }

            let game_code = rom.game_code();

            let save_type = self
                .game_db
                .get(|| {
                    config!(config.config, game_db_path)
                        .as_ref()
                        .and_then(|path| match game_db::Database::read_from_file(&path.0) {
                            Ok(db) => Some(db),
                            Err(err) => {
                                let location_str = location_str!(&path.0);
                                match err {
                                    game_db::Error::Io(err) => {
                                        if err.kind() == io::ErrorKind::NotFound {
                                            warning!(
                                                "Missing game database",
                                                "The game database was not found{location_str}.",
                                            );
                                        } else {
                                            config_error!(
                                                "Couldn't read game database{location_str}: {err}",
                                            );
                                        }
                                    }
                                    game_db::Error::Json(err) => {
                                        config_error!(
                                            "Couldn't load game database{location_str}: {err}",
                                        );
                                    }
                                }
                                None
                            }
                        })
                })
                .as_ref()
                .and_then(|db| db.lookup(game_code))
                .map(|entry| {
                    if entry.rom_size as usize != rom.len() {
                        #[cfg(feature = "log")]
                        slog::error!(
                            logger,
                            "Unexpected ROM size: expected {} B, got {} B",
                            entry.rom_size,
                            rom.len()
                        );
                    }
                    entry.save_type
                });
            Some(emu::DsSlot {
                rom,
                save_type,
                has_ir: game_code as u8 == b'I',
            })
        });

        let frame_tx = self.frame_tx.take().unwrap();

        let audio_tx_data = self
            .audio_channel
            .as_ref()
            .map(|audio_channel| audio_channel.tx_data.clone());

        let (to_emu, from_ui) = crossbeam_channel::unbounded::<emu::Message>();
        let (to_ui, from_emu) = crossbeam_channel::unbounded::<emu::Notification>();

        let shared_state = Arc::new(emu::SharedState {
            playing: AtomicBool::new(playing),
            limit_framerate: AtomicBool::new(config!(config.config, limit_framerate)),

            #[cfg(feature = "gdb-server")]
            gdb_server_active: AtomicBool::new(false),
        });

        let launch_data = emu::LaunchData {
            sys_files: launch_config.sys_files,
            ds_slot,

            model: launch_config.model,
            skip_firmware: launch_config.skip_firmware,

            save_path,
            save_interval_ms: config!(config.config, save_interval_ms),

            shared_state: Arc::clone(&shared_state),
            from_ui,
            to_ui,

            audio_tx_data,
            frame_tx,

            sync_to_audio: config!(config.config, sync_to_audio),
            audio_sample_chunk_size: config!(config.config, audio_sample_chunk_size),
            #[cfg(feature = "xq-audio")]
            audio_custom_sample_rate: config!(config.config, audio_custom_sample_rate),
            #[cfg(feature = "xq-audio")]
            audio_channel_interp_method: config!(config.config, audio_channel_interp_method),

            rtc_time_offset_seconds: config!(config.config, rtc_time_offset_seconds),

            #[cfg(feature = "log")]
            logger,
        };

        let thread = thread::Builder::new()
            .name("emulation".to_string())
            .spawn(move || emu::main(launch_data))
            .expect("couldn't spawn emulation thread");

        #[cfg(feature = "debug-views")]
        self.debug_views.reload_emu_state();

        self.emu = Some(EmuState {
            playing,
            title,
            game_loaded,
            save_path_update: None,
            #[cfg(feature = "gdb-server")]
            gdb_server_addr: None,

            thread,

            shared_state,
            from_emu,
            to_emu,
        });
    }

    fn stop_emu(&mut self, config: &mut Config) {
        if let Some(emu) = self.emu.take() {
            emu.send_message(emu::Message::Stop);
            self.frame_tx = Some(emu.thread.join().expect("couldn't join emulation thread"));

            if let Some(path) = config.game_path.take() {
                let game_config = config::File {
                    contents: config.config.serialize_game(),
                    path: Some(path),
                };
                game_config
                    .write()
                    .expect("couldn't save game configuration");
            }
        }
    }

    fn stop(&mut self, config: &mut Config, window: &window::Window) {
        self.stop_emu(config);
        config.config.unset_game();

        #[cfg(feature = "debug-views")]
        self.debug_views.clear_frame_data();

        triple_buffer::reset(
            (self.frame_tx.as_mut().unwrap(), &mut self.frame_rx),
            |frame_data| {
                for data in frame_data {
                    for fb in &mut data.fb.0 {
                        fb.fill(0);
                    }
                    data.fps = 0.0;
                    #[cfg(feature = "debug-views")]
                    data.debug.clear();
                }
            },
        );

        #[cfg(target_os = "windows")]
        {
            self.icon_update = Some(None);
        }

        #[cfg(feature = "discord-presence")]
        if let Some(presence) = &mut self.discord_presence {
            presence.stop();
        }

        self.fb_texture.clear(window);
    }

    fn playing(&self) -> bool {
        self.emu.as_ref().map_or(false, |emu| emu.playing)
    }

    fn update_menu_bar(&mut self, config: &config::Config, window: &mut window::Window) {
        if config_changed!(config, fullscreen_render) {
            self.show_menu_bar = !config!(config, fullscreen_render);
        }

        #[cfg(target_os = "macos")]
        {
            if config_changed!(config, hide_macos_title_bar) {
                window.set_macos_title_bar_hidden(config!(config, hide_macos_title_bar));
            }
        }
    }

    fn update_title(&self, window: &window::Window) {
        #[cfg(target_os = "macos")]
        if window.macos_title_bar_hidden() && !self.show_menu_bar {
            window.window.set_title("");
            return;
        }

        let mut buffer = "Dust - ".to_string();
        if let Some(emu) = &self.emu {
            buffer.push_str(&emu.title);
            if let Some(fps_fixed) = self.fps_fixed {
                let _ = write!(buffer, " - {:.01} FPS", fps_fixed as f32 / 10.0);
            }
        } else {
            buffer.push_str("No game loaded");
        }
        window.window.set_title(&buffer);
    }
}

struct FbTexture {
    id: imgui::TextureId,
}

impl FbTexture {
    fn new(window: &mut window::Window) -> Self {
        let texture = window.gfx.imgui.create_texture(
            &window.gfx.device_state.device,
            &wgpu::SamplerDescriptor {
                label: Some("framebuffer sampler"),
                min_filter: wgpu::FilterMode::Linear,
                ..Default::default()
            },
            imgui_wgpu::TextureDescriptor {
                label: Some("framebuffer texture".to_string()),
                size: wgpu::Extent3d {
                    width: SCREEN_WIDTH as u32,
                    height: SCREEN_HEIGHT as u32 * 2,
                    depth_or_array_layers: 1,
                },
                format: Some(
                    if window.gfx.device_state.surf_config.format.describe().srgb {
                        wgpu::TextureFormat::Rgba8UnormSrgb
                    } else {
                        wgpu::TextureFormat::Rgba8Unorm
                    },
                ),
                ..Default::default()
            },
        );
        FbTexture {
            id: window.gfx.imgui.add_texture(texture),
        }
    }

    fn id(&self) -> imgui::TextureId {
        self.id
    }

    fn clear(&self, window: &window::Window) {
        let mut data = zeroed_box::<[u8; SCREEN_WIDTH * SCREEN_HEIGHT * 8]>();
        for i in (3..data.len()).step_by(4) {
            data[i] = 0xFF;
        }
        window.gfx.imgui.texture(self.id).set_data(
            &window.gfx.device_state.queue,
            &data[..],
            imgui_wgpu::TextureSetRange::default(),
        );
    }

    fn set_data(&self, window: &window::Window, data: &Framebuffer) {
        let data = unsafe {
            slice::from_raw_parts(
                data.0.as_ptr() as *const u8,
                SCREEN_WIDTH * SCREEN_HEIGHT * 8,
            )
        };
        window.gfx.imgui.texture(self.id).set_data(
            &window.gfx.device_state.queue,
            data,
            imgui_wgpu::TextureSetRange::default(),
        );
    }
}

pub fn main() {
    let panic_hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        error!(
            "Unexpected panic",
            "Encountered unexpected panic: {}\n\nThe emulator will now quit.", info
        );
        panic_hook(info);
    }));

    let mut config = Config::new();

    #[cfg(feature = "log")]
    let log = Log::new(&config.config);

    let mut window_builder = futures_executor::block_on(window::Builder::new(
        "Dust",
        config.config.window_size,
        #[cfg(target_os = "macos")]
        config!(config.config, hide_macos_title_bar),
    ));
    // TODO: Allow custom styles
    window_builder.apply_default_imgui_style();

    let init_imgui_config_path = config!(config.config, &imgui_config_path).clone();
    if let Some(path) = &init_imgui_config_path {
        if let Ok(config) = fs::read_to_string(&path.0) {
            window_builder.imgui.load_ini_settings(&config);
        }
    }

    let audio_channel = audio::channel(
        config!(config.config, audio_interp_method),
        config!(config.config, audio_volume),
        #[cfg(feature = "xq-audio")]
        adjust_custom_sample_rate(config!(config.config, audio_custom_sample_rate)),
    );

    let (frame_tx, frame_rx) = triple_buffer::init([
        FrameData::default(),
        FrameData::default(),
        FrameData::default(),
    ]);

    let fb_texture = FbTexture::new(&mut window_builder.window);
    fb_texture.clear(&window_builder.window);

    let mut state = UiState {
        game_db: Lazy::new(),

        emu: None,

        fb_texture,
        frame_tx: Some(frame_tx),
        frame_rx,
        fps_fixed: None,

        show_menu_bar: true,
        screen_focused: true,

        input: input::State::new(),
        input_map_editor: None,

        config_editor: None,

        audio_channel,

        #[cfg(target_os = "windows")]
        icon_update: None,

        #[cfg(feature = "log")]
        log,

        #[cfg(feature = "debug-views")]
        debug_views: debug_views::UiState::new(),

        #[cfg(feature = "discord-presence")]
        discord_presence: if config!(config.config, discord_presence_enabled) {
            Some(DiscordPresence::new())
        } else {
            None
        },

        editing_save_slot_i: None,
    };

    #[cfg(feature = "discord-presence")]
    if let Some(discord_presence) = &mut state.discord_presence {
        discord_presence.stop();
    }

    if let Some(rom_path) = env::args_os().nth(1) {
        state.load_from_rom_path(Path::new(&rom_path), &mut config, &window_builder.window);
    }

    window_builder.run(
        (config, state),
        |window, (config, state), event| {
            use winit::event::{Event, WindowEvent};

            if let Event::WindowEvent {
                event: WindowEvent::DroppedFile(path),
                ..
            } = event
            {
                state.load_from_rom_path(path, config, window);
            }

            state.input.process_event(event, state.screen_focused);
            if let Some(editor) = &mut state.input_map_editor {
                editor.process_event(event, &mut config.input_map.contents);
            }
        },
        |window, (config, state), ui| {
            // Drain input updates
            let (input_actions, emu_input_changes) = state.input.drain_changes(
                &config.input_map.contents,
                if let Some(emu) = &state.emu {
                    emu.playing
                } else {
                    false
                },
            );

            // Process input actions
            for action in input_actions {
                match action {
                    input::Action::PlayPause => state.play_pause(),
                    input::Action::Reset => state.reset(),
                    input::Action::Stop => {
                        state.stop(config, window);
                    }
                    input::Action::ToggleFramerateLimit => {
                        toggle_config!(config.config, limit_framerate)
                    }
                    input::Action::ToggleSyncToAudio => {
                        toggle_config!(config.config, sync_to_audio)
                    }
                    input::Action::ToggleFullscreenRender => {
                        toggle_config!(config.config, fullscreen_render)
                    }
                }
            }

            // Process configuration changes
            {
                state.update_menu_bar(&config.config, window);

                #[cfg(feature = "log")]
                if state.log.update(&config.config) {
                    if let Some(emu) = &state.emu {
                        emu.send_message(emu::Message::UpdateLogger(state.log.logger().clone()));
                    }
                }

                #[cfg(feature = "discord-presence")]
                if let Some(value) = config_changed_value!(config.config, discord_presence_enabled)
                {
                    if value != state.discord_presence.is_some() {
                        state.discord_presence = if value {
                            let mut presence = DiscordPresence::new();
                            if let Some(emu) = &state.emu {
                                presence.start(&emu.title);
                            } else {
                                presence.stop();
                            }
                            Some(presence)
                        } else {
                            None
                        }
                    }
                }

                if config_changed!(config.config, game_db_path) {
                    state.game_db.invalidate();
                }

                if let Some(emu) = &mut state.emu {
                    if let Some(value) = config_changed_value!(config.config, limit_framerate) {
                        emu.shared_state
                            .limit_framerate
                            .store(value, Ordering::Relaxed);
                    }

                    if config_changed!(config.config, save_dir_path | save_path_config)
                        && emu.save_path_update.is_none()
                    {
                        let new_path = config.config.save_path(&emu.title);
                        emu.save_path_update = Some(emu::SavePathUpdate {
                            new: new_path.clone(),
                            new_prev: Some(new_path),
                            reload: false,
                            reset: false,
                        });
                    }

                    if let Some(update) = emu.save_path_update.take() {
                        emu.send_message(emu::Message::UpdateSavePath(update));
                    }

                    if let Some(value) = config_changed_value!(config.config, save_interval_ms) {
                        emu.send_message(emu::Message::UpdateSaveIntervalMs(value));
                    }

                    if let Some(value) =
                        config_changed_value!(config.config, rtc_time_offset_seconds)
                    {
                        emu.send_message(emu::Message::UpdateRtcTimeOffsetSeconds(value));
                    }

                    if let Some(value) = config_changed_value!(config.config, sync_to_audio) {
                        emu.send_message(emu::Message::UpdateSyncToAudio(value));
                    }

                    if let Some(value) =
                        config_changed_value!(config.config, audio_sample_chunk_size)
                    {
                        emu.send_message(emu::Message::UpdateAudioSampleChunkSize(value));
                    }

                    #[cfg(feature = "xq-audio")]
                    {
                        if let Some(value) =
                            config_changed_value!(config.config, audio_custom_sample_rate)
                        {
                            emu.send_message(emu::Message::UpdateAudioCustomSampleRate(
                                adjust_custom_sample_rate(value),
                            ));
                        }

                        if let Some(value) =
                            config_changed_value!(config.config, audio_channel_interp_method)
                        {
                            emu.send_message(emu::Message::UpdateAudioChannelInterpMethod(value));
                        }
                    }
                }

                if let Some(channel) = state.audio_channel.as_mut() {
                    if let Some(value) = config_changed_value!(config.config, audio_volume) {
                        channel.output_stream.set_volume(value);
                    }

                    if let Some(value) = config_changed_value!(config.config, audio_interp_method) {
                        channel.output_stream.set_interp_method(value);
                    }

                    #[cfg(feature = "xq-audio")]
                    if let Some(value) =
                        config_changed_value!(config.config, audio_custom_sample_rate)
                    {
                        channel.set_custom_sample_rate(adjust_custom_sample_rate(value));
                    }
                }

                config.config.clear_updates();
            }

            // Process emulator-visible input changes
            if let Some(changes) = emu_input_changes {
                if let Some(emu) = &mut state.emu {
                    if emu.playing {
                        emu.send_message(emu::Message::UpdateInput(changes));
                    }
                }
            }

            // Update Discord presence
            #[cfg(feature = "discord-presence")]
            if let Some(presence) = &mut state.discord_presence {
                presence.rpc_connection.check_events();
                presence.flush();
            }

            // Process emulator messages
            'process_notifs: loop {
                if let Some(emu) = &mut state.emu {
                    for notif in emu.from_emu.try_iter() {
                        match notif {
                            emu::Notification::Stopped => {
                                state.stop(config, window);
                                continue 'process_notifs;
                            }

                            emu::Notification::RtcTimeOffsetSecondsUpdated(value) => {
                                set_config!(config.config, rtc_time_offset_seconds, value);
                                config.config.rtc_time_offset_seconds.clear_updates();
                            }
                        }
                    }
                }
                break;
            }

            // Process new frame data, if present
            if let Ok(frame) = state.frame_rx.get() {
                #[cfg(feature = "debug-views")]
                state
                    .debug_views
                    .update_from_frame_data(&frame.debug, window);

                state.fb_texture.set_data(window, &frame.fb);

                let fps_fixed = (frame.fps * 10.0).round() as u64;
                if Some(fps_fixed) != state.fps_fixed {
                    state.fps_fixed = Some(fps_fixed);
                }
            }

            // Draw menu bar
            if config!(config.config, fullscreen_render)
                && ui.is_key_pressed(imgui::Key::Escape)
                && !ui.is_any_item_focused()
            {
                state.show_menu_bar = !state.show_menu_bar;
            }
            if state.show_menu_bar {
                window.main_menu_bar(ui, |window| {
                    ui.menu("Emulation", || {
                        if ui
                            .menu_item_config(if state.playing() { "Pause" } else { "Play" })
                            .enabled(state.emu.is_some())
                            .build()
                        {
                            state.play_pause();
                        }

                        if ui
                            .menu_item_config("Reset")
                            .enabled(state.emu.is_some())
                            .build()
                        {
                            state.reset();
                        }

                        if ui
                            .menu_item_config("Stop")
                            .enabled(state.emu.is_some())
                            .build()
                        {
                            state.stop(config, window);
                        }

                        if ui.menu_item("Load game...") {
                            if let Some(path) = FileDialog::new()
                                .add_filter("NDS ROM file", ALLOWED_ROM_EXTENSIONS)
                                .pick_file()
                            {
                                state.load_from_rom_path(&path, config, window);
                            }
                        }

                        if ui.menu_item("Load firmware") {
                            state.load_firmware(config, window);
                        }
                    });

                    ui.menu("Config", || {
                        ui.menu("Volume", || {
                            let mut volume = config!(config.config, audio_volume) * 100.0;
                            if ui
                                .slider_config("##audio_volume", 0.0, 100.0)
                                .display_format("%.02f%%")
                                .build(&mut volume)
                            {
                                set_config!(config.config, audio_volume, volume / 100.0);
                            }
                        });

                        ui.menu("Screen rotation", || {
                            let mut screen_rot = config!(config.config, screen_rot);
                            if ui
                                .input_scalar("##screen_rot", &mut screen_rot)
                                .step(1)
                                .build()
                            {
                                screen_rot = screen_rot.min(359);
                            }
                            macro_rules! buttons {
                                ($($value: expr),*) => {
                                    $(
                                        if ui.button(stringify!($value)) {
                                            screen_rot = $value;
                                        }
                                        ui.same_line();
                                    )*
                                    ui.new_line();
                                };
                            }
                            buttons!(0, 90, 180, 270);
                            set_config!(config.config, screen_rot, screen_rot);
                        });

                        macro_rules! draw_config_toggle {
                            ($ident: ident, $desc: literal) => {{
                                let mut value = config!(config.config, $ident);
                                if ui.menu_item_config($desc).build_with_ref(&mut value) {
                                    set_config!(config.config, $ident, value);
                                }
                            }};
                        }

                        draw_config_toggle!(limit_framerate, "Limit framerate");
                        draw_config_toggle!(sync_to_audio, "Sync to audio");
                        draw_config_toggle!(fullscreen_render, "Fullscreen render");

                        ui.separator();

                        let mut save_slots_shown = false;
                        ui.menu_with_enabled("Save slot", state.emu.as_ref().map_or(false, |emu| emu.game_loaded), || {
                            use config::Setting;

                            save_slots_shown = true;

                            let emu_state = state.emu.as_mut().unwrap();

                            if let Some(path_config) = config!(config.config, &save_path_config) {
                                macro_rules! update_path_config {
                                    (|$path_config: ident| $inner: expr) => {
                                        config.config.save_path_config.update(|inner| {
                                            inner.update(|path_config| {
                                                let $path_config = path_config.as_mut().unwrap();
                                                $inner
                                            })
                                        });
                                    }
                                }

                                let save_dir = &config!(config.config, &save_dir_path).0;
                                let game_title = &emu_state.title;

                                if let config::saves::Slots::Multiple { current, slots } = &path_config.slots {
                                    let mut text_width = 0.0;
                                    for slot in slots {
                                        text_width = ui.calc_text_size(slot)[0].max(text_width);
                                    }
                                    let two_frame_padding = style!(ui, frame_padding)[0] * 2.0;
                                    text_width += two_frame_padding;
                                    let line_width = text_width + style!(ui, item_spacing)[0] + two_frame_padding + ui.calc_text_size("-")[0];
                                    ui.dummy([line_width, 0.0]);

                                    let mut switch = None;
                                    let mut remove = None;
                                    let mut rename = None;

                                    let current = *current;
                                    
                                    for (i, slot) in slots.iter().enumerate() {
                                        let _id = ui.push_id_usize(i);

                                        if Some(i) == state.editing_save_slot_i {
                                            let mut buffer = slot.to_string();
                                            ui.set_keyboard_focus_here();
                                            ui.set_next_item_width(line_width);
                                            if ui.input_text("", &mut buffer)
                                                .auto_select_all(true)
                                                .enter_returns_true(true)
                                                .build()
                                            {
                                                state.editing_save_slot_i = None;
                                                rename = Some((i, buffer));
                                            }
                                        } else {
                                            let color = (Some(i) == current).then(|| ui.push_style_color(imgui::StyleColor::Button, ui.style_color(imgui::StyleColor::ButtonActive)));
                                            if ui.button_with_size(slot, [text_width, 0.0]) {
                                                switch = Some(i);
                                            } else if ui.is_item_clicked_with_button(imgui::MouseButton::Right) {
                                                state.editing_save_slot_i = Some(i);
                                            }
                                            drop(color);

                                            ui.same_line();
                                            if ui.button("-") {
                                                remove = Some(i);
                                            }
                                        }
                                    }

                                    if state.editing_save_slot_i == Some(slots.len()) {
                                        let mut buffer = String::new();
                                        ui.set_keyboard_focus_here();
                                        ui.set_next_item_width(line_width);
                                        if ui.input_text("##new", &mut buffer)
                                            .auto_select_all(true)
                                            .enter_returns_true(true)
                                            .build()
                                        {
                                            state.editing_save_slot_i = None;
                                            update_path_config!(|path_config| {
                                                path_config.create_slot(buffer);
                                            });
                                        }
                                    } else if ui.button("+") {
                                        state.editing_save_slot_i = Some(slots.len());
                                    }

                                    if let Some(i) = switch {
                                        update_path_config!(|path_config| {
                                            if path_config.switch_slot(i) {
                                                let new_path = path_config.path(save_dir, game_title).unwrap();
                                                emu_state.save_path_update = Some(emu::SavePathUpdate {
                                                    new: Some(new_path),
                                                    new_prev: None,
                                                    reload: true,
                                                    reset: config!(config.config, reset_on_save_slot_switch),
                                                });
                                            }
                                        });
                                    } else if let Some(i) = remove {
                                        update_path_config!(|path_config| {
                                            path_config.remove_slot(i, save_dir, game_title);
                                        });
                                    } else if let Some((i, name)) = rename {
                                        update_path_config!(|path_config| {
                                            path_config.rename_slot(
                                                i,
                                                name,
                                                save_dir,
                                                game_title,
                                            );
                                        });
                                    }
                                } else if ui.button("Make multi-slot") {
                                    update_path_config!(|path_config| {
                                        path_config.make_multi_slot();
                                    });
                                }
                            }
                        });

                        if !save_slots_shown {
                            state.editing_save_slot_i = None;
                        }

                        ui.separator();

                        {
                            let mut show = state.input_map_editor.is_some();
                            if ui.menu_item_config("Input").build_with_ref(&mut show) {
                                state.input_map_editor = if show {
                                    Some(input::MapEditor::new())
                                } else {
                                    None
                                };
                            }
                        }

                        {
                            let mut show = state.config_editor.is_some();
                            if ui.menu_item_config("Config").build_with_ref(&mut show) {
                                state.config_editor = if show {
                                    Some(ConfigEditor::new())
                                } else {
                                    None
                                };
                            }
                        }
                    });

                    #[cfg(feature = "log")]
                    let imgui_log_enabled = state.log.is_imgui();
                    #[cfg(not(feature = "log"))]
                    let imgui_log_enabled = false;
                    if cfg!(any(feature = "debug-views", feature = "gdb-server"))
                        || imgui_log_enabled
                    {
                        #[allow(unused_assignments)]
                        ui.menu("Debug", || {
                            #[allow(unused_mut, unused_variables)]
                            let mut separator_needed = false;

                            #[allow(unused_macros)]
                            macro_rules! section {
                                ($content: block) => {
                                    if separator_needed {
                                        ui.separator();
                                    }
                                    $content
                                    separator_needed = true;
                                }
                            }

                            #[cfg(feature = "log")]
                            if let Log::Imgui { console_opened, .. } = &mut state.log {
                                section! {{
                                    ui.menu_item_config("Log").build_with_ref(console_opened);
                                }}
                            }

                            #[cfg(feature = "gdb-server")]
                            section! {{
                                #[cfg(feature = "gdb-server")]

                                let active = state.emu.as_ref().map_or(
                                    false,
                                    |emu| emu.shared_state.gdb_server_active.load(Ordering::Relaxed),
                                );
                                if ui
                                    .menu_item_config(if active {
                                        "Stop GDB server"
                                    } else {
                                        "Start GDB server"
                                    })
                                    .enabled(state.emu.is_some())
                                    .build()
                                {
                                    if let Some(emu) = &mut state.emu {
                                        emu.gdb_server_addr = if active {
                                            None
                                        } else {
                                            Some(config!(config.config, gdb_server_addr))
                                        };
                                        emu.send_message(emu::Message::ToggleGdbServer(
                                            emu.gdb_server_addr,
                                        ));
                                    }
                                }
                            }}

                            #[cfg(feature = "debug-views")]
                            section! {{
                                state.debug_views.draw_menu(ui, window);
                            }}
                        });
                    }

                    #[cfg(feature = "gdb-server")]
                    if let Some(emu) = &state.emu {
                        if emu.shared_state.gdb_server_active.load(Ordering::Relaxed) {
                            if let Some(server_addr) = emu.gdb_server_addr.as_ref() {
                                let text = format!("GDB: {server_addr}");
                                let width =
                                    ui.calc_text_size(&text)[0] + style!(ui, item_spacing)[0];
                                ui.set_cursor_pos([
                                    ui.content_region_max()[0] - width,
                                    ui.cursor_pos()[1],
                                ]);
                                ui.separator();
                                ui.text(&text);
                            }
                        }
                    }
                });
            }

            // Draw log
            #[cfg(feature = "log")]
            state.log.draw(ui, window.mono_font);

            // Draw debug views
            #[cfg(feature = "debug-views")]
            for message in state.debug_views.draw(ui, window, state.emu.is_some()) {
                if let Some(emu) = &state.emu {
                    emu.send_message(emu::Message::DebugViews(message));
                }
            }

            // Draw input map editor
            if let Some(editor) = &mut state.input_map_editor {
                let mut opened = true;
                editor.draw(ui, &mut config.input_map, &mut opened);
                if !opened {
                    state.input_map_editor = None;
                }
            }

            // Draw config editor
            if let Some(editor) = &mut state.config_editor {
                let mut opened = true;
                editor.draw(
                    ui,
                    config,
                    state.emu.as_mut(),
                    &mut opened,
                );
                if !opened {
                    state.config_editor = None;
                }
            }

            // Draw screen
            let window_size = window.window.inner_size();
            let screen_integer_scale = config!(config.config, screen_integer_scale);
            let screen_rot = (config!(config.config, screen_rot) as f32).to_radians();
            if config!(config.config, fullscreen_render) {
                let (center, points) = scale_to_fit_rotated(
                    [SCREEN_WIDTH as f32, (2 * SCREEN_HEIGHT) as f32],
                    screen_integer_scale,
                    screen_rot,
                    [
                        (window_size.width as f64 / window.scale_factor) as f32,
                        (window_size.height as f64 / window.scale_factor) as f32,
                    ],
                );
                ui.get_background_draw_list()
                    .add_image_quad(
                        state.fb_texture.id(),
                        points[0],
                        points[1],
                        points[2],
                        points[3],
                    )
                    .build();
                state.screen_focused =
                    !ui.is_window_focused_with_flags(imgui::WindowFocusedFlags::ANY_WINDOW);
                state.input.set_touchscreen_bounds_from_points(
                    center,
                    &points,
                    screen_rot,
                    window.scale_factor,
                );
            } else {
                let _window_padding = ui.push_style_var(imgui::StyleVar::WindowPadding([0.0; 2]));
                let title_bar_height = style!(ui, frame_padding)[1] * 2.0 + ui.current_font_size();
                const DEFAULT_SCALE: f32 = 2.0;
                state.screen_focused = false;
                ui.window("Screen")
                    .size(
                        [
                            SCREEN_WIDTH as f32 * DEFAULT_SCALE,
                            (SCREEN_HEIGHT * 2) as f32 * DEFAULT_SCALE + title_bar_height,
                        ],
                        imgui::Condition::FirstUseEver,
                    )
                    .position(
                        [
                            (window_size.width as f64 * 0.5 / window.scale_factor) as f32,
                            (window_size.height as f64 * 0.5 / window.scale_factor) as f32,
                        ],
                        imgui::Condition::FirstUseEver,
                    )
                    .position_pivot([0.5; 2])
                    .build(|| {
                        let (center, points) = scale_to_fit_rotated(
                            [SCREEN_WIDTH as f32, (2 * SCREEN_HEIGHT) as f32],
                            screen_integer_scale,
                            screen_rot,
                            ui.content_region_avail(),
                        );
                        let mut min = [f32::INFINITY; 2];
                        for point in &points {
                            min[0] = min[0].min(point[0]);
                            min[1] = min[1].min(point[1]);
                        }
                        ui.dummy([0, 1].map(|i| {
                            (points[0][i] - points[2][i])
                                .abs()
                                .max((points[1][i] - points[3][i]).abs())
                        }));
                        let window_pos = ui.window_pos();
                        let content_region_min = ui.window_content_region_min();
                        let upper_left = [
                            window_pos[0] + content_region_min[0],
                            window_pos[1] + content_region_min[1],
                        ];
                        let abs_points =
                            points.map(|[x, y]| [x + upper_left[0], y + upper_left[1]]);
                        ui.get_window_draw_list()
                            .add_image_quad(
                                state.fb_texture.id(),
                                abs_points[0],
                                abs_points[1],
                                abs_points[2],
                                abs_points[3],
                            )
                            .build();
                        state.screen_focused = ui.is_window_focused();
                        state.input.set_touchscreen_bounds_from_points(
                            [center[0] + upper_left[0], center[1] + upper_left[1]],
                            &abs_points,
                            screen_rot,
                            window.scale_factor,
                        );
                    });
            };

            // Process icon and title changes
            #[cfg(target_os = "windows")]
            if let Some(icon) = state.icon_update.take() {
                window.window.set_window_icon(icon.and_then(|icon_pixels| {
                    let mut rgba = Vec::with_capacity(32 * 32 * 4);
                    for pixel in icon_pixels {
                        rgba.extend_from_slice(&pixel.to_le_bytes());
                    }
                    winit::window::Icon::from_rgba(rgba, 32, 32).ok()
                }));
            }

            state.update_title(window);

            window::ControlFlow::Continue
        },
        move |window, mut imgui, (mut config, mut state)| {
            state.stop_emu(&mut config);

            config.config.window_size = window
                .window
                .inner_size()
                .to_logical::<u32>(window.scale_factor)
                .into();

            if let Some(path) = config.global_path {
                let global_config = config::File {
                    contents: config.config.serialize_global(),
                    path: Some(path),
                };
                global_config
                    .write()
                    .expect("couldn't save global configuration");
            }
            config.input_map.write().expect("couldn't save input map");

            if let Some(path) = config!(config.config, &imgui_config_path) {
                if let Some(init_path) = init_imgui_config_path {
                    if init_path != *path {
                        let _ = fs::remove_file(&init_path.0);
                    }
                }
                let mut buf = String::new();
                imgui.save_ini_settings(&mut buf);
                fs::write(&path.0, &buf).expect("couldn't save imgui configuration");
            }
        },
    );
}
