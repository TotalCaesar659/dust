#[cfg(feature = "gdb-server")]
mod gdb_server;
mod renderer_3d;
mod rtc;

#[cfg(feature = "debug-views")]
use super::debug_views;
use super::{
    audio, config::CommonLaunchConfig, game_db::SaveType, input, triple_buffer, DsSlotRom,
    FrameData,
};
use dust_core::{
    audio::DummyBackend as DummyAudioBackend,
    cpu::{arm7, interpreter::Interpreter},
    ds_slot::{self, spi::Spi as DsSlotSpi},
    emu::RunOutput,
    flash::Flash,
    spi::firmware,
    utils::{BoxedByteSlice, Bytes},
    SaveContents,
};
#[cfg(feature = "gdb-server")]
use std::net::SocketAddr;
#[cfg(feature = "xq-audio")]
use std::num::NonZeroU32;
use std::{
    fs::{self, File},
    hint,
    io::{self, Read},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

pub struct SharedState {
    // UI to emu
    pub playing: AtomicBool,
    pub limit_framerate: AtomicBool,
    pub autosave_interval_ns: AtomicU64,

    // Emu to UI
    pub stopped: AtomicBool,
    #[cfg(feature = "gdb-server")]
    pub gdb_server_active: AtomicBool,
}

pub enum Message {
    UpdateInput(input::Changes),
    UpdateSavePath(Option<PathBuf>),
    UpdateAudioSampleChunkSize(u16),
    #[cfg(feature = "xq-audio")]
    UpdateAudioCustomSampleRate(Option<NonZeroU32>),
    #[cfg(feature = "xq-audio")]
    UpdateAudioChannelInterpMethod(dust_core::audio::ChannelInterpMethod),
    UpdateSyncToAudio(bool),
    #[cfg(feature = "debug-views")]
    DebugViews(debug_views::Message),
    CreateSavestate,
    Reset,
    Stop,
}

pub struct DsSlot {
    pub rom: DsSlotRom,
    pub save_type: Option<SaveType>,
    pub has_ir: bool,
}

fn setup_ds_slot(
    ds_slot: Option<DsSlot>,
    arm7_bios: &Option<Box<Bytes<{ arm7::BIOS_SIZE }>>>,
    cur_save_path: &Option<PathBuf>,
    #[cfg(feature = "log")] logger: &slog::Logger,
) -> (ds_slot::rom::Rom, ds_slot::spi::Spi) {
    if let Some(ds_slot) = ds_slot {
        let rom = ds_slot::rom::normal::Normal::new(
            ds_slot.rom.into(),
            arm7_bios.as_deref(),
            #[cfg(feature = "log")]
            logger.new(slog::o!("ds_rom" => "normal")),
        )
        .unwrap()
        .into();

        let save_contents = cur_save_path
            .as_deref()
            .and_then(|path| match File::open(path) {
                Ok(mut save_file) => {
                    let save_len = save_file
                        .metadata()
                        .expect("Couldn't get save RAM file metadata")
                        .len()
                        .next_power_of_two() as usize;
                    let mut save = BoxedByteSlice::new_zeroed(save_len);
                    save_file
                        .read_exact(&mut save[..])
                        .expect("Couldn't read save file");
                    Some(save)
                }
                Err(err) => match err.kind() {
                    io::ErrorKind::NotFound => None,
                    _err => {
                        #[cfg(feature = "log")]
                        slog::error!(logger, "Couldn't read save file: {:?}.", _err);
                        None
                    }
                },
            });

        let save_type = if let Some(save_contents) = &save_contents {
            if let Some(save_type) = ds_slot.save_type {
                let expected_len = save_type.expected_len();
                if expected_len != Some(save_contents.len()) {
                    let (chosen_save_type, _message) = if let Some(detected_save_type) =
                        SaveType::from_save_len(save_contents.len())
                    {
                        (detected_save_type, "existing save file")
                    } else {
                        (save_type, "database entry")
                    };
                    #[cfg(feature = "log")]
                    slog::error!(
                        logger,
                        "Unexpected save file size: expected {}, got {} B; respecting {}.",
                        if let Some(expected_len) = expected_len {
                            format!("{} B", expected_len)
                        } else {
                            "no file".to_string()
                        },
                        save_contents.len(),
                        _message,
                    );
                    chosen_save_type
                } else {
                    save_type
                }
            } else {
                #[allow(clippy::unnecessary_lazy_evaluations)]
                SaveType::from_save_len(save_contents.len()).unwrap_or_else(|| {
                    #[cfg(feature = "log")]
                    slog::error!(
                        logger,
                        concat!(
                            "Unrecognized save file size ({} B) and no database entry found, ",
                            "defaulting to an empty save.",
                        ),
                        save_contents.len()
                    );
                    SaveType::None
                })
            }
        } else {
            #[allow(clippy::unnecessary_lazy_evaluations)]
            ds_slot.save_type.unwrap_or_else(|| {
                #[cfg(feature = "log")]
                slog::error!(
                    logger,
                    concat!(
                        "No existing save file present and no database entry found, defaulting to ",
                        "an empty save.",
                    )
                );
                SaveType::None
            })
        };

        let spi = if save_type == SaveType::None {
            ds_slot::spi::Empty::new(
                #[cfg(feature = "log")]
                logger.new(slog::o!("ds_spi" => "empty")),
            )
            .into()
        } else {
            let expected_len = save_type.expected_len().unwrap();
            let save_contents = match save_contents {
                Some(save_contents) => {
                    SaveContents::Existing(if save_contents.len() == expected_len {
                        let mut new_contents = BoxedByteSlice::new_zeroed(expected_len);
                        new_contents[..save_contents.len()].copy_from_slice(&save_contents);
                        drop(save_contents);
                        new_contents
                    } else {
                        save_contents
                    })
                }
                None => SaveContents::New(expected_len),
            };
            match save_type {
                SaveType::None => unreachable!(),
                SaveType::Eeprom4k => ds_slot::spi::eeprom_4k::Eeprom4k::new(
                    save_contents,
                    None,
                    #[cfg(feature = "log")]
                    logger.new(slog::o!("ds_spi" => "eeprom_4k")),
                )
                .expect("Couldn't create 4 Kib EEPROM DS slot SPI device")
                .into(),
                SaveType::EepromFram64k | SaveType::EepromFram512k | SaveType::EepromFram1m => {
                    ds_slot::spi::eeprom_fram::EepromFram::new(
                        save_contents,
                        None,
                        #[cfg(feature = "log")]
                        logger.new(slog::o!("ds_spi" => "eeprom_fram")),
                    )
                    .expect("Couldn't create EEPROM/FRAM DS slot SPI device")
                    .into()
                }
                SaveType::Flash2m | SaveType::Flash4m | SaveType::Flash8m => {
                    ds_slot::spi::flash::Flash::new(
                        save_contents,
                        [0; 20],
                        ds_slot.has_ir,
                        #[cfg(feature = "log")]
                        logger.new(
                            slog::o!("ds_spi" => if ds_slot.has_ir { "flash" } else { "flash_ir" }),
                        ),
                    )
                    .expect("Couldn't create FLASH DS slot SPI device")
                    .into()
                }
                SaveType::Nand64m | SaveType::Nand128m | SaveType::Nand256m => {
                    #[cfg(feature = "log")]
                    slog::error!(logger, "TODO: NAND saves");
                    ds_slot::spi::Empty::new(
                        #[cfg(feature = "log")]
                        logger.new(slog::o!("ds_spi" => "nand_todo")),
                    )
                    .into()
                }
            }
        };

        (rom, spi)
    } else {
        (
            ds_slot::rom::Empty::new(
                #[cfg(feature = "log")]
                logger.new(slog::o!("ds_rom" => "empty")),
            )
            .into(),
            ds_slot::spi::Empty::new(
                #[cfg(feature = "log")]
                logger.new(slog::o!("ds_spi" => "empty")),
            )
            .into(),
        )
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn main(
    config: CommonLaunchConfig,
    mut cur_save_path: Option<PathBuf>,
    ds_slot: Option<DsSlot>,
    audio_tx_data: Option<audio::SenderData>,
    mut frame_tx: triple_buffer::Sender<FrameData>,
    message_rx: crossbeam_channel::Receiver<Message>,
    shared_state: Arc<SharedState>,
    #[cfg(feature = "gdb-server")] gdb_server_addr: SocketAddr,
    #[cfg(feature = "log")] logger: slog::Logger,
) -> triple_buffer::Sender<FrameData> {
    let skip_firmware = config.skip_firmware;
    let mut sync_to_audio = config.sync_to_audio.value;

    let (ds_slot_rom, ds_slot_spi) = setup_ds_slot(
        ds_slot,
        &config.sys_files.arm7_bios,
        &cur_save_path,
        #[cfg(feature = "log")]
        &logger,
    );

    let mut emu_builder = dust_core::emu::Builder::new(
        Flash::new(
            SaveContents::Existing(
                config
                    .sys_files
                    .firmware
                    .unwrap_or_else(|| firmware::default(config.model)),
            ),
            firmware::id_for_model(config.model),
            #[cfg(feature = "log")]
            logger.new(slog::o!("fw" => "")),
        )
        .expect("Couldn't build firmware"),
        ds_slot_rom,
        ds_slot_spi,
        match &audio_tx_data {
            Some(data) => Box::new(audio::Sender::new(data, sync_to_audio)),
            None => Box::new(DummyAudioBackend),
        },
        Box::new(rtc::Backend::new(config.rtc_time_offset_seconds.value)),
        Box::new(renderer_3d::Renderer::new()),
        #[cfg(feature = "log")]
        logger.clone(),
    );

    emu_builder.arm7_bios = config.sys_files.arm7_bios.clone();
    emu_builder.arm9_bios = config.sys_files.arm9_bios.clone();

    emu_builder.model = config.model;
    emu_builder.direct_boot = skip_firmware;
    // TODO: Set batch_duration and first_launch?
    emu_builder.audio_sample_chunk_size = config.audio_sample_chunk_size.value;
    #[cfg(feature = "xq-audio")]
    {
        emu_builder.audio_custom_sample_rate = config.audio_custom_sample_rate.value;
        emu_builder.audio_channel_interp_method = config.audio_channel_interp_method.value;
    }

    let mut emu = emu_builder.build(Interpreter).unwrap();

    const FRAME_INTERVAL: Duration = Duration::from_nanos(1_000_000_000 / 60);
    let mut last_frame_time = Instant::now();

    const FPS_CALC_INTERVAL: Duration = Duration::from_secs(1);
    let mut frames_since_last_fps_calc = 0;
    let mut last_fps_calc_time = last_frame_time;
    let mut fps = 0.0;

    let mut last_save_flush_time = last_frame_time;

    #[cfg(feature = "debug-views")]
    let mut debug_views = debug_views::EmuState::new();

    #[cfg(feature = "gdb-server")]
    let mut gdb_server = None;

    macro_rules! save {
        () => {
            if let Some(save_path) = &cur_save_path {
                if emu.ds_slot.spi.contents_dirty()
                    && save_path
                        .parent()
                        .map(|parent| fs::create_dir_all(parent).is_ok())
                        .unwrap_or(true)
                    && fs::write(save_path, &emu.ds_slot.spi.contents()[..]).is_ok()
                {
                    emu.ds_slot.spi.mark_contents_flushed();
                }
            }
        };
    }

    'run_loop: loop {
        let mut reset_triggered = false;

        for message in message_rx.try_iter() {
            match message {
                Message::UpdateInput(changes) => {
                    emu.press_keys(changes.pressed);
                    emu.release_keys(changes.released);
                    if let Some(new_touch_pos) = changes.touch_pos {
                        if let Some(touch_pos) = new_touch_pos {
                            emu.set_touch_pos(touch_pos);
                        } else {
                            emu.end_touch();
                        }
                    }
                }

                Message::UpdateSavePath(new_path) => {
                    if let Some(prev_path) = cur_save_path {
                        let _ = if let Some(new_path) = &new_path {
                            fs::rename(prev_path, new_path)
                        } else {
                            fs::remove_file(prev_path)
                        };
                    }
                    cur_save_path = new_path;
                }

                Message::UpdateAudioSampleChunkSize(chunk_size) => {
                    emu.audio.sample_chunk_size = chunk_size;
                }

                #[cfg(feature = "xq-audio")]
                Message::UpdateAudioCustomSampleRate(sample_rate) => {
                    dust_core::audio::Audio::set_custom_sample_rate(&mut emu, sample_rate);
                }

                #[cfg(feature = "xq-audio")]
                Message::UpdateAudioChannelInterpMethod(interp_method) => {
                    emu.audio.set_channel_interp_method(interp_method);
                }

                Message::UpdateSyncToAudio(new_sync_to_audio) => {
                    sync_to_audio = new_sync_to_audio;
                    if let Some(data) = &audio_tx_data {
                        emu.audio.backend = Box::new(audio::Sender::new(data, sync_to_audio));
                    }
                }

                #[cfg(feature = "debug-views")]
                Message::DebugViews(message) => {
                    debug_views.handle_message(&mut emu, message);
                }

                Message::CreateSavestate => {
                    todo!();
                }

                Message::Reset => {
                    reset_triggered = true;
                }

                Message::Stop => {
                    break 'run_loop;
                }
            }
        }

        #[cfg(feature = "gdb-server")]
        if shared_state.gdb_server_active.load(Ordering::Relaxed) != gdb_server.is_some() {
            if gdb_server.is_some() {
                gdb_server = None;
            } else {
                match gdb_server::GdbServer::new(gdb_server_addr) {
                    Ok(mut server) => {
                        server.attach(&mut emu);
                        gdb_server = Some(server);
                    }
                    Err(_err) => {
                        #[cfg(feature = "log")]
                        slog::error!(logger, "Couldn't start GDB server: {}", _err);
                        shared_state
                            .gdb_server_active
                            .store(false, Ordering::Relaxed);
                    }
                };
            }
        }

        let mut playing = true;

        #[cfg(feature = "gdb-server")]
        if let Some(gdb_server) = &mut gdb_server {
            reset_triggered |= gdb_server.poll(&mut emu);
            playing &= !gdb_server.target_stopped();
        }

        if reset_triggered {
            #[cfg(feature = "xq-audio")]
            let audio_custom_sample_rate = emu.audio.custom_sample_rate();
            #[cfg(feature = "xq-audio")]
            let audio_channel_interp_method = emu.audio.channel_interp_method();

            let mut emu_builder = dust_core::emu::Builder::new(
                emu.spi.firmware.reset(),
                match emu.ds_slot.rom {
                    ds_slot::rom::Rom::Empty(device) => ds_slot::rom::Rom::Empty(device.reset()),
                    ds_slot::rom::Rom::Normal(device) => ds_slot::rom::Rom::Normal(device.reset()),
                },
                match emu.ds_slot.spi {
                    DsSlotSpi::Empty(device) => DsSlotSpi::Empty(device.reset()),
                    DsSlotSpi::Eeprom4k(device) => DsSlotSpi::Eeprom4k(device.reset()),
                    DsSlotSpi::EepromFram(device) => DsSlotSpi::EepromFram(device.reset()),
                    DsSlotSpi::Flash(device) => DsSlotSpi::Flash(device.reset()),
                },
                emu.audio.backend,
                emu.rtc.backend,
                emu.gpu.engine_3d.renderer,
                #[cfg(feature = "log")]
                logger.clone(),
            );

            emu_builder.arm7_bios = config.sys_files.arm7_bios.clone();
            emu_builder.arm9_bios = config.sys_files.arm9_bios.clone();

            emu_builder.model = config.model;
            emu_builder.direct_boot = skip_firmware;
            // TODO: Set batch_duration and first_launch?
            emu_builder.audio_sample_chunk_size = emu.audio.sample_chunk_size;
            #[cfg(feature = "xq-audio")]
            {
                emu_builder.audio_custom_sample_rate = audio_custom_sample_rate;
                emu_builder.audio_channel_interp_method = audio_channel_interp_method;
            }

            emu = emu_builder.build(Interpreter).unwrap();
            #[cfg(feature = "gdb-server")]
            if let Some(server) = &mut gdb_server {
                server.attach(&mut emu);
            }
        }

        playing &= shared_state.playing.load(Ordering::Relaxed);

        let frame = frame_tx.start();

        if playing {
            #[cfg(feature = "gdb-server")]
            let mut run_forever = 0;
            #[cfg(feature = "gdb-server")]
            let cycles = if let Some(gdb_server) = &mut gdb_server {
                &mut gdb_server.remaining_step_cycles
            } else {
                &mut run_forever
            };
            match emu.run(
                #[cfg(feature = "gdb-server")]
                cycles,
            ) {
                RunOutput::FrameFinished => {}
                RunOutput::Shutdown => {
                    shared_state.stopped.store(true, Ordering::Relaxed);
                    #[cfg(feature = "gdb-server")]
                    if let Some(gdb_server) = &mut gdb_server {
                        gdb_server.emu_shutdown(&mut emu);
                    }
                }
                #[cfg(feature = "gdb-server")]
                RunOutput::StoppedByDebugHook | RunOutput::CyclesOver => {
                    if let Some(gdb_server) = &mut gdb_server {
                        gdb_server.emu_stopped(&mut emu);
                    }
                }
            }
        }
        frame.fb.0.copy_from_slice(&emu.gpu.framebuffer.0);

        #[cfg(feature = "debug-views")]
        debug_views.prepare_frame_data(&mut emu, &mut frame.debug);

        frames_since_last_fps_calc += 1;
        let now = Instant::now();
        let elapsed = now - last_fps_calc_time;
        if elapsed >= FPS_CALC_INTERVAL {
            fps = (frames_since_last_fps_calc as f64 / elapsed.as_secs_f64()) as f32;
            last_fps_calc_time = now;
            frames_since_last_fps_calc = 0;
        }
        frame.fps = fps;

        frame_tx.finish();

        let now = Instant::now();
        if now - last_save_flush_time
            >= Duration::from_nanos(shared_state.autosave_interval_ns.load(Ordering::Relaxed))
        {
            last_save_flush_time = now;
            save!();
        }

        if !playing || shared_state.limit_framerate.load(Ordering::Relaxed) {
            let now = Instant::now();
            let elapsed = now - last_frame_time;
            if elapsed < FRAME_INTERVAL {
                last_frame_time += FRAME_INTERVAL;
                let sleep_interval =
                    (FRAME_INTERVAL - elapsed).saturating_sub(Duration::from_millis(1));
                if !sleep_interval.is_zero() {
                    std::thread::sleep(sleep_interval);
                }
                while Instant::now() < last_frame_time {
                    hint::spin_loop();
                }
            } else {
                last_frame_time = now;
            }
        }
    }

    save!();

    frame_tx
}
