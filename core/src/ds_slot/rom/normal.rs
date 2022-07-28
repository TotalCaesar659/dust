use super::{super::RomOutputLen, key1, Contents};
use crate::{
    cpu::arm7,
    utils::{make_zero, ByteMutSlice, Bytes, Savestate},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CreationError {
    SizeNotPowerOfTwo,
    SizeTooSmall,
}

#[derive(Clone, Copy, PartialEq, Eq, Savestate)]
enum Stage {
    Initial,
    Key1,
    Key2,
    // Invalid,
}

#[derive(Savestate)]
#[load(in_place_only)]
pub struct Normal {
    #[cfg(feature = "log")]
    #[savestate(skip)]
    logger: slog::Logger,
    #[savestate(skip)]
    rom: Box<dyn Contents>,
    #[savestate(skip)]
    rom_mask: u32,
    #[savestate(skip)]
    chip_id: u32,
    #[savestate(skip)]
    key_buf: Option<Box<key1::KeyBuffer<false>>>, // Always at level 2
    stage: Stage,
}

impl Normal {
    /// # Errors
    /// - [`CreationError::SizeNotPowerOfTwo`](CreationError::SizeNotPowerOfTwo): the ROM contents'
    ///   size is not a power of two.
    pub fn new(
        rom: Box<dyn Contents>,
        arm7_bios: Option<&Bytes<{ arm7::BIOS_SIZE }>>,
        #[cfg(feature = "log")] logger: slog::Logger,
    ) -> Result<Self, CreationError> {
        let len = rom.len();
        if !len.is_power_of_two() {
            return Err(CreationError::SizeNotPowerOfTwo);
        }
        if len < 0x200 {
            return Err(CreationError::SizeTooSmall);
        }
        let rom_mask = (len - 1) as u32;
        let chip_id = 0x0000_00C2
            | match len as u32 {
                0..=0xF_FFFF => 0,
                len @ 0x10_0000..=0xFFF_FFFF => (len >> 20) - 1,
                len @ 0x1000_0000..=0xFFFF_FFFF => 0x100 - (len >> 28),
            };
        let game_code = rom.game_code();
        Ok(Normal {
            #[cfg(feature = "log")]
            logger,
            rom,
            rom_mask,
            chip_id,
            key_buf: arm7_bios.map(|bios| Box::new(key1::KeyBuffer::new::<2>(game_code, bios))),
            stage: Stage::Initial,
        })
    }

    #[must_use]
    pub fn reset(self) -> Self {
        Normal {
            stage: Stage::Initial,
            ..self
        }
    }
}

impl super::RomDevice for Normal {
    fn read(&mut self, addr: u32, mut output: ByteMutSlice) {
        let addr = (addr & self.rom_mask & !3) as usize;
        let rom_len = self.rom_mask as usize + 1;
        let first_read_max_len = rom_len - addr;
        if output.len() <= first_read_max_len {
            self.rom.read_slice(addr, output);
        } else {
            self.rom
                .read_slice(addr, ByteMutSlice::new(&mut output[..first_read_max_len]));
            let mut i = first_read_max_len;
            while i < output.len() {
                let end_i = (i + rom_len).min(output.len());
                self.rom
                    .read_slice(0, ByteMutSlice::new(&mut output[i..end_i]));
                i += rom_len;
            }
        }
    }

    fn read_header(&mut self, buf: &mut Bytes<0x170>) {
        self.rom.read_header(buf);
    }

    fn chip_id(&self) -> u32 {
        self.chip_id
    }

    fn setup(&mut self, direct_boot: bool) {
        if direct_boot {
            self.stage = Stage::Key2;
        } else {
            let key_buf = self.key_buf.as_ref().unwrap();
            let mut secure_area = self
                .rom
                .secure_area_mut()
                .expect("couldn't read DS slot ROM secure area");
            if secure_area.read_le::<u32>(0) == 0xE7FF_DEFF {
                secure_area[..8].copy_from_slice(b"encryObj");
                let level_3_key_buf = key_buf.level_3::<2>();
                for i in (0..0x800).step_by(8) {
                    let res = level_3_key_buf
                        .encrypt_64_bit([secure_area.read_le(i), secure_area.read_le(4 + i)]);
                    secure_area.write_le(i, res[0]);
                    secure_area.write_le(i + 4, res[1]);
                }
                let res = key_buf.encrypt_64_bit([secure_area.read_le(0), secure_area.read_le(4)]);
                secure_area.write_le(0, res[0]);
                secure_area.write_le(4, res[1]);
            }
        }
    }

    fn handle_rom_command(
        &mut self,
        mut cmd: Bytes<8>,
        output: &mut Bytes<0x4000>,
        output_len: RomOutputLen,
    ) {
        match self.stage {
            Stage::Initial => {
                #[cfg(feature = "log")]
                slog::trace!(self.logger, "Raw: {:016X}", cmd.read_be::<u64>(0));
                match cmd[0] {
                    0x9F => {
                        if cmd.read_be::<u64>(0) & 0x00FF_FFFF_FFFF_FFFF == 0 {
                            output[..output_len.get() as usize].fill(0xFF);
                            return;
                        }
                    }

                    0x00 => {
                        if cmd.read_be::<u64>(0) & 0x00FF_FFFF_FFFF_FFFF == 0 {
                            for start_i in (0..output_len.get() as usize).step_by(0x1000) {
                                let len = 0x1000.min(output_len.get() as usize - start_i);
                                self.rom.read_slice(
                                    0,
                                    ByteMutSlice::new(&mut output[start_i..start_i + len]),
                                );
                            }
                            return;
                        }
                    }

                    0x90 => {
                        if cmd.read_be::<u64>(0) & 0x00FF_FFFF_FFFF_FFFF == 0 {
                            let chip_id = self.chip_id;
                            for i in (0..output_len.get() as usize).step_by(4) {
                                output.write_le(i, chip_id);
                            }
                            return;
                        }
                    }

                    0x3C => {
                        self.stage = Stage::Key1;
                        output[..output_len.get() as usize].fill(0xFF);
                        return;
                    }

                    _ => {}
                }
                // TODO: What value is returned?
                #[cfg(feature = "log")]
                slog::warn!(
                    self.logger,
                    "Unknown ROM command in raw mode: {:016X}",
                    cmd.read_be::<u64>(0)
                );
                output[..output_len.get() as usize].fill(0xFF);
            }

            Stage::Key1 => {
                #[cfg(feature = "log")]
                let prev_cmd = cmd.clone();
                {
                    let res = self
                        .key_buf
                        .as_ref()
                        .unwrap()
                        .decrypt_64_bit([cmd.read_be(4), cmd.read_be(0)]);
                    cmd.write_be(4, res[0]);
                    cmd.write_be(0, res[1]);
                }
                #[cfg(feature = "log")]
                slog::trace!(
                    self.logger,
                    "KEY1: {:016X} (decrypted from {:016X})",
                    cmd.read_be::<u64>(0),
                    prev_cmd.read_be::<u64>(0)
                );
                // TODO: Handle repeated commands for larger carts (bit 31 of chip ID set)
                // TODO: Check other command bytes for correctness too
                match cmd[0] >> 4 {
                    0x4 => {
                        // TODO: What value is returned?
                        output[..output_len.get() as usize].fill(0xFF);
                        return;
                    }

                    0x1 => {
                        let chip_id = self.chip_id;
                        for i in (0..output_len.get() as usize).step_by(4) {
                            output.write_le(i, chip_id);
                        }
                        return;
                    }

                    0x2 => {
                        // TODO: What's the actual range of the address command bytes?
                        // TODO: What happens if the read goes out of bounds? (Though it can only
                        //       happen for homebrew)
                        let start_addr = 0x4000 | (cmd[2] as usize & 0x30) << 8;
                        for start_i in (0..output_len.get() as usize).step_by(0x1000) {
                            let len = (output_len.get() as usize - start_i).min(0x1000);
                            self.rom.read_slice(
                                start_addr,
                                ByteMutSlice::new(&mut output[start_i..start_i + len]),
                            );
                        }
                        return;
                    }

                    // 0x6 => {
                    //     // TODO: What value is returned?
                    //     make_zero(&mut output[..output_len.get() as usize]);
                    //     self.cmd_encryption = State::Key2;
                    //     return;
                    // }
                    0xA => {
                        self.stage = Stage::Key2;
                        make_zero(&mut output[..output_len.get() as usize]);
                        return;
                    }

                    _ => {}
                }
                // TODO: What value is returned?
                #[cfg(feature = "log")]
                slog::warn!(
                    self.logger,
                    "Unknown ROM command in KEY1 mode: {:016X}",
                    cmd.read_be::<u64>(0)
                );
                make_zero(&mut output[..output_len.get() as usize]);
            }

            Stage::Key2 => {
                #[cfg(feature = "log")]
                slog::trace!(self.logger, "KEY2: {:016X}", cmd.read_be::<u64>(0));
                match cmd[0] {
                    0xB7 => {
                        // if cmd.read_be::<u32>(4) & 0x00FF_FFFF == 0 {
                        let mut addr = (cmd.read_be::<u32>(1) & self.rom_mask) as usize;
                        if addr < 0x8000 {
                            addr = 0x8000 | (addr & 0x1FF);
                        }
                        let page_start = addr & !0xFFF;
                        let page_end = page_start + 0x1000;
                        let mut start_i = 0;
                        while start_i < output_len.get() as usize {
                            let len = (page_end - addr).min(output_len.get() as usize - start_i);
                            self.rom.read_slice(
                                addr,
                                ByteMutSlice::new(&mut output[start_i..start_i + len]),
                            );
                            addr = page_start;
                            start_i += len;
                        }
                        return;
                        // }
                    }

                    0xB8 => {
                        if cmd.read_be::<u64>(0) & 0x00FF_FFFF_FFFF_FFFF == 0 {
                            let chip_id = self.chip_id;
                            for i in (0..output_len.get() as usize).step_by(4) {
                                output.write_le(i, chip_id);
                            }
                            return;
                        }
                    }

                    _ => {}
                }
                #[cfg(feature = "log")]
                slog::warn!(
                    self.logger,
                    "Unknown ROM command in KEY2 mode: {:016X}",
                    cmd.read_be::<u64>(0)
                );
                // self.stage = Stage::Invalid;
                make_zero(&mut output[..output_len.get() as usize]);
            } // Stage::Invalid => {
              //     #[cfg(feature = "log")]
              //     slog::warn!(
              //         self.logger,
              //         "Unknown ROM command after entering invalid state: {:016X}",
              //         cmd.read_be::<u64>(0)
              //     );
              //     make_zero(&mut output[..output_len.get() as usize]);
              // }
        }
    }
}
