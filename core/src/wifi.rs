mod io;

use crate::utils::{Bytes, Savestate};

#[derive(Savestate)]
pub struct WiFi {
    pub mmio: Box<Bytes<0x1000>>,
    pub ram: Box<Bytes<0x2000>>,
    bb_regs: [u8; 0x100],
}

impl WiFi {
    pub(crate) fn new() -> Self {
        let mut mmio = unsafe { Box::<Bytes<0x1000>>::new_zeroed().assume_init() };
        mmio[0x3D] = 0x02;

        let mut bb_regs = [0; 0x100];
        bb_regs[0x00] = 0x6D;
        bb_regs[0x4D] = 0xBF; // ???
        bb_regs[0x5D] = 0x01;
        bb_regs[0x64] = 0xFF; // ???

        WiFi {
            mmio,
            ram: unsafe { Box::new_zeroed().assume_init() },
            bb_regs,
        }
    }
}
