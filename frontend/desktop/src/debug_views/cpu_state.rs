use super::{
    common::{
        layout_group, psr_mode_to_str,
        regs::{bitfield, regs_32, regs_32_default_label, BitfieldCommand},
        separator_with_width,
    },
    FrameDataSlot, Messages, View,
};
use crate::ui::window::Window;
use dust_core::{
    cpu::{
        arm7::Arm7,
        arm9::Arm9,
        psr::{Cpsr, Mode},
        Engine, Regs,
    },
    emu::Emu,
};
use imgui::{StyleColor, StyleVar, TableFlags};

pub struct CpuState<const ARM9: bool> {
    reg_values: Option<(Regs, Cpsr)>,
    reg_bank: Option<RegBank>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegBank {
    User,
    Fiq,
    Irq,
    Supervisor,
    Abort,
    Undefined,
}

mod bounded {
    use dust_core::utils::bounded_int_lit;
    bounded_int_lit!(pub struct RegIndex(u8), max 15);
}
pub use bounded::*;

impl<const ARM9: bool> View for CpuState<ARM9> {
    const NAME: &'static str = if ARM9 { "ARM9 state" } else { "ARM7 state" };

    type FrameData = (Regs, Cpsr);
    type EmuState = ();
    type Message = (RegBank, RegIndex, u32);

    fn new(_window: &mut Window) -> Self {
        CpuState {
            reg_values: None,
            reg_bank: None,
        }
    }

    fn destroy(self, _window: &mut Window) {}

    fn emu_state(&self) -> Self::EmuState {}

    fn handle_emu_state_changed<E: Engine>(
        _prev: Option<&Self::EmuState>,
        _new: Option<&Self::EmuState>,
        _emu: &mut Emu<E>,
    ) {
    }

    fn prepare_frame_data<'a, E: Engine, S: FrameDataSlot<'a, Self::FrameData>>(
        _emu_state: &Self::EmuState,
        emu: &mut Emu<E>,
        frame_data: S,
    ) {
        frame_data.insert(if ARM9 {
            (emu.arm9.regs(), emu.arm9.cpsr())
        } else {
            (emu.arm7.regs(), emu.arm7.cpsr())
        });
    }

    fn handle_custom_message<E: dust_core::cpu::Engine>(
        (bank, i, value): Self::Message,
        _emu_state: &Self::EmuState,
        emu: &mut Emu<E>,
    ) {
        let (mut regs, cpsr) = if ARM9 {
            (emu.arm9.regs(), emu.arm9.cpsr())
        } else {
            (emu.arm7.regs(), emu.arm7.cpsr())
        };

        let mode = cpsr.mode();

        let i = i.get() as usize;
        match i {
            0..=7 | 15 => regs.gprs[i] = value,

            8..=12 => {
                if (mode == Mode::Fiq) == (bank == RegBank::Fiq) {
                    regs.gprs[i] = value;
                } else {
                    regs.r8_14_fiq[i] = value;
                }
            }

            _ => {
                if match bank {
                    RegBank::User => matches!(mode, Mode::User | Mode::System),
                    RegBank::Fiq => mode == Mode::Fiq,
                    RegBank::Irq => mode == Mode::Irq,
                    RegBank::Supervisor => mode == Mode::Supervisor,
                    RegBank::Abort => mode == Mode::Abort,
                    RegBank::Undefined => mode == Mode::Undefined,
                } {
                    regs.gprs[i] = value;
                } else {
                    match bank {
                        RegBank::User => regs.r13_14_user[i] = value,
                        RegBank::Fiq => regs.r8_14_fiq[5 + i] = value,
                        RegBank::Irq => regs.r13_14_irq[i] = value,
                        RegBank::Supervisor => regs.r13_14_svc[i] = value,
                        RegBank::Abort => regs.r13_14_abt[i] = value,
                        RegBank::Undefined => regs.r13_14_und[i] = value,
                    }
                }
            }
        }

        if ARM9 {
            Arm9::set_regs(emu, &regs);
        } else {
            Arm7::set_regs(emu, &regs);
        }
    }

    fn clear_frame_data(&mut self) {
        self.reg_values = None;
    }

    fn update_from_frame_data(&mut self, frame_data: &Self::FrameData, _window: &mut Window) {
        self.reg_values = Some(frame_data.clone());
    }

    fn customize_window<'ui, 'a, T: AsRef<str>>(
        &mut self,
        _ui: &imgui::Ui,
        window: imgui::Window<'ui, 'a, T>,
    ) -> imgui::Window<'ui, 'a, T> {
        window.always_auto_resize(true)
    }

    fn render(
        &mut self,
        ui: &imgui::Ui,
        window: &mut Window,
        _emu_running: bool,
        mut messages: impl Messages<Self>,
    ) -> Option<Self::EmuState> {
        if let Some((reg_values, cpsr)) = self.reg_values.as_mut() {
            let _mono_font_token = ui.push_font(window.mono_font);
            let _item_spacing = ui.push_style_var(StyleVar::ItemSpacing([0.0, unsafe {
                ui.style().item_spacing[1]
            }]));
            let _table_border = ui.push_style_color(
                StyleColor::TableBorderLight,
                ui.style_color(StyleColor::Border),
            );

            let mode = cpsr.mode();
            let cpu_reg_bank = match mode {
                Mode::User | Mode::System => RegBank::User,
                Mode::Fiq => RegBank::Fiq,
                Mode::Irq => RegBank::Irq,
                Mode::Supervisor => RegBank::Supervisor,
                Mode::Abort => RegBank::Abort,
                Mode::Undefined => RegBank::Undefined,
            };

            if let Some(_table_token) = ui.begin_table_with_flags(
                "regs",
                4,
                TableFlags::BORDERS_INNER_V | TableFlags::SIZING_FIXED_FIT | TableFlags::NO_CLIP,
            ) {
                regs_32(
                    ui,
                    0,
                    &reg_values.gprs,
                    |i, value| {
                        messages.push_custom((cpu_reg_bank, RegIndex::new(i as u8), value));
                    },
                    regs_32_default_label,
                    |i| {
                        if i & 3 == 0 {
                            ui.table_next_column();
                        }
                    },
                );
            }

            ui.separator();

            let psr_fields: &'static [BitfieldCommand] = if ARM9 {
                &[
                    BitfieldCommand::Field("Mode", 5),
                    BitfieldCommand::Field("T", 1),
                    BitfieldCommand::Field("F", 1),
                    BitfieldCommand::Field("I", 1),
                    BitfieldCommand::Skip(19),
                    BitfieldCommand::Field("Q", 1),
                    BitfieldCommand::Field("V", 1),
                    BitfieldCommand::Field("C", 1),
                    BitfieldCommand::Field("Z", 1),
                    BitfieldCommand::Field("N", 1),
                ]
            } else {
                &[
                    BitfieldCommand::Field("Mode", 5),
                    BitfieldCommand::Field("T", 1),
                    BitfieldCommand::Field("F", 1),
                    BitfieldCommand::Field("I", 1),
                    BitfieldCommand::Skip(20),
                    BitfieldCommand::Field("V", 1),
                    BitfieldCommand::Field("C", 1),
                    BitfieldCommand::Field("Z", 1),
                    BitfieldCommand::Field("N", 1),
                ]
            };

            ui.text("CPSR:");
            {
                let _frame_rounding = ui.push_style_var(StyleVar::FrameRounding(0.0));
                bitfield(ui, 2.0, false, true, cpsr.raw(), psr_fields);
            }

            ui.text(&format!("Mode: {}", psr_mode_to_str(mode),));

            ui.separator();

            ui.text("SPSR: ");
            if mode.is_exception() {
                {
                    let _frame_rounding = ui.push_style_var(StyleVar::FrameRounding(0.0));
                    bitfield(ui, 2.0, false, true, reg_values.spsr.raw(), psr_fields);
                }

                ui.text(&format!(
                    "Mode: {}",
                    match reg_values.spsr.mode() {
                        Ok(mode) => psr_mode_to_str(mode),
                        Err(_) => "Invalid",
                    },
                ));
            } else {
                ui.same_line();
                ui.text("None");
            }

            ui.separator();

            static REG_BANKS: [Option<RegBank>; 7] = [
                None,
                Some(RegBank::User),
                Some(RegBank::Fiq),
                Some(RegBank::Irq),
                Some(RegBank::Supervisor),
                Some(RegBank::Abort),
                Some(RegBank::Undefined),
            ];

            ui.align_text_to_frame_padding();
            ui.text("Banked registers: ");
            ui.same_line();
            let mut reg_bank_index = REG_BANKS.iter().position(|b| *b == self.reg_bank).unwrap();
            if ui.combo("##reg_bank", &mut reg_bank_index, &REG_BANKS, |reg_bank| {
                if let Some(reg_bank) = reg_bank {
                    let mut label = match reg_bank {
                        RegBank::User => "User",
                        RegBank::Fiq => "Fiq",
                        RegBank::Irq => "Irq",
                        RegBank::Supervisor => "Supervisor",
                        RegBank::Abort => "Abort",
                        RegBank::Undefined => "Undefined",
                    }
                    .to_string();
                    if *reg_bank == cpu_reg_bank {
                        label.push_str(" (current)");
                    }
                    label.into()
                } else {
                    "None".into()
                }
            }) {
                self.reg_bank = REG_BANKS[reg_bank_index];
            }

            if let Some(reg_bank) = self.reg_bank {
                let mut child_bg_color = ui.style_color(StyleColor::WindowBg);
                for component in &mut child_bg_color[..3] {
                    *component += (0.5 - *component) * 0.33;
                }
                child_bg_color[3] *= 0.33;

                let (cell_padding, item_spacing) = unsafe {
                    let style = ui.style();
                    (style.cell_padding[1], style.item_spacing[1])
                };

                let mut child_height = 3.0 * ui.frame_height_with_spacing() + 2.0 * cell_padding;
                if reg_bank != RegBank::User {
                    child_height += 2.0 * item_spacing + ui.text_line_height();
                    if reg_bank != cpu_reg_bank {
                        child_height +=
                            ui.frame_height() + 2.0 * ui.text_line_height() + 3.0 * item_spacing;
                    }
                }

                layout_group(ui, child_height, Some(child_bg_color), |window_padding_x| {
                    let (bank_str, r8_r12, r13_14, spsr) = match reg_bank {
                        RegBank::User => (
                            "usr",
                            &reg_values.r8_12_other[..],
                            &reg_values.r13_14_user[..],
                            None,
                        ),
                        RegBank::Fiq => (
                            "fiq",
                            &reg_values.r8_14_fiq[..5],
                            &reg_values.r8_14_fiq[5..],
                            Some(reg_values.spsr_fiq),
                        ),
                        RegBank::Irq => (
                            "irq",
                            &reg_values.r8_12_other[..],
                            &reg_values.r13_14_irq[..],
                            Some(reg_values.spsr_irq),
                        ),
                        RegBank::Supervisor => (
                            "svc",
                            &reg_values.r8_12_other[..],
                            &reg_values.r13_14_svc[..],
                            Some(reg_values.spsr_svc),
                        ),
                        RegBank::Abort => (
                            "abt",
                            &reg_values.r8_12_other[..],
                            &reg_values.r13_14_abt[..],
                            Some(reg_values.spsr_abt),
                        ),
                        RegBank::Undefined => (
                            "und",
                            &reg_values.r8_12_other[..],
                            &reg_values.r13_14_und[..],
                            Some(reg_values.spsr_und),
                        ),
                    };

                    if let Some(_table_token) = ui.begin_table_with_flags(
                        "banked_regs",
                        3,
                        TableFlags::BORDERS_INNER_V
                            | TableFlags::NO_CLIP
                            | TableFlags::SIZING_STRETCH_PROP,
                    ) {
                        let regs_32_label = |i: usize, max_digits| {
                            format!(
                                "r{}_{}: {:<len$}",
                                i,
                                bank_str,
                                "",
                                len = (max_digits - i.log10()) as usize
                            )
                        };
                        if (reg_bank != RegBank::Fiq && cpu_reg_bank != RegBank::Fiq)
                            || reg_bank == cpu_reg_bank
                        {
                            for i in 0..5 {
                                if i % 3 == 0 {
                                    ui.table_next_column();
                                }
                                ui.align_text_to_frame_padding();
                                ui.text(&format!(
                                    "r{}_{}: {}",
                                    i,
                                    bank_str,
                                    if i < 10 { " " } else { "" }
                                ));
                                ui.same_line();
                                ui.text("<cur>");
                            }
                        } else {
                            regs_32(
                                ui,
                                8,
                                r8_r12,
                                |i, value| {
                                    messages.push_custom((reg_bank, RegIndex::new(i as u8), value));
                                },
                                regs_32_label,
                                |i| {
                                    if (i - 8) % 3 == 0 {
                                        ui.table_next_column();
                                    }
                                },
                            );
                        }
                        ui.table_next_column();
                        if reg_bank == cpu_reg_bank {
                            for i in 13..15 {
                                ui.align_text_to_frame_padding();
                                ui.text(&format!("r{}_{}: {:<1}", i, bank_str, ""));
                                ui.same_line();
                                ui.text("<cur>");
                                ui.same_line();
                                ui.dummy([window_padding_x, 0.0]);
                            }
                        } else {
                            regs_32(
                                ui,
                                13,
                                r13_14,
                                |i, value| {
                                    messages.push_custom((reg_bank, RegIndex::new(i as u8), value));
                                },
                                regs_32_label,
                                |i| {
                                    if i == 14 {
                                        ui.same_line();
                                        ui.dummy([window_padding_x, 0.0]);
                                    }
                                },
                            );
                            ui.same_line();
                            ui.dummy([window_padding_x, 0.0]);
                        }
                    }

                    if let Some(spsr) = spsr {
                        separator_with_width(ui, -window_padding_x);

                        ui.text(&format!("SPSR_{}:", bank_str));
                        if reg_bank == cpu_reg_bank {
                            ui.same_line();
                            ui.text("<current>");
                        } else {
                            {
                                let _frame_rounding =
                                    ui.push_style_var(StyleVar::FrameRounding(0.0));
                                bitfield(ui, 2.0, false, true, spsr.raw(), psr_fields);
                            }

                            ui.text(&format!(
                                "Mode: {}",
                                match spsr.mode() {
                                    Ok(mode) => psr_mode_to_str(mode),
                                    Err(_) => "Invalid",
                                },
                            ));
                        }
                    }
                });
            }
        }
        None
    }
}
