mod arm;
mod thumb;

#[cfg(feature = "interp-pipeline")]
use super::common::{thumb_pipeline_entry, PipelineEntry};
use super::{super::Regs as EngineRegs, common::StateSource, Interpreter, Regs};
#[cfg(feature = "debugger-hooks")]
use crate::cpu::debug;
#[cfg(feature = "interp-arm9-interlocks")]
use crate::schedule::SignedTimestamp;
use crate::{
    cpu::{
        arm9::{bus, Arm9, Event, Timestamp},
        bus::CpuAccess,
        hle_bios,
        psr::{Mode, Psr},
        Arm9Data, CoreData, Schedule as _,
    },
    ds_slot::DsSlot,
    emu::Emu,
    gpu::engine_3d::Engine3d,
    utils::{schedule::RawTimestamp, Savestate},
};
use core::intrinsics::unlikely;

#[cfg(feature = "interp-arm9-interlocks")]
#[derive(Clone, Copy, Savestate)]
struct Interlock {
    port_ab: RawTimestamp,
    port_c: RawTimestamp,
}

#[derive(Savestate)]
#[load(in_place_only)]
pub struct EngineData {
    #[cfg(feature = "interp-pipeline-accurate-reloads")]
    r15_increment: u32,
    pub regs: Regs,
    #[cfg(feature = "interp-pipeline")]
    pipeline: [PipelineEntry; 2],
    #[cfg(not(feature = "interp-pipeline"))]
    thumb_next_instr: u16,
    #[cfg(feature = "interp-arm9-interlocks")]
    bus_cycle: RawTimestamp,
    #[cfg(feature = "interp-arm9-interlocks")]
    interlocks: [Interlock; 16],
    data_cycles: u8,
    #[cfg(feature = "debugger-hooks")]
    #[savestate(skip)]
    next_breakpoint_addr: u32,
    exc_vectors_start: u32,
}

impl EngineData {
    pub const fn new() -> Self {
        EngineData {
            #[cfg(feature = "interp-pipeline-accurate-reloads")]
            r15_increment: 4,
            regs: Regs::STARTUP,
            #[cfg(feature = "interp-pipeline")]
            pipeline: [0; 2],
            #[cfg(not(feature = "interp-pipeline"))]
            thumb_next_instr: 0,
            #[cfg(feature = "interp-arm9-interlocks")]
            bus_cycle: 0,
            #[cfg(feature = "interp-arm9-interlocks")]
            interlocks: [Interlock {
                port_ab: 0,
                port_c: 0,
            }; 16],
            data_cycles: 0,
            #[cfg(feature = "debugger-hooks")]
            next_breakpoint_addr: 0xFFFF_FFFF,
            exc_vectors_start: 0xFFFF_0000,
        }
    }
}

#[inline]
fn add_cycles(emu: &mut Emu<Interpreter>, cycles: RawTimestamp) {
    emu.arm9
        .schedule
        .set_cur_time(emu.arm9.schedule.cur_time() + Timestamp(cycles));
}

const ARM_BKPT: u32 = 0xE120_0070;
const THUMB_BKPT: u16 = 0xBE00;

fn prefetch_arm<const RESET_DATA_CYCLES: bool, const INC_R15: bool>(emu: &mut Emu<Interpreter>) {
    #[cfg(feature = "interp-arm9-interlocks")]
    let fetch_addr = reg!(emu.arm9, 15);
    if INC_R15 {
        inc_r15!(emu.arm9, 4);
    }
    #[cfg(feature = "interp-arm9-interlocks")]
    {
        if unlikely(!can_execute(
            emu,
            fetch_addr,
            emu.arm9.engine_data.regs.is_in_priv_mode(),
        )) {
            // Cause a prefetch abort when run by replacing the prefetced instruction with BKPT
            emu.arm9.engine_data.pipeline[1] = ARM_BKPT as PipelineEntry;
            add_cycles(emu, emu.arm9.engine_data.data_cycles as RawTimestamp);
        } else {
            let instr = bus::read_32::<CpuAccess, _, true>(emu, fetch_addr);
            let cycles = emu.arm9.cp15.timings.get(fetch_addr).code;
            emu.arm9.engine_data.pipeline[1] = instr as PipelineEntry;
            add_cycles(
                emu,
                cycles.max(emu.arm9.engine_data.data_cycles) as RawTimestamp,
            );
        }
        if RESET_DATA_CYCLES {
            emu.arm9.engine_data.data_cycles = 1;
        }
    }
}

fn prefetch_thumb<const RESET_DATA_CYCLES: bool, const INC_R15: bool>(emu: &mut Emu<Interpreter>) {
    #[cfg(feature = "interp-arm9-interlocks")]
    let fetch_addr = reg!(emu.arm9, 15);
    if INC_R15 {
        inc_r15!(emu.arm9, 2);
    }
    #[cfg(feature = "interp-arm9-interlocks")]
    {
        // NOTE: The ARM9 should actually only merge thumb code fetches from the system bus and not
        // from TCM, but timings are the same as long as concurrent data/code access waitstates are
        // not emulated.
        if fetch_addr & 2 == 0 {
            if unlikely(!can_execute(
                emu,
                fetch_addr,
                emu.arm9.engine_data.regs.is_in_priv_mode(),
            )) {
                // Cause a prefetch abort when run by replacing the prefetced instructions with BKPT
                emu.arm9.engine_data.pipeline[1] = thumb_pipeline_entry(
                    THUMB_BKPT as PipelineEntry | (THUMB_BKPT as PipelineEntry) << 16,
                );
                add_cycles(emu, emu.arm9.engine_data.data_cycles as RawTimestamp);
            } else {
                let new_instrs = bus::read_32::<CpuAccess, _, true>(emu, fetch_addr);
                let cycles = emu.arm9.cp15.timings.get(fetch_addr).code;
                emu.arm9.engine_data.pipeline[1] =
                    thumb_pipeline_entry(new_instrs as PipelineEntry);
                add_cycles(
                    emu,
                    cycles.max(emu.arm9.engine_data.data_cycles) as RawTimestamp,
                );
            }
        } else {
            emu.arm9.engine_data.pipeline[1] =
                thumb_pipeline_entry(emu.arm9.engine_data.pipeline[1] >> 16);
            add_cycles(emu, emu.arm9.engine_data.data_cycles as RawTimestamp);
        }
        if RESET_DATA_CYCLES {
            emu.arm9.engine_data.data_cycles = 1;
        }
    }
}

#[cfg_attr(not(feature = "interp-arm9-interlocks"), allow(unused_variables))]
#[inline]
fn apply_reg_interlock_1<const PORT_C: bool>(emu: &mut Emu<Interpreter>, reg: u8) {
    #[cfg(feature = "interp-arm9-interlocks")]
    {
        let interlock = emu.arm9.engine_data.interlocks[reg as usize];
        let interlock_end = if PORT_C {
            interlock.port_c
        } else {
            interlock_c.port_ab
        };
        if emu.arm9.engine_data.bus_cycle < interlock_end {
            add_cycles(
                emu,
                (interlock_end - emu.arm9.engine_data.bus_cycle - 1)
                    + emu.arm9.engine_data.data_cycles as RawTimestamp,
            );
            emu.arm9.engine_data.data_cycles = 1;
            emu.arm9.engine_data.bus_cycle = interlock_end;
        }
    }
}

#[cfg_attr(not(feature = "interp-arm9-interlocks"), allow(unused_variables))]
#[inline]
fn apply_reg_interlocks_2<const OFFSET_A: u8, const B_PORT_C: bool>(
    emu: &mut Emu<Interpreter>,
    reg_a: u8,
    reg_b: u8,
) {
    #[cfg(feature = "interp-arm9-interlocks")]
    {
        let interlock_end = (emu.arm9.engine_data.interlocks[reg_a as usize].port_ab
            as SignedTimestamp
            - OFFSET_A as SignedTimestamp)
            .max({
                let interlock_b = emu.arm9.engine_data.interlocks[reg_b as usize];
                (if B_PORT_C {
                    interlock_b.port_c
                } else {
                    interlock_b.port_ab
                }) as SignedTimestamp
            });
        if (emu.arm9.engine_data.bus_cycle as SignedTimestamp) < interlock_end {
            add_cycles(
                emu,
                (interlock_end as RawTimestamp - emu.arm9.engine_data.bus_cycle - 1)
                    + emu.arm9.engine_data.data_cycles as RawTimestamp,
            );
            emu.arm9.engine_data.data_cycles = 1;
            emu.arm9.engine_data.bus_cycle = interlock_end as RawTimestamp;
        }
    }
}

#[cfg_attr(not(feature = "interp-arm9-interlocks"), allow(unused_variables))]
#[inline]
fn apply_reg_interlocks_3<const OFFSET_AB: u8, const C_PORT_C: bool>(
    emu: &mut Emu<Interpreter>,
    reg_a: u8,
    reg_b: u8,
    reg_c: u8,
) {
    #[cfg(feature = "interp-arm9-interlocks")]
    {
        let interlock_end = (emu.arm9.engine_data.interlocks[reg_a as usize].port_ab
            as SignedTimestamp
            - OFFSET_AB as SignedTimestamp)
            .max(
                emu.arm9.engine_data.interlocks[reg_b as usize].port_ab as SignedTimestamp
                    - OFFSET_AB as SignedTimestamp,
            )
            .max({
                let interlock_c = emu.arm9.engine_data.interlocks[reg_c as usize];
                (if C_PORT_C {
                    interlock_c.port_c
                } else {
                    interlock_c.port_ab
                }) as SignedTimestamp
            });
        if (emu.arm9.engine_data.bus_cycle as SignedTimestamp) < interlock_end {
            add_cycles(
                emu,
                (interlock_end as RawTimestamp - emu.arm9.engine_data.bus_cycle - 1)
                    + emu.arm9.engine_data.data_cycles as RawTimestamp,
            );
            emu.arm9.engine_data.data_cycles = 1;
            emu.arm9.engine_data.bus_cycle = interlock_end as RawTimestamp;
        }
    }
}

// NOTE: Clearing interlocks for the C port is never necessary as they'll at most be one-cycle,
// which will be ignored by everything but the next instruction (which will be the one running
// this).

#[inline]
fn write_reg_clear_interlock_ab(emu: &mut Emu<Interpreter>, reg: u8, value: u32) {
    #[cfg(feature = "interp-arm9-interlocks")]
    {
        emu.arm9.engine_data.interlocks[reg as usize].port_ab = 0;
    }
    reg!(emu.arm9, reg) = value;
}

#[inline]
fn write_reg_interlock_ab(emu: &mut Emu<Interpreter>, reg: u8, value: u32, _offset: RawTimestamp) {
    #[cfg(feature = "interp-arm9-interlocks")]
    {
        emu.arm9.engine_data.interlocks[reg as usize].port_ab =
            emu.arm9.engine_data.bus_cycle + _offset;
    }
    reg!(emu.arm9, reg) = value;
}

#[inline]
fn write_reg_interlock(
    emu: &mut Emu<Interpreter>,
    reg: u8,
    value: u32,
    _port_ab_offset: RawTimestamp,
    _port_c_offset: RawTimestamp,
) {
    #[cfg(feature = "interp-arm9-interlocks")]
    {
        emu.arm9.engine_data.interlocks[reg as usize] = Interlock {
            port_ab: emu.arm9.engine_data.bus_cycle + _port_ab_offset,
            port_c: emu.arm9.engine_data.bus_cycle + _port_c_offset,
        };
    }
    reg!(emu.arm9, reg) = value;
}

#[cfg_attr(not(feature = "interp-arm9-interlocks"), allow(unused_variables))]
#[inline]
fn add_interlock(
    emu: &mut Emu<Interpreter>,
    reg: u8,
    port_ab_offset: RawTimestamp,
    port_c_offset: RawTimestamp,
) {
    #[cfg(feature = "interp-arm9-interlocks")]
    {
        emu.arm9.engine_data.interlocks[reg as usize] = Interlock {
            port_ab: emu.arm9.engine_data.bus_cycle + port_ab_offset,
            port_c: emu.arm9.engine_data.bus_cycle + port_c_offset,
        };
    }
}

/// Add a specific amount of bus cycles to an internal counter used for interlock calculations.
///
/// NOTE: only values <= 2 are needed for any single instruction, as interlocks will only ever last
/// up to 2 cycles.
#[cfg_attr(not(feature = "interp-arm9-interlocks"), allow(unused_variables))]
#[inline]
fn add_bus_cycles(emu: &mut Emu<Interpreter>, cycles: RawTimestamp) {
    #[cfg(feature = "interp-arm9-interlocks")]
    {
        emu.arm9.engine_data.bus_cycle += cycles;
    }
}

macro_rules! get_next_breakpoint {
    ($emu: expr, $addr: expr, $mask: expr) => {
        #[cfg(feature = "debugger-hooks")]
        if !$emu.arm9.debug.breakpoints.is_empty() {
            let (Ok(i) | Err(i)) = $emu.arm9.debug.breakpoints.binary_search(&$addr);
            $emu.arm9.engine_data.next_breakpoint_addr =
                $emu.arm9.debug.breakpoints[if i >= $emu.arm9.debug.breakpoints.len() {
                    0
                } else {
                    i
                }] & !$mask;
        }
    };
}

fn reload_pipeline<const STATE_SOURCE: StateSource>(emu: &mut Emu<Interpreter>) {
    let mut addr = reg!(emu.arm9, 15);

    if match STATE_SOURCE {
        StateSource::Arm => false,
        StateSource::Thumb => true,
        StateSource::R15Bit0 => {
            let thumb = addr & 1 != 0;
            emu.arm9.engine_data.regs.cpsr.set_thumb_state(thumb);
            #[cfg(feature = "interp-pipeline-accurate-reloads")]
            {
                emu.arm9.engine_data.r15_increment = 4 >> thumb as u8;
            }
            thumb
        }
        StateSource::Cpsr => emu.arm9.engine_data.regs.cpsr.thumb_state(),
    } {
        addr &= !1;
        get_next_breakpoint!(emu, addr, 1);
        // NOTE: The ARM9 should actually only merge thumb code fetches from the system bus and not
        // from TCM, but timings are the same as long as concurrent data/code access waitstates are
        // not emulated.
        #[cfg(feature = "interp-pipeline")]
        if addr & 2 == 0 {
            if unlikely(!can_execute(
                emu,
                addr,
                emu.arm9.engine_data.regs.is_in_priv_mode(),
            )) {
                emu.arm9.engine_data.pipeline = [
                    thumb_pipeline_entry(THUMB_BKPT as PipelineEntry),
                    thumb_pipeline_entry(THUMB_BKPT as PipelineEntry),
                ];
                add_cycles(emu, 2);
            } else {
                let instrs = bus::read_32::<CpuAccess, _, true>(emu, addr);
                emu.arm9.engine_data.pipeline = [
                    thumb_pipeline_entry(instrs as PipelineEntry),
                    thumb_pipeline_entry((instrs >> 16) as PipelineEntry),
                ];
                let cycles = emu.arm9.cp15.timings.get(addr).code;
                add_cycles(emu, cycles as RawTimestamp + 1);
            }
            reg!(emu.arm9, 15) = addr.wrapping_add(4);
        } else {
            if unlikely(!can_execute(
                emu,
                addr,
                emu.arm9.engine_data.regs.is_in_priv_mode(),
            )) {
                emu.arm9.engine_data.pipeline[0] =
                    thumb_pipeline_entry(THUMB_BKPT as PipelineEntry);
                add_cycles(emu, 1);
            } else {
                let first_word = bus::read_32::<CpuAccess, _, true>(emu, addr);
                emu.arm9.engine_data.pipeline[0] =
                    thumb_pipeline_entry((first_word >> 16) as PipelineEntry);
                let first_cycles = emu.arm9.cp15.timings.get(addr).code;
                add_cycles(emu, first_cycles as RawTimestamp);
            }
            addr = addr.wrapping_add(4);
            if unlikely(!can_execute(
                emu,
                addr,
                emu.arm9.engine_data.regs.is_in_priv_mode(),
            )) {
                emu.arm9.engine_data.pipeline[1] = thumb_pipeline_entry(
                    THUMB_BKPT as PipelineEntry | (THUMB_BKPT as PipelineEntry) << 16,
                );
                add_cycles(emu, 1);
            } else {
                let second_word = bus::read_32::<CpuAccess, _, true>(emu, addr);
                emu.arm9.engine_data.pipeline[1] =
                    thumb_pipeline_entry(second_word as PipelineEntry);
                let second_cycles = emu.arm9.cp15.timings.get(addr).code;
                add_cycles(emu, second_cycles as RawTimestamp);
            }
            reg!(emu.arm9, 15) = addr;
        }
        #[cfg(not(feature = "interp-pipeline"))]
        {
            if addr & 2 == 0 {
                if unlikely(!can_execute(
                    emu,
                    addr,
                    emu.arm9.engine_data.regs.is_in_priv_mode(),
                )) {
                    add_cycles(emu, 2);
                } else {
                    let cycles = emu.arm9.cp15.timings.get(addr).code;
                    add_cycles(emu, cycles as RawTimestamp + 1);
                }
            } else if unlikely(!can_execute(
                emu,
                addr,
                emu.arm9.engine_data.regs.is_in_priv_mode(),
            )) {
                emu.arm9.engine_data.thumb_next_instr = THUMB_BKPT;
                add_cycles(emu, 1);
            } else {
                let instrs = bus::read_32::<CpuAccess, _, true>(emu, addr);
                emu.arm9.engine_data.thumb_next_instr = (instrs >> 16) as u16;
                let cycles = emu.arm9.cp15.timings.get(addr).code;
                add_cycles(emu, (cycles as RawTimestamp) << 1);
            }
            reg!(emu.arm9, 15) = addr.wrapping_add(4);
        }
    } else {
        addr &= !3;
        get_next_breakpoint!(emu, addr, 3);
        #[cfg(feature = "interp-pipeline")]
        {
            if unlikely(!can_execute(
                emu,
                addr,
                emu.arm9.engine_data.regs.is_in_priv_mode(),
            )) {
                emu.arm9.engine_data.pipeline[0] = ARM_BKPT as PipelineEntry;
                add_cycles(emu, 1);
            } else {
                let first_instr = bus::read_32::<CpuAccess, _, true>(emu, addr);
                emu.arm9.engine_data.pipeline[0] = first_instr as PipelineEntry;
                let first_cycles = emu.arm9.cp15.timings.get(addr).code;
                add_cycles(emu, first_cycles as RawTimestamp);
            }
            addr = addr.wrapping_add(4);
            if unlikely(!can_execute(
                emu,
                addr,
                emu.arm9.engine_data.regs.is_in_priv_mode(),
            )) {
                emu.arm9.engine_data.pipeline[1] = ARM_BKPT as PipelineEntry;
                add_cycles(emu, 1);
            } else {
                let second_instr = bus::read_32::<CpuAccess, _, true>(emu, addr);
                emu.arm9.engine_data.pipeline[1] = second_instr as PipelineEntry;
                let second_cycles = emu.arm9.cp15.timings.get(addr).code;
                add_cycles(emu, second_cycles as RawTimestamp);
            }
            reg!(emu.arm9, 15) = addr.wrapping_add(4);
        }
        #[cfg(not(feature = "interp-pipeline"))]
        {
            if unlikely(!can_execute(
                emu,
                addr,
                emu.arm9.engine_data.regs.is_in_priv_mode(),
            )) {
                add_cycles(emu, 2);
            } else {
                let cycles = emu.arm9.cp15.timings.get(addr).code;
                add_cycles(emu, (cycles as RawTimestamp) << 1);
            }
            reg!(emu.arm9, 15) = addr.wrapping_add(8);
        }
    }
}

#[inline]
fn set_cpsr_update_control(emu: &mut Emu<Interpreter>, value: Psr) {
    let prev_value = emu.arm9.engine_data.regs.cpsr;
    emu.arm9.engine_data.regs.cpsr = value;
    emu.arm9
        .irqs
        .set_enabled_in_cpsr(!value.irqs_disabled(), &mut emu.arm9.schedule);
    emu.arm9
        .engine_data
        .regs
        .update_mode::<false>(prev_value.mode(), value.mode());
}

fn restore_spsr(emu: &mut Emu<Interpreter>) {
    if !emu.arm9.engine_data.regs.has_spsr() {
        unimplemented!("Unpredictable SPSR restore in non-exception mode");
    }
    set_cpsr_update_control(emu, emu.arm9.engine_data.regs.spsr);
    #[cfg(feature = "interp-pipeline-accurate-reloads")]
    {
        emu.arm9.engine_data.r15_increment =
            4 >> emu.arm9.engine_data.regs.cpsr.thumb_state() as u8;
    }
}

fn handle_undefined<const THUMB: bool>(emu: &mut Emu<Interpreter>) {
    #[cfg(feature = "log")]
    slog::warn!(
        emu.arm9.logger,
        "Undefined instruction @ {:#X} ({} state)",
        reg!(emu.arm9, 15).wrapping_sub(8 >> THUMB as u8),
        if THUMB { "Thumb" } else { "ARM" },
    );
    #[cfg(feature = "debugger-hooks")]
    if let Some(undef_hook) = emu.arm9.undef_hook() {
        if unsafe { undef_hook.get()(emu) } {
            emu.arm9
                .schedule
                .set_target_time(emu.arm9.schedule.cur_time());
            emu.arm9.stopped_by_debug_hook = true;
        }
    }
    if THUMB {
        prefetch_thumb::<false, false>(emu);
    } else {
        prefetch_arm::<false, false>(emu);
    }
    #[cfg(feature = "interp-arm9-interlocks")]
    {
        emu.arm9.engine_data.data_cycles = 1;
    }
    add_bus_cycles(emu, 2);
    let prev_cpsr = emu.arm9.engine_data.regs.cpsr;
    emu.arm9.engine_data.regs.cpsr = emu
        .arm9
        .engine_data
        .regs
        .cpsr
        .with_mode(Mode::UNDEFINED)
        .with_thumb_state(false)
        .with_irqs_disabled(true);
    emu.arm9.irqs.set_enabled_in_cpsr(false, ());
    #[cfg(feature = "interp-pipeline-accurate-reloads")]
    {
        emu.arm9.engine_data.r15_increment = 4;
    }
    emu.arm9
        .engine_data
        .regs
        .update_mode::<false>(prev_cpsr.mode(), Mode::UNDEFINED);
    emu.arm9.engine_data.regs.spsr = prev_cpsr;
    reg!(emu.arm9, 14) = reg!(emu.arm9, 15).wrapping_sub(4 >> THUMB as u8);
    reg!(emu.arm9, 15) = emu.arm9.engine_data.exc_vectors_start | 0x4;
    reload_pipeline::<{ StateSource::Arm }>(emu);
}

fn handle_swi<const THUMB: bool>(emu: &mut Emu<Interpreter>, number: u8) {
    #[cfg(feature = "debugger-hooks")]
    if let Some(swi_hook) = emu.arm9.swi_hook() {
        if unsafe { swi_hook.get()(emu, number) } {
            emu.arm9
                .schedule
                .set_target_time(emu.arm9.schedule.cur_time());
            emu.arm9.stopped_by_debug_hook = true;
        }
    }

    if emu.arm9.hle_bios_enabled() && emu.arm9.cp15.control().high_exc_vectors() {
        enter_hle_swi::<true>(
            emu,
            number,
            reg!(emu.arm9, 15).wrapping_sub(4 >> THUMB as u8),
        );
        return;
    }

    if THUMB {
        prefetch_thumb::<false, false>(emu);
    } else {
        prefetch_arm::<false, false>(emu);
    }
    #[cfg(feature = "interp-arm9-interlocks")]
    {
        emu.arm9.engine_data.data_cycles = 1;
    }
    add_bus_cycles(emu, 2);
    let prev_cpsr = emu.arm9.engine_data.regs.cpsr;
    emu.arm9.engine_data.regs.cpsr = prev_cpsr
        .with_mode(Mode::SUPERVISOR)
        .with_thumb_state(false)
        .with_irqs_disabled(true);
    emu.arm9.irqs.set_enabled_in_cpsr(false, ());
    #[cfg(feature = "interp-pipeline-accurate-reloads")]
    {
        emu.arm9.engine_data.r15_increment = 4;
    }
    emu.arm9
        .engine_data
        .regs
        .update_mode::<false>(prev_cpsr.mode(), Mode::SUPERVISOR);
    emu.arm9.engine_data.regs.spsr = prev_cpsr;
    reg!(emu.arm9, 14) = reg!(emu.arm9, 15).wrapping_sub(4 >> THUMB as u8);
    reg!(emu.arm9, 15) = emu.arm9.engine_data.exc_vectors_start | 0x8;
    reload_pipeline::<{ StateSource::Arm }>(emu);
}

fn handle_prefetch_abort<const THUMB: bool>(emu: &mut Emu<Interpreter>) {
    #[cfg(feature = "log")]
    slog::warn!(
        emu.arm9.logger,
        "Prefetch abort @ {:#X} ({} state)",
        reg!(emu.arm9, 15).wrapping_sub(8 >> THUMB as u8),
        if THUMB { "Thumb" } else { "ARM" },
    );
    #[cfg(feature = "debugger-hooks")]
    if let Some(prefetch_abort_hook) = emu.arm9.prefetch_abort_hook() {
        if unsafe { prefetch_abort_hook.get()(emu) } {
            emu.arm9
                .schedule
                .set_target_time(emu.arm9.schedule.cur_time());
            emu.arm9.stopped_by_debug_hook = true;
        }
    }
    if THUMB {
        prefetch_thumb::<false, false>(emu);
    } else {
        prefetch_arm::<false, false>(emu);
    }
    #[cfg(feature = "interp-arm9-interlocks")]
    {
        emu.arm9.engine_data.data_cycles = 1;
    }
    add_bus_cycles(emu, 2);
    let prev_cpsr = emu.arm9.engine_data.regs.cpsr;
    emu.arm9.engine_data.regs.cpsr = prev_cpsr
        .with_mode(Mode::ABORT)
        .with_thumb_state(false)
        .with_irqs_disabled(true);
    emu.arm9.irqs.set_enabled_in_cpsr(false, ());
    #[cfg(feature = "interp-pipeline-accurate-reloads")]
    {
        emu.arm9.engine_data.r15_increment = 4;
    }
    emu.arm9
        .engine_data
        .regs
        .update_mode::<false>(prev_cpsr.mode(), Mode::ABORT);
    emu.arm9.engine_data.regs.spsr = prev_cpsr;
    reg!(emu.arm9, 14) = reg!(emu.arm9, 15).wrapping_sub((!THUMB as u32) << 2);
    reg!(emu.arm9, 15) = emu.arm9.engine_data.exc_vectors_start | 0xC;
    reload_pipeline::<{ StateSource::Arm }>(emu);
}

fn handle_data_abort<const THUMB: bool>(emu: &mut Emu<Interpreter>, _addr: u32) {
    // r15 is assumed to be PC + 3i, and not PC + 2i (where i = instr size)
    #[cfg(feature = "log")]
    slog::warn!(
        emu.arm9.logger,
        "Data abort @ {:#X} ({} state) accessing {:#X}",
        reg!(emu.arm9, 15).wrapping_sub(12 >> THUMB as u8),
        if THUMB { "Thumb" } else { "ARM" },
        _addr,
    );
    #[cfg(feature = "debugger-hooks")]
    if let Some(data_abort_hook) = emu.arm9.data_abort_hook() {
        if unsafe { data_abort_hook.get()(emu, _addr) } {
            emu.arm9
                .schedule
                .set_target_time(emu.arm9.schedule.cur_time());
            emu.arm9.stopped_by_debug_hook = true;
        }
    }
    let prev_cpsr = emu.arm9.engine_data.regs.cpsr;
    emu.arm9.engine_data.regs.cpsr = prev_cpsr
        .with_mode(Mode::ABORT)
        .with_thumb_state(false)
        .with_irqs_disabled(true);
    emu.arm9.irqs.set_enabled_in_cpsr(false, ());
    #[cfg(feature = "interp-pipeline-accurate-reloads")]
    {
        emu.arm9.engine_data.r15_increment = 4;
    }
    emu.arm9
        .engine_data
        .regs
        .update_mode::<false>(prev_cpsr.mode(), Mode::ABORT);
    emu.arm9.engine_data.regs.spsr = prev_cpsr;
    reg!(emu.arm9, 14) = if THUMB {
        reg!(emu.arm9, 15).wrapping_add(2)
    } else {
        reg!(emu.arm9, 15).wrapping_sub(4)
    };
    reg!(emu.arm9, 15) = emu.arm9.engine_data.exc_vectors_start | 0x10;
    reload_pipeline::<{ StateSource::Arm }>(emu);
}

fn enter_hle_swi<const FROM_USER_CODE: bool>(
    emu: &mut Emu<Interpreter>,
    number: u8,
    return_addr: u32,
) {
    let spsr = if FROM_USER_CODE {
        emu.arm9.engine_data.regs.cpsr
    } else {
        spsr!(emu.arm9)
    };

    if FROM_USER_CODE {
        let prev_mode = emu.arm9.engine_data.regs.cpsr.mode();
        emu.arm9.engine_data.regs.cpsr.set_mode(Mode::SUPERVISOR);
        emu.arm9
            .engine_data
            .regs
            .update_mode::<false>(prev_mode, Mode::SUPERVISOR);
        reg!(emu.arm9, 14) = return_addr;
        emu.arm9.engine_data.regs.spsr = spsr;
    }

    reg!(emu.arm9, 13) = reg!(emu.arm9, 13).wrapping_sub(0x10);
    let base_addr = reg!(emu.arm9, 13);
    for (i, value) in [
        spsr.raw(),
        reg!(emu.arm9, 11),
        reg!(emu.arm9, 12),
        reg!(emu.arm9, 14),
    ]
    .into_iter()
    .enumerate()
    {
        bus::write_32::<CpuAccess, _>(emu, base_addr.wrapping_add((i << 2) as u32), value);
    }

    let new_cpsr = Psr::from_raw((spsr.raw() & 0x80) | 0x1F);
    reg!(emu.arm9, 11) = new_cpsr.raw();
    set_cpsr_update_control(emu, new_cpsr);

    let base_addr = reg!(emu.arm9, 13).wrapping_sub(8);
    reg!(emu.arm9, 13) = base_addr;
    for (i, reg) in [2_u8, 14].into_iter().enumerate() {
        bus::write_32::<CpuAccess, _>(
            emu,
            base_addr.wrapping_add((i << 2) as u32),
            reg!(emu.arm9, reg),
        );
    }

    hle_bios::arm9::handle_swi(emu, number, emu.arm9.engine_data.regs.r0_3());
}

fn return_from_hle_swi(emu: &mut Emu<Interpreter>) {
    let base_addr = reg!(emu.arm9, 13);
    reg!(emu.arm9, 13) = base_addr.wrapping_add(8);
    for (i, reg) in [2_u8, 14].into_iter().enumerate() {
        reg!(emu.arm9, reg) =
            bus::read_32::<CpuAccess, _, false>(emu, base_addr.wrapping_add((i << 2) as u32));
    }

    let prev_cpsr = emu.arm9.engine_data.regs.cpsr;
    emu.arm9.engine_data.regs.cpsr = prev_cpsr.with_mode(Mode::SUPERVISOR);
    emu.arm9
        .engine_data
        .regs
        .update_mode::<false>(prev_cpsr.mode(), Mode::SUPERVISOR);

    let base_addr = reg!(emu.arm9, 13);
    reg!(emu.arm9, 13) = base_addr.wrapping_add(0x10);
    emu.arm9.engine_data.regs.spsr =
        Psr::from_raw_masked::<true>(bus::read_32::<CpuAccess, _, false>(emu, base_addr));
    for (i, reg) in [11_u8, 12, 14].into_iter().enumerate() {
        reg!(emu.arm9, reg) =
            bus::read_32::<CpuAccess, _, false>(emu, base_addr.wrapping_add(((i + 1) << 2) as u32));
    }

    reg!(emu.arm9, 15) = reg!(emu.arm9, 14);
    restore_spsr(emu);
    reload_pipeline::<{ StateSource::Cpsr }>(emu);
}

fn enter_hle_irq<const FROM_USER_CODE: bool>(emu: &mut Emu<Interpreter>, return_addr: u32) {
    if FROM_USER_CODE {
        let prev_cpsr = emu.arm9.engine_data.regs.cpsr;
        emu.arm9.engine_data.regs.cpsr = prev_cpsr.with_mode(Mode::IRQ).with_irqs_disabled(true);
        emu.arm9.irqs.set_enabled_in_cpsr(false, ());
        emu.arm9
            .engine_data
            .regs
            .update_mode::<false>(prev_cpsr.mode(), Mode::IRQ);
        emu.arm9.engine_data.regs.spsr = prev_cpsr;
    }

    let base_addr = reg!(emu.arm9, 13).wrapping_sub(0x18);
    reg!(emu.arm9, 13) = base_addr;
    for (i, reg) in [0_u8, 1, 2, 3, 12].into_iter().enumerate() {
        bus::write_32::<CpuAccess, _>(
            emu,
            base_addr.wrapping_add((i << 2) as u32),
            reg!(emu.arm9, reg),
        );
    }
    bus::write_32::<CpuAccess, _>(emu, base_addr.wrapping_add(0x14), return_addr);

    reg!(emu.arm9, 0) = hle_bios::arm9::handle_irq(emu);
}

fn return_from_hle_irq(emu: &mut Emu<Interpreter>) {
    let base_addr = reg!(emu.arm9, 13);
    reg!(emu.arm9, 13) = base_addr.wrapping_add(0x18);
    for (i, reg) in [0_u8, 1, 2, 3, 12, 14].into_iter().enumerate() {
        reg!(emu.arm9, reg) =
            bus::read_32::<CpuAccess, _, false>(emu, base_addr.wrapping_add((i << 2) as u32));
    }
    reg!(emu.arm9, 15) = reg!(emu.arm9, 14).wrapping_sub(4);
    restore_spsr(emu);
    reload_pipeline::<{ StateSource::Cpsr }>(emu);
}

#[allow(unused_variables)]
#[inline]
fn can_read(emu: &Emu<Interpreter>, addr: u32, privileged: bool) -> bool {
    #[cfg(feature = "pu-checks")]
    {
        emu.arm9.cp15.perm_map.read(addr, privileged)
    }
    #[cfg(not(feature = "pu-checks"))]
    true
}

#[allow(unused_variables)]
#[inline]
fn can_write(emu: &Emu<Interpreter>, addr: u32, privileged: bool) -> bool {
    #[cfg(feature = "pu-checks")]
    {
        emu.arm9.cp15.perm_map.write(addr, privileged)
    }
    #[cfg(not(feature = "pu-checks"))]
    true
}

#[allow(unused_variables)]
#[inline]
fn can_execute(emu: &Emu<Interpreter>, addr: u32, privileged: bool) -> bool {
    #[cfg(feature = "pu-checks")]
    {
        emu.arm9.cp15.perm_map.execute(addr, privileged)
    }
    #[cfg(not(feature = "pu-checks"))]
    true
}

impl CoreData for EngineData {
    type Engine = Interpreter;

    #[inline]
    fn setup(emu: &mut Emu<Interpreter>) {
        add_bus_cycles(emu, 2);
        reg!(emu.arm9, 15) = 0xFFFF_0000;
        reload_pipeline::<{ StateSource::Arm }>(emu);
    }

    fn setup_direct_boot(emu: &mut Emu<Interpreter>, entry_addr: u32) {
        let prev_mode = emu.arm9.engine_data.regs.cpsr.mode();
        emu.arm9.engine_data.regs.cpsr.set_mode(Mode::SYSTEM);
        emu.arm9
            .engine_data
            .regs
            .update_mode::<false>(prev_mode, Mode::SYSTEM);
        for reg in 0..12 {
            reg!(emu.arm9, reg) = 0;
        }
        reg!(emu.arm9, 12) = entry_addr;
        reg!(emu.arm9, 13) = 0x0300_2F7C;
        reg!(emu.arm9, 14) = entry_addr;
        emu.arm9.engine_data.regs.r13_14_irq[0] = 0x0300_3F80;
        emu.arm9.engine_data.regs.r13_14_svc[0] = 0x0300_3FC0;
        emu.arm9.engine_data.data_cycles = 1;
        #[cfg(feature = "interp-arm9-interlocks")]
        {
            emu.arm9.engine_data.bus_cycle = 0;
            emu.arm9.engine_data.interlocks = [Interlock {
                port_ab: 0,
                port_c: 0,
            }; 16];
        };
        reg!(emu.arm9, 15) = entry_addr;
        reload_pipeline::<{ StateSource::R15Bit0 }>(emu);
    }

    #[inline]
    fn post_load(_emu: &mut Emu<Interpreter>) {
        get_next_breakpoint!(
            _emu,
            reg!(_emu.arm9, 15),
            1 | (!_emu.arm9.engine_data.regs.cpsr.thumb_state() as u32) << 1
        );
    }

    #[inline]
    fn invalidate_word(&mut self, _addr: u32) {}

    #[inline]
    fn invalidate_word_range(&mut self, _bounds: (u32, u32)) {}

    #[inline]
    fn jump(emu: &mut Emu<Interpreter>, addr: u32) {
        reg!(emu.arm9, 15) = addr;
        reload_pipeline::<{ StateSource::R15Bit0 }>(emu);
    }

    #[inline]
    fn r15(&self) -> u32 {
        self.regs.cur[15]
    }

    #[inline]
    fn cpsr(&self) -> Psr {
        self.regs.cpsr
    }

    #[inline]
    fn set_cpsr(emu: &mut Emu<Interpreter>, value: Psr) {
        set_cpsr_update_control(emu, value);
    }

    #[inline]
    fn regs(&self) -> EngineRegs {
        self.regs.to_engine_regs()
    }

    #[inline]
    fn set_regs(emu: &mut Emu<Interpreter>, regs: &EngineRegs) {
        emu.arm9.engine_data.regs.set_from_engine_regs(regs);
        reg!(emu.arm9, 15) = reg!(emu.arm9, 15)
            .wrapping_sub(8 >> emu.arm9.engine_data.regs.cpsr.thumb_state() as u8);
        reload_pipeline::<{ StateSource::Cpsr }>(emu);
    }

    #[inline]
    fn jump_and_link(emu: &mut Emu<Interpreter>, addr: u32, lr: u32) {
        reg!(emu.arm9, 14) = lr;
        reg!(emu.arm9, 15) = addr;
        reload_pipeline::<{ StateSource::R15Bit0 }>(emu);
    }

    #[inline]
    fn return_from_hle_swi(emu: &mut Emu<Self::Engine>, r0_3: [u32; 4]) {
        emu.arm9.engine_data.regs.set_r0_3(r0_3);
        return_from_hle_swi(emu);
    }

    cfg_if::cfg_if! {
        if #[cfg(feature = "debugger-hooks")] {
            #[inline]
            fn set_swi_hook(&mut self, _hook: &Option<debug::SwiHook<Interpreter>>) {}

            #[inline]
            fn set_undef_hook(&mut self, _hook: &Option<debug::UndefHook<Interpreter>>) {}

            #[inline]
            fn add_breakpoint(&mut self, addr: u32) {
                if addr < self.next_breakpoint_addr
                    && addr
                        >= self.regs.cur[15].wrapping_sub(8 >> self.regs.cpsr.thumb_state() as u8)
                {
                    self.next_breakpoint_addr = addr;
                }
            }

            #[inline]
            fn remove_breakpoint(&mut self, addr: u32, i: usize, breakpoints: &[u32]) {
                if self.next_breakpoint_addr == addr {
                    self.next_breakpoint_addr = if breakpoints.is_empty() {
                        u32::MAX
                    } else {
                        breakpoints[if i == breakpoints.len() { 0 } else { i }]
                    };
                }
            }

            #[inline]
            fn clear_breakpoints(&mut self) {
                self.next_breakpoint_addr = u32::MAX;
            }

            #[inline]
            fn set_breakpoint_hook(&mut self, _hook: &Option<debug::BreakpointHook<Interpreter>>) {}

            #[inline]
            fn set_mem_watchpoint_hook(
                &mut self,
                _hook: &Option<debug::MemWatchpointHook<Interpreter>>,
            ) {}

            #[inline]
            fn add_mem_watchpoint(
                &mut self,
                _addr: u32,
                _size: u8,
                _rw: debug::MemWatchpointRwMask,
            ) {}

            #[inline]
            fn remove_mem_watchpoint(
                &mut self,
                _addr: u32,
                _size: u8,
                _rw: debug::MemWatchpointRwMask,
            ) {}

            #[inline]
            fn clear_mem_watchpoints(&mut self) {}
        }
    }
}

macro_rules! handle_pending_events {
    ($emu: expr, $handle_gx_fifo_stall: expr) => {
        while let Some((event, time)) = $emu.arm9.schedule.pop_pending_event() {
            match event {
                Event::DsSlotRomDataReady => DsSlot::handle_rom_data_ready($emu),
                Event::DsSlotSpiDataReady => $emu.ds_slot.handle_spi_data_ready(),
                Event::DivResultReady => $emu.arm9.div_engine.handle_result_ready(),
                Event::SqrtResultReady => $emu.arm9.sqrt_engine.handle_result_ready(),
                Event::Timer(i) => $emu.arm9.timers.handle_scheduled_overflow(
                    i,
                    time,
                    &mut $emu.arm9.schedule,
                    &mut $emu.arm9.irqs,
                ),
                Event::GxFifoStall => $handle_gx_fifo_stall,
                Event::Engine3dCommandFinished => Engine3d::process_next_command($emu),
            }
        }
    };
}

impl Arm9Data for EngineData {
    #[inline]
    fn set_high_exc_vectors(&mut self, value: bool) {
        self.exc_vectors_start = if value { 0xFFFF_0000 } else { 0 };
    }

    #[inline]
    fn set_t_bit_load_disabled(&mut self, _value: bool) {}

    cfg_if::cfg_if! {
        if #[cfg(feature = "debugger-hooks")] {
            #[inline]
            fn set_prefetch_abort_hook(
                &mut self,
                _hook: &Option<debug::PrefetchAbortHook<Interpreter>>,
            ) {}

            #[inline]
            fn set_data_abort_hook(&mut self, _hook: &Option<debug::DataAbortHook<Interpreter>>) {}
        }
    }

    #[inline]
    fn run_stalled_until(emu: &mut Emu<Interpreter>, end_time: Timestamp) {
        emu.arm9.schedule.set_cur_time(end_time);
        handle_pending_events!(emu, {});
    }

    #[inline]
    fn run_until(emu: &mut Emu<Interpreter>, end_time: Timestamp) {
        while emu.arm9.schedule.cur_time() < end_time {
            handle_pending_events!(emu, return);
            emu.arm9
                .schedule
                .set_target_time(emu.arm9.schedule.schedule().next_event_time().min(end_time));
            if let Some(channel) = emu.arm9.dma.cur_channel() {
                // TODO: Keep the ARM9 running while processing a DMA transfer if it doesn't use the
                //       system bus.
                Arm9::run_dma_transfer(emu, channel);
            } else {
                if emu.arm9.irqs.triggered() {
                    let prev_cpsr = emu.arm9.engine_data.regs.cpsr;
                    let return_addr =
                        reg!(emu.arm9, 15).wrapping_sub((!prev_cpsr.thumb_state() as u32) << 2);
                    if emu.arm9.hle_bios_enabled() && emu.arm9.cp15.control().high_exc_vectors() {
                        enter_hle_irq::<true>(emu, return_addr);
                    } else {
                        // Perform an extra instruction fetch before branching, like real hardware does,
                        // according to the ARM9E-S reference manual
                        add_bus_cycles(emu, 2);
                        #[cfg(feature = "interp-pipeline")]
                        {
                            let fetch_addr = reg!(emu.arm9, 15);
                            let cycles = if unlikely(!can_execute(
                                emu,
                                fetch_addr,
                                emu.arm9.engine_data.regs.is_in_priv_mode(),
                            )) {
                                1
                            } else {
                                bus::read_32::<CpuAccess, _, true>(emu, fetch_addr);
                                emu.arm9.cp15.timings.get(fetch_addr).code
                            };
                            add_cycles(
                                emu,
                                cycles.max(emu.arm9.engine_data.data_cycles) as RawTimestamp,
                            );
                            emu.arm9.engine_data.data_cycles = 1;
                        }
                        let prev_cpsr = emu.arm9.engine_data.regs.cpsr;
                        emu.arm9.engine_data.regs.cpsr = prev_cpsr
                            .with_mode(Mode::IRQ)
                            .with_thumb_state(false)
                            .with_irqs_disabled(true);
                        emu.arm9.irqs.set_enabled_in_cpsr(false, ());
                        #[cfg(feature = "interp-pipeline-accurate-reloads")]
                        {
                            emu.arm9.engine_data.r15_increment = 4;
                        }
                        emu.arm9
                            .engine_data
                            .regs
                            .update_mode::<false>(prev_cpsr.mode(), Mode::IRQ);
                        emu.arm9.engine_data.regs.spsr = prev_cpsr;
                        reg!(emu.arm9, 14) = return_addr;
                        reg!(emu.arm9, 15) = emu.arm9.engine_data.exc_vectors_start | 0x18;
                        reload_pipeline::<{ StateSource::Arm }>(emu);
                    }
                } else if emu.arm9.irqs.halted() {
                    emu.arm9
                        .schedule
                        .set_cur_time(emu.arm9.schedule.target_time());
                    continue;
                }
                while emu.arm9.schedule.cur_time() < emu.arm9.schedule.target_time() {
                    #[cfg(feature = "debugger-hooks")]
                    {
                        let r15 = reg!(emu.arm9, 15)
                            .wrapping_sub(8 >> emu.arm9.engine_data.regs.cpsr.thumb_state() as u8);
                        if emu.arm9.engine_data.next_breakpoint_addr == r15 {
                            if let Some(breakpoint_hook) = emu.arm9.breakpoint_hook().as_ref() {
                                if unsafe { breakpoint_hook.get()(emu, r15) } {
                                    emu.arm9
                                        .schedule
                                        .set_target_time(emu.arm9.schedule.cur_time());
                                    emu.arm9.stopped_by_debug_hook = true;
                                    return;
                                }
                            }
                        }
                    }
                    #[cfg(feature = "interp-pipeline")]
                    {
                        #[cfg(not(feature = "interp-arm9-interlocks"))]
                        let addr = reg!(emu.arm9, 15);
                        let instr = emu.arm9.engine_data.pipeline[0];
                        emu.arm9.engine_data.pipeline[0] = emu.arm9.engine_data.pipeline[1];
                        #[cfg(not(feature = "interp-arm9-interlocks"))]
                        if emu.arm9.engine_data.regs.cpsr.thumb_state() {
                            if addr & 2 == 0 {
                                if unlikely(!can_execute(
                                    emu,
                                    addr,
                                    emu.arm9.engine_data.regs.is_in_priv_mode(),
                                )) {
                                    // Cause a prefetch abort when run by replacing the prefetced
                                    // instructions with BKPT
                                    emu.arm9.engine_data.pipeline[1] = thumb_pipeline_entry(
                                        THUMB_BKPT as PipelineEntry
                                            | (THUMB_BKPT as PipelineEntry) << 16,
                                    );
                                    add_cycles(
                                        emu,
                                        emu.arm9.engine_data.data_cycles as RawTimestamp,
                                    );
                                } else {
                                    let new_instrs = bus::read_32::<CpuAccess, _, true>(emu, addr);
                                    let cycles = emu.arm9.cp15.timings.get(addr).code;
                                    emu.arm9.engine_data.pipeline[1] =
                                        thumb_pipeline_entry(new_instrs as PipelineEntry);
                                    add_cycles(
                                        emu,
                                        cycles.max(emu.arm9.engine_data.data_cycles)
                                            as RawTimestamp,
                                    );
                                }
                            } else {
                                emu.arm9.engine_data.pipeline[1] =
                                    thumb_pipeline_entry(emu.arm9.engine_data.pipeline[1] >> 16);
                                add_cycles(emu, emu.arm9.engine_data.data_cycles as RawTimestamp);
                            }
                            #[cfg(not(feature = "interp-pipeline-accurate-reloads"))]
                            {
                                emu.arm9.engine_data.data_cycles = 1;
                                thumb::handle_instr(emu, instr as u16);
                            }
                        } else {
                            if unlikely(!can_execute(
                                emu,
                                addr,
                                emu.arm9.engine_data.regs.is_in_priv_mode(),
                            )) {
                                // Cause a prefetch abort when run by replacing the prefetced
                                // instruction with BKPT
                                emu.arm9.engine_data.pipeline[1] = ARM_BKPT as PipelineEntry;
                                add_cycles(emu, emu.arm9.engine_data.data_cycles as RawTimestamp);
                            } else {
                                let new_instr = bus::read_32::<CpuAccess, _, true>(emu, addr);
                                let cycles = emu.arm9.cp15.timings.get(addr).code;
                                emu.arm9.engine_data.pipeline[1] = new_instr as PipelineEntry;
                                add_cycles(
                                    emu,
                                    cycles.max(emu.arm9.engine_data.data_cycles) as RawTimestamp,
                                );
                            }
                            #[cfg(not(feature = "interp-pipeline-accurate-reloads"))]
                            {
                                emu.arm9.engine_data.data_cycles = 1;
                                arm::handle_instr(emu, instr as u32);
                            }
                        }
                        #[cfg(feature = "interp-pipeline-accurate-reloads")]
                        {
                            emu.arm9.engine_data.data_cycles = 1;
                            if instr & 1 << 32 == 0 {
                                arm::handle_instr(emu, instr as u32);
                            } else {
                                thumb::handle_instr(emu, instr as u16);
                            }
                        }
                    }
                    #[cfg(not(feature = "interp-pipeline"))]
                    {
                        if emu.arm9.engine_data.regs.cpsr.thumb_state() {
                            let addr = reg!(emu.arm9, 15).wrapping_sub(4);
                            let instr = if addr & 2 == 0 {
                                if unlikely(!can_execute(
                                    emu,
                                    addr,
                                    emu.arm9.engine_data.regs.is_in_priv_mode(),
                                )) {
                                    add_cycles(
                                        emu,
                                        emu.arm9.engine_data.data_cycles as RawTimestamp,
                                    );
                                    THUMB_BKPT
                                } else {
                                    let instrs = bus::read_32::<CpuAccess, _, true>(emu, addr);
                                    add_cycles(
                                        emu,
                                        emu.arm9
                                            .cp15
                                            .timings
                                            .get(addr)
                                            .code
                                            .max(emu.arm9.engine_data.data_cycles)
                                            as RawTimestamp,
                                    );
                                    emu.arm9.engine_data.thumb_next_instr = (instrs >> 16) as u16;
                                    instrs as u16
                                }
                            } else {
                                add_cycles(emu, emu.arm9.engine_data.data_cycles as RawTimestamp);
                                emu.arm9.engine_data.thumb_next_instr
                            };
                            emu.arm9.engine_data.data_cycles = 1;
                            thumb::handle_instr(emu, instr);
                        } else {
                            let addr = reg!(emu.arm9, 15).wrapping_sub(8);
                            let instr = if unlikely(!can_execute(
                                emu,
                                addr,
                                emu.arm9.engine_data.regs.is_in_priv_mode(),
                            )) {
                                add_cycles(emu, emu.arm9.engine_data.data_cycles as RawTimestamp);
                                ARM_BKPT
                            } else {
                                let instr = bus::read_32::<CpuAccess, _, true>(emu, addr);
                                add_cycles(
                                    emu,
                                    emu.arm9
                                        .cp15
                                        .timings
                                        .get(addr)
                                        .code
                                        .max(emu.arm9.engine_data.data_cycles)
                                        as RawTimestamp,
                                );
                                instr
                            };
                            emu.arm9.engine_data.data_cycles = 1;
                            arm::handle_instr(emu, instr);
                        }
                    }
                }
            }
        }
    }
}
