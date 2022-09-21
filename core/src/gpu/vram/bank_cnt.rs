use super::{BankControl, Vram};
use crate::{
    cpu::{
        self,
        arm7::{self, bus::ptrs::mask as arm7_ptr_mask, Arm7},
        arm9::{bus::ptrs::mask as ptr_mask, Arm9},
    },
    gpu::engine_3d::Engine3d,
    utils::OwnedBytesCellPtr,
};
use core::{iter::once, mem::size_of, ops::Range};

// TODO: Find out what happens with invalid VRAM bank MST values.
// TODO: `generic_arg_infer` isn't exactly stable right now; when it is, remove the bank lengths
// that were manually specified in some unmap_* calls

macro_rules! map_cpu_visible {
    (
        $cpu: expr, $mask: expr, $cpu_start_addr: expr, $cpu_end_addr: expr,
        $usage: expr, $usage_len: literal, $region: expr, $region_shift: expr
    ) => {
        for mirror_base_addr in ($cpu_start_addr..$cpu_end_addr).step_by($usage_len) {
            let region_base_addr = mirror_base_addr | ($region as u32) << $region_shift;
            $cpu.map_sys_bus_ptr_range(
                $mask,
                $usage.as_ptr().add($region << $region_shift),
                1 << $region_shift,
                (
                    region_base_addr,
                    region_base_addr | ((1 << $region_shift) - 1),
                ),
            );
        }
    };
}

unsafe fn copy_slice_wrapping_unchecked_with_dst_range<
    const SRC_MASK: usize,
    const DST_LEN: usize,
    const SRC_LEN: usize,
>(
    dst: &OwnedBytesCellPtr<DST_LEN>,
    src: &OwnedBytesCellPtr<SRC_LEN>,
    dst_range: Range<usize>,
) {
    let mut dst = dst.as_byte_mut_slice();
    let src = src.as_byte_slice();
    let src_base_addr = dst_range.start & SRC_MASK;
    let copy_len = ((dst_range.end - 1) & SRC_MASK) - src_base_addr + 1;
    for dst_base_addr in dst_range.step_by(copy_len) {
        dst.get_unchecked_mut(dst_base_addr..dst_base_addr + copy_len)
            .copy_from_slice(src.get_unchecked(src_base_addr..src_base_addr + copy_len));
    }
}

unsafe fn or_assign_slice_wrapping_unchecked<
    const SRC_MASK: usize,
    const DST_LEN: usize,
    const SRC_LEN: usize,
>(
    dst: &OwnedBytesCellPtr<DST_LEN>,
    src: &OwnedBytesCellPtr<SRC_LEN>,
    dst_range: Range<usize>,
) {
    for dst_addr in dst_range.step_by(size_of::<usize>()) {
        dst.write_ne_aligned_unchecked(
            dst_addr,
            dst.read_ne_aligned_unchecked::<usize>(dst_addr)
                | src.read_ne_aligned_unchecked::<usize>(dst_addr & SRC_MASK),
        );
    }
}

macro_rules! map_region {
    (
        no_wb $self: expr,
        $usage: ident, $region_shift: expr,
        $bank: expr, $mask: expr, $bank_bit: expr, $region: expr
    ) => {{
        let prev = $self.map.$usage[$region].get();
        $self.map.$usage[$region].set(prev | 1 << $bank_bit);
        let usage_addr_range = $region << $region_shift..($region + 1) << $region_shift;
        if prev == 0 {
            copy_slice_wrapping_unchecked_with_dst_range::<$mask, _, _>(
                &$self.$usage,
                $bank,
                usage_addr_range,
            );
        } else {
            or_assign_slice_wrapping_unchecked::<$mask, _, _>(&$self.$usage, $bank, usage_addr_range);
        }
    }};
    (
        wb $self: expr,
        $usage: ident,
        $usage_len: literal,
        $region_shift: expr,
        $mirrored_banks_mask: expr,
        $($bg_obj_updates: ident,)?
        ($($bit: literal => $mappable_bank: ident, $mask: literal,)*),

        $cpu: expr,
        $cpu_r_mask: expr,
        $cpu_rw_mask: expr,
        $cpu_start_addr: expr,
        $cpu_end_addr: expr,
        $bank: ident,
        $bank_mask: expr,
        $bank_bit: expr,
        $region: expr
    ) => {{
        let prev = $self.map.$usage[$region].get();
        $self.map.$usage[$region].set(prev | 1 << $bank_bit);
        let usage_addr_range = $region << $region_shift..($region + 1) << $region_shift;
        let writeback_arr = &mut *$self.writeback.$usage.get();
        #[allow(clippy::bad_bit_mask)]
        if prev == 0 {
            copy_slice_wrapping_unchecked_with_dst_range::<$bank_mask, _, _>(
                &$self.$usage,
                $bank,
                usage_addr_range.clone(),
            );
            if 1 << $bank_bit & $mirrored_banks_mask == 0
                $(&& $self.$bg_obj_updates.is_none())*
            {
                map_cpu_visible!(
                    $cpu, $cpu_rw_mask, $cpu_start_addr, $cpu_end_addr,
                    $self.$usage, $usage_len, $region, $region_shift
                );
            }
        } else {
            if prev & (prev - 1) == 0
                && prev & $mirrored_banks_mask == 0
                $(&& $self.$bg_obj_updates.is_none())*
            {
                map_cpu_visible!(
                    $cpu, $cpu_r_mask, $cpu_start_addr, $cpu_end_addr,
                    $self.$usage, $usage_len, $region, $region_shift
                );
                'writeback_to_prev_bank: {
                    $(
                        if $bit != $bank_bit && prev & 1 << $bit != 0 {
                            let bank_base_addr = usage_addr_range.start & $mask;
                            let copy_len =
                                ((usage_addr_range.end - 1) & $mask) - bank_base_addr + 1;
                            $self.banks.$mappable_bank.as_byte_mut_slice()
                                .get_unchecked_mut(bank_base_addr..bank_base_addr + copy_len)
                                .copy_from_slice($self.$usage.as_byte_slice().get_unchecked(
                                    usage_addr_range.start..usage_addr_range.start + copy_len
                                ));
                            break 'writeback_to_prev_bank;
                        }
                    )*
                }
            } else {
                for usage_addr in usage_addr_range.clone() {
                    let prev_value = $self.$usage.read_unchecked(usage_addr);
                    if *writeback_arr.get_unchecked(usage_addr / usize::BITS as usize)
                        & 1 << (usage_addr & (usize::BITS - 1) as usize)
                        != 0
                    {
                        $(
                            if $bit != $bank_bit && prev & 1 << $bit != 0 {
                                $self.banks.$mappable_bank.write_unchecked(
                                    usage_addr & $mask,
                                    prev_value,
                                );
                            }
                        )*
                    }
                }
            }
            or_assign_slice_wrapping_unchecked::<$bank_mask, _, _>(
                &$self.$usage,
                $bank,
                usage_addr_range.clone(),
            );
        }
        #[allow(clippy::bad_bit_mask)]
        if prev != 0 || 1 << $bank_bit & $mirrored_banks_mask != 0 {
            writeback_arr.get_unchecked_mut(
                usage_addr_range.start / usize::BITS as usize
                    ..usage_addr_range.end / usize::BITS as usize
            ).fill(0);
        }
    }};
}

macro_rules! unmap_region {
    (
        no_wb $self: expr,
        $usage: ident, $region_shift: expr,
        ($($bit: literal => $mappable_bank: ident, $mask: literal,)*),
        $bank_bit: expr, $region: expr
    ) => {{
        let new = $self.map.$usage[$region].get() & !(1 << $bank_bit);
        $self.map.$usage[$region].set(new);
        let usage_addr_range = $region << $region_shift..($region + 1) << $region_shift;
        if new == 0 {
            $self.$usage.as_byte_mut_slice().get_unchecked_mut(usage_addr_range).fill(0);
        } else {
            for usage_addr in usage_addr_range.step_by(size_of::<usize>()) {
                let mut value = 0_usize;
                $(
                    if $bit != $bank_bit && new & 1 << $bit != 0 {
                        value |= $self
                            .banks
                            .$mappable_bank
                            .read_ne_aligned_unchecked::<usize>(
                                usage_addr & $mask,
                            );
                    }
                )*
                $self.$usage.write_ne_aligned_unchecked(usage_addr, value);
            }
        }
    }};
    (
        wb $self: expr,

        $usage: ident,
        $usage_len: literal,
        $region_shift: expr,
        $mirrored_banks_mask: expr,
        $($bg_obj_updates: ident,)?
        ($($bit: literal => $mappable_bank: ident, $mask: literal,)*),

        $cpu: expr,
        $cpu_r_mask: expr,
        $cpu_rw_mask: expr,
        $cpu_start_addr: expr,
        $cpu_end_addr: expr,

        $bank: expr,
        $bank_mask: expr,
        $bank_bit: expr,
        $is_mirror: expr,
        $region: expr
    ) => {{
        let prev = $self.map.$usage[$region].get();
        let new = prev & !(1 << $bank_bit);
        $self.map.$usage[$region].set(new);
        let usage_addr_range = $region << $region_shift..($region + 1) << $region_shift;
        #[allow(clippy::bad_bit_mask)]
        if new == 0 {
            if !$is_mirror {
                $bank.as_byte_mut_slice().get_unchecked_mut(
                    usage_addr_range.start & $bank_mask
                        ..=(usage_addr_range.end - 1) & $bank_mask
                ).copy_from_slice(
                    &$self.$usage.as_byte_slice().get_unchecked(usage_addr_range.clone())
                );
            }
            $self.$usage.as_byte_mut_slice().get_unchecked_mut(usage_addr_range).fill(0);
            if prev & $mirrored_banks_mask == 0 $(&& $self.$bg_obj_updates.is_none())* {
                map_cpu_visible!(
                    $cpu, $cpu_r_mask, $cpu_start_addr, $cpu_end_addr,
                    $self.$usage, $usage_len, $region, $region_shift
                );
            }
        } else {
            if new & (new - 1) == 0
                && new & $mirrored_banks_mask == 0
                $(&& $self.$bg_obj_updates.is_none())*
            {
                map_cpu_visible!(
                    $cpu, $cpu_rw_mask, $cpu_start_addr, $cpu_end_addr,
                    $self.$usage, $usage_len, $region, $region_shift
                );
            }
            let writeback_arr = &*$self.writeback.$usage.get();
            for usage_addr in usage_addr_range {
                if *writeback_arr.get_unchecked(usage_addr / usize::BITS as usize)
                    & 1 << (usage_addr & (usize::BITS - 1) as usize)
                    == 0
                {
                    let mut value = 0;
                    $(
                        if $bit != $bank_bit && new & 1 << $bit != 0 {
                            value |= $self.banks.$mappable_bank.read_unchecked(
                                usage_addr & $mask,
                            );
                        }
                    )*
                    $self.$usage.write_unchecked(usage_addr, value);
                } else if !$is_mirror {
                    $bank.write_unchecked(
                        usage_addr & $bank_mask,
                        $self.$usage.read_unchecked(usage_addr),
                    );
                }
            }
        }
    }};
}

macro_rules! modify_bg_obj_updates {
    ($self: ident, $i: literal, |$bg_obj_updates: ident| $f: expr) => {
        #[allow(unused_unsafe)]
        if let Some(updates) = &$self.bg_obj_updates {
            let $bg_obj_updates = &mut unsafe { &mut *updates.get() }[$i];
            $f
        }
    };
}

impl Vram {
    pub(super) fn flush_writeback(&mut self) {
        macro_rules! flush_usage {
            (
                $usage: ident, $region_shift: expr, $mirrored_banks_mask: expr,
                ($($bit: literal => $bank: ident, $mask: literal,)*)
            ) => {
                for (region, mapped) in self.map.$usage.iter().enumerate() {
                    let mapped = mapped.get();
                    if mapped == 0 {
                        continue;
                    }
                    let usage_addr_range = region << $region_shift..(region + 1) << $region_shift;
                    unsafe {
                        let writeback_arr = &mut *self.writeback.$usage.get();
                        #[allow(clippy::bad_bit_mask)]
                        let clear = if mapped & (mapped - 1) == 0 {
                            'flush_bank: {
                                $(
                                    if mapped & 1 << $bit != 0 {
                                        let bank_start =
                                            usage_addr_range.start & $mask;
                                        let bank_end = (usage_addr_range.end - 1)
                                            & $mask;
                                        let usage_end =
                                            usage_addr_range.start + (bank_end - bank_start);
                                        self.banks.$bank.as_byte_mut_slice().get_unchecked_mut(
                                            bank_start..=bank_end,
                                        ).copy_from_slice(
                                            &self.$usage
                                                .as_byte_slice()
                                                .get_unchecked(usage_addr_range.start..=usage_end)
                                        );
                                        break 'flush_bank;
                                    }
                                )*
                            }
                            mapped & $mirrored_banks_mask != 0
                        } else {
                            for usage_addr in usage_addr_range.clone() {
                                if *writeback_arr.get_unchecked(usage_addr / usize::BITS as usize)
                                    & 1 << (usage_addr & (usize::BITS - 1) as usize)
                                    != 0
                                {
                                    $(
                                        if mapped & 1 << $bit != 0 {
                                            self.banks.$bank.write_unchecked(
                                                usage_addr & $mask,
                                                self.$usage.read_unchecked(usage_addr),
                                            );
                                        }
                                    )*
                                }
                            }
                            true
                        };
                        if clear {
                            writeback_arr.get_unchecked_mut(
                                usage_addr_range.start / usize::BITS as usize
                                    ..usage_addr_range.end / usize::BITS as usize
                            ).fill(0);
                        }
                    }
                }
            }
        }

        flush_usage!(
            a_bg, 14, 0x60,
            (
                0 => a, 0x1_FFFF,
                1 => b, 0x1_FFFF,
                2 => c, 0x1_FFFF,
                3 => d, 0x1_FFFF,
                4 => e, 0xFFFF,
                5 => f, 0x3FFF,
                6 => g, 0x3FFF,
            )
        );
        flush_usage!(
            a_obj, 14, 0x18,
            (
                0 => a, 0x1_FFFF,
                1 => b, 0x1_FFFF,
                2 => e, 0xFFFF,
                3 => f, 0x3FFF,
                4 => g, 0x3FFF,
            )
        );
        flush_usage!(
            b_bg, 15, 6,
            (
                0 => c, 0x1_FFFF,
                1 => h, 0x7FFF,
                2 => i, 0x3FFF,
            )
        );
        flush_usage!(
            b_obj, 17, 2,
            (
                0 => d, 0x1_FFFF,
                1 => i, 0x3FFF,
            )
        );
        flush_usage!(
            arm7, 17, 0,
            (
                0 => c, 0x1_FFFF,
                1 => d, 0x1_FFFF,
            )
        );
    }

    pub(super) fn restore_cpu_bg_obj_mappings<E: cpu::Engine>(&mut self, arm9: &mut Arm9<E>) {
        macro_rules! restore_region {
            (
                $cpu: expr, $cpu_r_mask: expr, $cpu_rw_mask: expr,
                $usage: ident, $usage_len: literal, $region_shift: expr, $mirrored_banks_mask: expr,
                $cpu_start_addr: expr, $cpu_end_addr: expr
            ) => {{
                (&mut *self.writeback.$usage.get()).fill(0);
                for (region, mapped) in self.map.$usage.iter().enumerate() {
                    let mapped = mapped.get();
                    let mask = if mapped != 0
                        && mapped & (mapped - 1) == 0
                        && mapped & $mirrored_banks_mask == 0
                        && self.bg_obj_updates.is_none()
                    {
                        $cpu_rw_mask
                    } else {
                        $cpu_r_mask
                    };
                    map_cpu_visible!(
                        $cpu,
                        mask,
                        $cpu_start_addr,
                        $cpu_end_addr,
                        self.$usage,
                        $usage_len,
                        region,
                        $region_shift
                    );
                }
            }};
        }

        unsafe {
            restore_region!(
                arm9,
                ptr_mask::R,
                ptr_mask::R | ptr_mask::W_16_32,
                a_bg,
                0x8_0000,
                14,
                0x60,
                0x0600_0000,
                0x0620_0000
            );
            restore_region!(
                arm9,
                ptr_mask::R,
                ptr_mask::R | ptr_mask::W_16_32,
                a_obj,
                0x4_0000,
                14,
                0x18,
                0x0640_0000,
                0x0660_0000
            );
            restore_region!(
                arm9,
                ptr_mask::R,
                ptr_mask::R | ptr_mask::W_16_32,
                b_bg,
                0x2_0000,
                15,
                6,
                0x0620_0000,
                0x0640_0000
            );
            restore_region!(
                arm9,
                ptr_mask::R,
                ptr_mask::R | ptr_mask::W_16_32,
                b_obj,
                0x2_0000,
                17,
                2,
                0x0660_0000,
                0x0680_0000
            );
        }
    }

    pub(crate) fn restore_mappings<E: cpu::Engine>(
        &mut self,
        arm7: &mut Arm7<E>,
        arm9: &mut Arm9<E>,
    ) {
        macro_rules! map_lcdc_banks {
            ($(
                $i: expr, $bank: ident, $regions_lower_bound: expr, $regions_upper_bound: expr;
            )*) => {
                $({
                    let bank_control = self.bank_control[$i];
                    if bank_control.enabled() && bank_control.mst() == 0 {
                        self.map_lcdc(
                            arm9,
                            $regions_lower_bound,
                            $regions_upper_bound,
                            self.banks.$bank.as_ptr(),
                        );
                    } else {
                        self.unmap_lcdc(arm9, $regions_lower_bound, $regions_upper_bound);
                    }
                })*
            };
        }

        unsafe {
            map_lcdc_banks!(
                0, a, 0x00, 0x07;
                1, b, 0x08, 0x0F;
                2, c, 0x10, 0x17;
                3, d, 0x18, 0x1F;
                4, e, 0x20, 0x23;
                5, f, 0x24, 0x24;
                6, g, 0x25, 0x25;
                7, h, 0x26, 0x27;
                8, i, 0x28, 0x28;
            );
        }

        self.b_bg_ext_pal_ptr = if self.bank_control[7].enabled() && self.bank_control[7].mst() == 2
        {
            self.banks.h.as_ptr()
        } else {
            self.zero_buffer.as_ptr()
        };

        self.b_obj_ext_pal_ptr =
            if self.bank_control[8].enabled() && self.bank_control[8].mst() == 3 {
                self.banks.i.as_ptr()
            } else {
                self.zero_buffer.as_ptr()
            };

        macro_rules! restore_region {
            (
                cpu_visible $cpu: expr, $cpu_r_mask: expr, $cpu_rw_mask: expr,
                $usage: ident, $usage_len: literal, $region_shift: expr, $mirrored_banks_mask: expr,
                $cpu_start_addr: expr, $cpu_end_addr: expr, $($bg_obj_updates: ident,)?
                ($($bit: literal => $bank: ident, $mask: literal,)*)
            ) => {{
                (&mut *self.writeback.$usage.get()).fill(0);
                for (region, mapped) in self.map.$usage.iter().enumerate() {
                    let mapped = mapped.get();
                    let usage_addr_range = region << $region_shift..(region + 1) << $region_shift;
                    let mask = if mapped == 0 {
                        self.$usage
                            .as_byte_mut_slice()
                            .get_unchecked_mut(usage_addr_range.clone())
                            .fill(0);
                        $cpu_r_mask
                    } else if mapped & (mapped - 1) == 0 {
                        'copy_bank: {
                            $(
                                if mapped & 1 << $bit != 0 {
                                    copy_slice_wrapping_unchecked_with_dst_range::<$mask, _, _>(
                                        &self.$usage,
                                        &self.banks.$bank,
                                        usage_addr_range,
                                    );
                                    break 'copy_bank;
                                }
                            )*
                        }
                        #[allow(clippy::bad_bit_mask)]
                        if mapped & $mirrored_banks_mask == 0 $(&& self.$bg_obj_updates.is_none())* {
                            $cpu_rw_mask
                        } else {
                            $cpu_r_mask
                        }
                    } else {
                        self.$usage
                            .as_byte_mut_slice()
                            .get_unchecked_mut(usage_addr_range.clone())
                            .fill(0);
                        $(
                            if mapped & 1 << $bit != 0 {
                                or_assign_slice_wrapping_unchecked::<$mask, _, _>(
                                    &self.$usage,
                                    &self.banks.$bank,
                                    usage_addr_range.clone(),
                                );
                            }
                        )*
                        $cpu_r_mask
                    };
                    map_cpu_visible!(
                        $cpu, mask, $cpu_start_addr, $cpu_end_addr,
                        self.$usage, $usage_len, region, $region_shift
                    );
                }
            }};
            (
                $usage: ident, $region_shift: expr,
                ($($bit: literal => $bank: ident, $mask: literal,)*)
            ) => {{
                for (region, mapped) in self.map.$usage.iter().enumerate() {
                    let mapped = mapped.get();
                    let usage_addr_range = region << $region_shift..(region + 1) << $region_shift;
                    if mapped == 0 {
                        self.$usage.as_byte_mut_slice().get_unchecked_mut(usage_addr_range).fill(0);
                    } else if mapped & (mapped - 1) == 0 {
                        $(
                            if mapped & 1 << $bit != 0 {
                                copy_slice_wrapping_unchecked_with_dst_range::<$mask, _, _>(
                                    &self.$usage,
                                    &self.banks.$bank,
                                    usage_addr_range.clone(),
                                );
                                continue;
                            }
                        )*
                    } else {
                        self.$usage
                            .as_byte_mut_slice()
                            .get_unchecked_mut(usage_addr_range.clone())
                            .fill(0);
                        $(
                            if mapped & 1 << $bit != 0 {
                                or_assign_slice_wrapping_unchecked::<$mask, _, _>(
                                    &self.$usage,
                                    &self.banks.$bank,
                                    usage_addr_range.clone(),
                                );
                            }
                        )*
                    }
                }
            }};
        }

        unsafe {
            restore_region!(
                a_bg_ext_pal, 14,
                (
                    0 => e, 0xFFFF,
                    1 => f, 0x3FFF,
                    2 => g, 0x3FFF,
                )
            );
            restore_region!(
                a_obj_ext_pal, 13,
                (
                    0 => f, 0x3FFF,
                    1 => g, 0x3FFF,
                )
            );
            restore_region!(
                texture, 17,
                (
                    0 => a, 0x1_FFFF,
                    1 => b, 0x1_FFFF,
                    2 => c, 0x1_FFFF,
                    3 => d, 0x1_FFFF,
                )
            );
            restore_region!(
                tex_pal, 14,
                (
                    0 => e, 0xFFFF,
                    1 => f, 0x3FFF,
                    2 => g, 0x3FFF,
                )
            );

            restore_region!(
                cpu_visible arm9, ptr_mask::R, ptr_mask::R | ptr_mask::W_16_32,
                a_bg, 0x8_0000, 14, 0x60,
                0x0600_0000, 0x0620_0000, bg_obj_updates,
                (
                    0 => a, 0x1_FFFF,
                    1 => b, 0x1_FFFF,
                    2 => c, 0x1_FFFF,
                    3 => d, 0x1_FFFF,
                    4 => e, 0xFFFF,
                    5 => f, 0x3FFF,
                    6 => g, 0x3FFF,
                )
            );
            restore_region!(
                cpu_visible arm9, ptr_mask::R, ptr_mask::R | ptr_mask::W_16_32,
                a_obj, 0x4_0000, 14, 0x18,
                0x0640_0000, 0x0660_0000, bg_obj_updates,
                (
                    0 => a, 0x1_FFFF,
                    1 => b, 0x1_FFFF,
                    2 => e, 0xFFFF,
                    3 => f, 0x3FFF,
                    4 => g, 0x3FFF,
                )
            );
            restore_region!(
                cpu_visible arm9, ptr_mask::R, ptr_mask::R | ptr_mask::W_16_32,
                b_bg, 0x2_0000, 15, 6,
                0x0620_0000, 0x0640_0000, bg_obj_updates,
                (
                    0 => c, 0x1_FFFF,
                    1 => h, 0x7FFF,
                    2 => i, 0x3FFF,
                )
            );
            restore_region!(
                cpu_visible arm9, ptr_mask::R, ptr_mask::R | ptr_mask::W_16_32,
                b_obj, 0x2_0000, 17, 2,
                0x0660_0000, 0x0680_0000, bg_obj_updates,
                (
                    0 => d, 0x1_FFFF,
                    1 => i, 0x3FFF,
                )
            );
            restore_region!(
                cpu_visible arm7, arm7_ptr_mask::R, arm7_ptr_mask::ALL,
                arm7, 0x4_0000, 17, 0,
                0x0600_0000, 0x0700_0000,
                (
                    0 => c, 0x1_FFFF,
                    1 => d, 0x1_FFFF,
                )
            );
        }
    }

    unsafe fn map_lcdc<E: cpu::Engine>(
        &mut self,
        arm9: &mut Arm9<E>,
        regions_lower_bound: usize,
        regions_upper_bound: usize,
        ptr: *mut u8,
    ) {
        {
            let mut ptr = ptr;
            for region in regions_lower_bound..=regions_upper_bound {
                self.lcdc_r_ptrs[region] = ptr;
                self.lcdc_w_ptrs[region] = ptr;
                ptr = ptr.add(0x4000);
            }
        }
        let lower_bound = 0x0680_0000 | (regions_lower_bound as u32) << 14;
        let upper_bound = 0x0680_0000 | (regions_upper_bound as u32) << 14 | 0x3FFF;
        let size = (regions_upper_bound - regions_lower_bound + 1) << 14;
        for mirror_base in (0..0x80_0000).step_by(0x10_0000) {
            arm9.map_sys_bus_ptr_range(
                ptr_mask::ALL & !ptr_mask::W_8,
                ptr,
                size,
                (mirror_base | lower_bound, mirror_base | upper_bound),
            );
        }
    }

    fn unmap_lcdc<E: cpu::Engine>(
        &mut self,
        arm9: &mut Arm9<E>,
        regions_lower_bound: usize,
        regions_upper_bound: usize,
    ) {
        let lower_bound = 0x0680_0000 | (regions_lower_bound as u32) << 14;
        let upper_bound = 0x0680_0000 | (regions_upper_bound as u32) << 14 | 0x3FFF;
        for region in regions_lower_bound..=regions_upper_bound {
            self.lcdc_r_ptrs[region] = self.zero_buffer.as_ptr();
            self.lcdc_w_ptrs[region] = self.ignore_buffer.as_ptr();
        }
        for mirror_base in (0..0x80_0000).step_by(0x10_0000) {
            unsafe {
                arm9.map_sys_bus_ptr_range(
                    ptr_mask::R,
                    self.zero_buffer.as_ptr(),
                    0x8000,
                    (mirror_base | lower_bound, mirror_base | upper_bound),
                );
            }
        }
    }

    unsafe fn map_a_bg<
        E: cpu::Engine,
        R: IntoIterator<Item = usize>,
        const LEN: usize,
        const MASK: usize,
        const BANK_BIT: u8,
    >(
        &self,
        arm9: &mut Arm9<E>,
        bank: &OwnedBytesCellPtr<LEN>,
        regions: R,
    ) {
        for region in regions {
            map_region!(
                wb self,
                a_bg, 0x8_0000, 14, 0x60, bg_obj_updates,
                (
                    0 => a, 0x1_FFFF,
                    1 => b, 0x1_FFFF,
                    2 => c, 0x1_FFFF,
                    3 => d, 0x1_FFFF,
                    4 => e, 0xFFFF,
                    5 => f, 0x3FFF,
                    6 => g, 0x3FFF,
                ),
                arm9, ptr_mask::R, ptr_mask::R | ptr_mask::W_16_32, 0x0600_0000, 0x0620_0000,
                bank, MASK, BANK_BIT, region
            );
            modify_bg_obj_updates!(self, 0, |updates| {
                updates.bg |= 1 << region;
            });
        }
    }

    unsafe fn unmap_a_bg<
        E: cpu::Engine,
        R: IntoIterator<Item = usize>,
        const LEN: usize,
        const MASK: usize,
        const IS_MIRROR: bool,
        const BANK_BIT: u8,
    >(
        &self,
        arm9: &mut Arm9<E>,
        bank: &OwnedBytesCellPtr<LEN>,
        regions: R,
    ) {
        for region in regions {
            unmap_region!(
                wb self,
                a_bg, 0x8_0000, 14, 0x60, bg_obj_updates,
                (
                    0 => a, 0x1_FFFF,
                    1 => b, 0x1_FFFF,
                    2 => c, 0x1_FFFF,
                    3 => d, 0x1_FFFF,
                    4 => e, 0xFFFF,
                    5 => f, 0x3FFF,
                    6 => g, 0x3FFF,
                ),
                arm9, ptr_mask::R, ptr_mask::R | ptr_mask::W_16_32, 0x0600_0000, 0x0620_0000,
                bank, MASK, BANK_BIT, IS_MIRROR, region
            );
            modify_bg_obj_updates!(self, 0, |updates| {
                updates.bg |= 1 << region;
            });
        }
    }

    unsafe fn map_a_obj<
        E: cpu::Engine,
        R: IntoIterator<Item = usize>,
        const LEN: usize,
        const MASK: usize,
        const BANK_BIT: u8,
    >(
        &self,
        arm9: &mut Arm9<E>,
        bank: &OwnedBytesCellPtr<LEN>,
        regions: R,
    ) {
        for region in regions {
            map_region!(
                wb self,
                a_obj, 0x4_0000, 14, 0x18, bg_obj_updates,
                (
                    0 => a, 0x1_FFFF,
                    1 => b, 0x1_FFFF,
                    2 => e, 0xFFFF,
                    3 => f, 0x3FFF,
                    4 => g, 0x3FFF,
                ),
                arm9, ptr_mask::R, ptr_mask::R | ptr_mask::W_16_32, 0x0640_0000, 0x0660_0000,
                bank, MASK, BANK_BIT, region
            );
            modify_bg_obj_updates!(self, 0, |updates| {
                updates.obj |= 1 << region;
            });
        }
    }

    unsafe fn unmap_a_obj<
        E: cpu::Engine,
        R: IntoIterator<Item = usize>,
        const LEN: usize,
        const MASK: usize,
        const IS_MIRROR: bool,
        const BANK_BIT: u8,
    >(
        &self,
        arm9: &mut Arm9<E>,
        bank: &OwnedBytesCellPtr<LEN>,
        regions: R,
    ) {
        for region in regions {
            unmap_region!(
                wb self,
                a_obj, 0x4_0000, 14, 0x18, bg_obj_updates,
                (
                    0 => a, 0x1_FFFF,
                    1 => b, 0x1_FFFF,
                    2 => e, 0xFFFF,
                    3 => f, 0x3FFF,
                    4 => g, 0x3FFF,
                ),
                arm9, ptr_mask::R, ptr_mask::R | ptr_mask::W_16_32, 0x0640_0000, 0x0660_0000,
                bank, MASK, BANK_BIT, IS_MIRROR, region
            );
            modify_bg_obj_updates!(self, 0, |updates| {
                updates.obj |= 1 << region;
            });
        }
    }

    unsafe fn map_a_bg_ext_pal<
        R: IntoIterator<Item = usize>,
        const LEN: usize,
        const MASK: usize,
        const BANK_BIT: u8,
    >(
        &self,
        bank: &OwnedBytesCellPtr<LEN>,
        regions: R,
    ) {
        for region in regions {
            map_region!(no_wb self, a_bg_ext_pal, 14, bank, MASK, BANK_BIT, region);
            modify_bg_obj_updates!(self, 0, |updates| {
                updates.bg_ext_palette |= 1 << region;
            });
        }
    }

    unsafe fn unmap_a_bg_ext_pal<R: IntoIterator<Item = usize>, const BANK_BIT: u8>(
        &self,
        regions: R,
    ) {
        for region in regions {
            unmap_region!(
                no_wb self,
                a_bg_ext_pal, 14,
                (
                    0 => e, 0xFFFF,
                    1 => f, 0x3FFF,
                    2 => g, 0x3FFF,
                ),
                BANK_BIT, region
            );
            modify_bg_obj_updates!(self, 0, |updates| {
                updates.bg_ext_palette |= 1 << region;
            });
        }
    }

    unsafe fn map_a_obj_ext_pal<const LEN: usize, const MASK: usize, const BANK_BIT: u8>(
        &self,
        bank: &OwnedBytesCellPtr<LEN>,
    ) {
        map_region!(no_wb self, a_obj_ext_pal, 13, bank, MASK, BANK_BIT, 0);
        modify_bg_obj_updates!(self, 0, |updates| {
            updates.obj_ext_palette = true;
        });
    }

    unsafe fn unmap_a_obj_ext_pal<const BANK_BIT: u8>(&self) {
        unmap_region!(
            no_wb self,
            a_obj_ext_pal, 13,
            (
                0 => f, 0x3FFF,
                1 => g, 0x3FFF,
            ),
            BANK_BIT, 0
        );
        modify_bg_obj_updates!(self, 0, |updates| {
            updates.obj_ext_palette = true;
        });
    }

    unsafe fn map_b_bg<
        E: cpu::Engine,
        R: IntoIterator<Item = usize>,
        const LEN: usize,
        const MASK: usize,
        const BANK_BIT: u8,
    >(
        &self,
        arm9: &mut Arm9<E>,
        bank: &OwnedBytesCellPtr<LEN>,
        regions: R,
    ) {
        for region in regions {
            map_region!(
                wb self,
                b_bg, 0x2_0000, 15, 6, bg_obj_updates,
                (
                    0 => c, 0x1_FFFF,
                    1 => h, 0x7FFF,
                    2 => i, 0x3FFF,
                ),
                arm9, ptr_mask::R, ptr_mask::R | ptr_mask::W_16_32, 0x0620_0000, 0x0640_0000,
                bank, MASK, BANK_BIT, region
            );
            modify_bg_obj_updates!(self, 1, |updates| {
                updates.bg |= 3 << (region << 1);
            });
        }
    }

    unsafe fn unmap_b_bg<
        E: cpu::Engine,
        R: IntoIterator<Item = usize>,
        const LEN: usize,
        const MASK: usize,
        const IS_MIRROR: bool,
        const BANK_BIT: u8,
    >(
        &self,
        arm9: &mut Arm9<E>,
        bank: &OwnedBytesCellPtr<LEN>,
        regions: R,
    ) {
        for region in regions {
            unmap_region!(
                wb self,
                b_bg, 0x2_0000, 15, 6, bg_obj_updates,
                (
                    0 => c, 0x1_FFFF,
                    1 => h, 0x7FFF,
                    2 => i, 0x3FFF,
                ),
                arm9, ptr_mask::R, ptr_mask::R | ptr_mask::W_16_32, 0x0620_0000, 0x0640_0000,
                bank, MASK, BANK_BIT, IS_MIRROR, region
            );
            modify_bg_obj_updates!(self, 1, |updates| {
                updates.bg |= 3 << (region << 1);
            });
        }
    }

    unsafe fn map_b_obj<E: cpu::Engine, const LEN: usize, const MASK: usize, const BANK_BIT: u8>(
        &self,
        arm9: &mut Arm9<E>,
        bank: &OwnedBytesCellPtr<LEN>,
    ) {
        map_region!(
            wb self,
            b_obj, 0x2_0000, 17, 2, bg_obj_updates,
            (
                0 => d, 0x1_FFFF,
                1 => i, 0x3FFF,
            ),
            arm9, ptr_mask::R, ptr_mask::R | ptr_mask::W_16_32, 0x0660_0000, 0x0680_0000,
            bank, MASK, BANK_BIT, 0
        );
        modify_bg_obj_updates!(self, 1, |updates| {
            updates.obj = 0xFF;
        });
    }

    unsafe fn unmap_b_obj<
        E: cpu::Engine,
        const LEN: usize,
        const MASK: usize,
        const BANK_BIT: u8,
    >(
        &self,
        arm9: &mut Arm9<E>,
        bank: &OwnedBytesCellPtr<LEN>,
    ) {
        unmap_region!(
            wb self,
            b_obj, 0x2_0000, 17, 2, bg_obj_updates,
            (
                0 => d, 0x1_FFFF,
                1 => i, 0x3FFF,
            ),
            arm9, ptr_mask::R, ptr_mask::R | ptr_mask::W_16_32, 0x0660_0000, 0x0680_0000,
            bank, MASK, BANK_BIT, false, 0
        );
        modify_bg_obj_updates!(self, 1, |updates| {
            updates.obj = 0xFF;
        });
    }

    unsafe fn map_texture<const LEN: usize, const MASK: usize, const BANK_BIT: u8>(
        &self,
        bank: &OwnedBytesCellPtr<LEN>,
        region: usize,
    ) {
        map_region!(no_wb self, texture, 17, bank, MASK, BANK_BIT, region);
    }

    unsafe fn unmap_texture<const BANK_BIT: u8>(&self, region: usize) {
        unmap_region!(
            no_wb self,
            texture, 17,
            (
                0 => a, 0x1_FFFF,
                1 => b, 0x1_FFFF,
                2 => c, 0x1_FFFF,
                3 => d, 0x1_FFFF,
            ),
            BANK_BIT, region
        );
    }

    unsafe fn map_tex_pal<
        R: IntoIterator<Item = usize>,
        const LEN: usize,
        const MASK: usize,
        const BANK_BIT: u8,
    >(
        &self,
        bank: &OwnedBytesCellPtr<LEN>,
        regions: R,
    ) {
        for region in regions {
            map_region!(no_wb self, tex_pal, 14, bank, MASK, BANK_BIT, region);
        }
    }

    unsafe fn unmap_tex_pal<R: IntoIterator<Item = usize>, const BANK_BIT: u8>(&self, regions: R) {
        for region in regions {
            unmap_region!(
                no_wb self,
                tex_pal, 14,
                (
                    0 => e, 0xFFFF,
                    1 => f, 0x3FFF,
                    2 => g, 0x3FFF,
                ),
                BANK_BIT, region
            );
        }
    }

    unsafe fn map_arm7<E: cpu::Engine, const LEN: usize, const MASK: usize, const BANK_BIT: u8>(
        &self,
        arm7: &mut Arm7<E>,
        bank: &OwnedBytesCellPtr<LEN>,
        region: usize,
    ) {
        map_region!(
            wb self,
            arm7, 0x4_0000, 17, 0,
            (
                0 => c, 0x1_FFFF,
                1 => d, 0x1_FFFF,
            ),
            arm7, arm7_ptr_mask::R, arm7_ptr_mask::ALL, 0x0600_0000, 0x0700_0000,
            bank, MASK, BANK_BIT, region
        );
    }

    unsafe fn unmap_arm7<
        E: cpu::Engine,
        const LEN: usize,
        const MASK: usize,
        const BANK_BIT: u8,
    >(
        &self,
        arm7: &mut Arm7<E>,
        bank: &OwnedBytesCellPtr<LEN>,
        region: usize,
    ) {
        use arm7::bus::ptrs::mask as ptr_mask;
        unmap_region!(
            wb self,
            arm7, 0x4_0000, 17, 0,
            (
                0 => c, 0x1_FFFF,
                1 => d, 0x1_FFFF,
            ),
            arm7, ptr_mask::R, ptr_mask::ALL, 0x0600_0000, 0x0700_0000,
            bank, MASK, BANK_BIT, false, region
        );
    }

    pub fn write_bank_control_a<E: cpu::Engine>(
        &mut self,
        mut value: BankControl,
        arm9: &mut Arm9<E>,
        engine_3d: &mut Engine3d,
    ) {
        value.0 &= 0x9B;
        let prev_value = self.bank_control[0];
        if value == prev_value {
            return;
        }
        self.bank_control[0] = value;
        unsafe {
            if prev_value.enabled() {
                match prev_value.mst() & 3 {
                    0 => self.unmap_lcdc(arm9, 0, 7),
                    1 => {
                        let base_region = (prev_value.offset() as usize) << 3;
                        self.unmap_a_bg::<_, _, _, 0x1_FFFF, false, 0>(
                            arm9,
                            &self.banks.a,
                            base_region..base_region + 8,
                        );
                    }
                    2 => {
                        let base_region = (prev_value.offset() as usize & 1) << 3;
                        self.unmap_a_obj::<_, _, _, 0x1_FFFF, false, 0>(
                            arm9,
                            &self.banks.a,
                            base_region..base_region + 8,
                        );
                    }
                    _ => {
                        let region = prev_value.offset() as usize;
                        engine_3d.set_texture_dirty(1 << region);
                        self.unmap_texture::<0>(region);
                    }
                }
            }
            if value.enabled() {
                match value.mst() & 3 {
                    0 => self.map_lcdc(arm9, 0, 7, self.banks.a.as_ptr()),
                    1 => {
                        let base_region = (value.offset() as usize) << 3;
                        self.map_a_bg::<_, _, _, 0x1_FFFF, 0>(
                            arm9,
                            &self.banks.a,
                            base_region..base_region + 8,
                        );
                    }
                    2 => {
                        let base_region = (value.offset() as usize & 1) << 3;
                        self.map_a_obj::<_, _, _, 0x1_FFFF, 0>(
                            arm9,
                            &self.banks.a,
                            base_region..base_region + 8,
                        );
                    }
                    _ => {
                        let region = value.offset() as usize;
                        engine_3d.set_texture_dirty(1 << region);
                        self.map_texture::<_, 0x1_FFFF, 0>(&self.banks.a, region);
                    }
                }
            }
        }
    }

    pub fn write_bank_control_b<E: cpu::Engine>(
        &mut self,
        mut value: BankControl,
        arm9: &mut Arm9<E>,
        engine_3d: &mut Engine3d,
    ) {
        value.0 &= 0x9B;
        let prev_value = self.bank_control[1];
        if value == prev_value {
            return;
        }
        self.bank_control[1] = value;
        unsafe {
            if prev_value.enabled() {
                match prev_value.mst() & 3 {
                    0 => self.unmap_lcdc(arm9, 8, 0xF),
                    1 => {
                        let base_region = (prev_value.offset() as usize) << 3;
                        self.unmap_a_bg::<_, _, _, 0x1_FFFF, false, 1>(
                            arm9,
                            &self.banks.b,
                            base_region..base_region + 8,
                        );
                    }
                    2 => {
                        let base_region = (prev_value.offset() as usize & 1) << 3;
                        self.unmap_a_obj::<_, _, _, 0x1_FFFF, false, 1>(
                            arm9,
                            &self.banks.b,
                            base_region..base_region + 8,
                        );
                    }
                    _ => {
                        let region = prev_value.offset() as usize;
                        engine_3d.set_texture_dirty(1 << region);
                        self.unmap_texture::<1>(region);
                    }
                }
            }
            if value.enabled() {
                match value.mst() & 3 {
                    0 => self.map_lcdc(arm9, 8, 0xF, self.banks.b.as_ptr()),
                    1 => {
                        let base_region = (value.offset() as usize) << 3;
                        self.map_a_bg::<_, _, _, 0x1_FFFF, 1>(
                            arm9,
                            &self.banks.b,
                            base_region..base_region + 8,
                        );
                    }
                    2 => {
                        let base_region = (value.offset() as usize & 1) << 3;
                        self.map_a_obj::<_, _, _, 0x1_FFFF, 1>(
                            arm9,
                            &self.banks.b,
                            base_region..base_region + 8,
                        );
                    }
                    _ => {
                        let region = value.offset() as usize;
                        engine_3d.set_texture_dirty(1 << region);
                        self.map_texture::<_, 0x1_FFFF, 1>(&self.banks.b, region);
                    }
                }
            }
        }
    }

    pub fn write_bank_control_c<E: cpu::Engine>(
        &mut self,
        mut value: BankControl,
        arm7: &mut Arm7<E>,
        arm9: &mut Arm9<E>,
        engine_3d: &mut Engine3d,
    ) {
        value.0 &= 0x9F;
        let prev_value = self.bank_control[2];
        if value == prev_value {
            return;
        }
        self.bank_control[2] = value;
        unsafe {
            if prev_value.enabled() {
                match prev_value.mst() {
                    0 => {
                        self.unmap_lcdc(arm9, 0x10, 0x17);
                    }
                    1 => {
                        let base_region = (prev_value.offset() as usize) << 3;
                        self.unmap_a_bg::<_, _, _, 0x1_FFFF, false, 2>(
                            arm9,
                            &self.banks.c,
                            base_region..base_region + 8,
                        );
                    }
                    2 => {
                        let region = prev_value.offset() as usize & 1;
                        self.unmap_arm7::<_, _, 0x1_FFFF, 0>(arm7, &self.banks.c, region);
                        self.arm7_status.set_c_used_as_arm7(false);
                    }
                    3 => {
                        let region = prev_value.offset() as usize;
                        engine_3d.set_texture_dirty(1 << region);
                        self.unmap_texture::<2>(region);
                    }
                    4 => self.unmap_b_bg::<_, _, _, 0x1_FFFF, false, 0>(arm9, &self.banks.c, 0..4),
                    _ => {
                        unimplemented!("Specified invalid mapping for bank C: {}", prev_value.mst())
                    }
                }
            }
            if value.enabled() {
                match value.mst() {
                    0 => {
                        self.map_lcdc(arm9, 0x10, 0x17, self.banks.c.as_ptr());
                    }
                    1 => {
                        let base_region = (value.offset() as usize) << 3;
                        self.map_a_bg::<_, _, _, 0x1_FFFF, 2>(
                            arm9,
                            &self.banks.c,
                            base_region..base_region + 8,
                        );
                    }
                    2 => {
                        let region = value.offset() as usize & 1;
                        self.map_arm7::<_, _, 0x1_FFFF, 0>(arm7, &self.banks.c, region);
                        self.arm7_status.set_c_used_as_arm7(true);
                    }
                    3 => {
                        let region = value.offset() as usize;
                        engine_3d.set_texture_dirty(1 << region);
                        self.map_texture::<_, 0x1_FFFF, 2>(&self.banks.c, region);
                    }
                    4 => self.map_b_bg::<_, _, _, 0x1_FFFF, 0>(arm9, &self.banks.c, 0..4),
                    _ => {
                        unimplemented!("Specified invalid mapping for bank C: {}", value.mst())
                    }
                }
            }
        }
    }

    pub fn write_bank_control_d<E: cpu::Engine>(
        &mut self,
        mut value: BankControl,
        arm7: &mut Arm7<E>,
        arm9: &mut Arm9<E>,
        engine_3d: &mut Engine3d,
    ) {
        value.0 &= 0x9F;
        unsafe {
            let prev_value = self.bank_control[3];
            if value == prev_value {
                return;
            }
            self.bank_control[3] = value;
            if prev_value.enabled() {
                match prev_value.mst() {
                    0 => {
                        self.unmap_lcdc(arm9, 0x18, 0x1F);
                    }
                    1 => {
                        let base_region = (prev_value.offset() as usize) << 3;
                        self.unmap_a_bg::<_, _, _, 0x1_FFFF, false, 3>(
                            arm9,
                            &self.banks.d,
                            base_region..base_region + 8,
                        );
                    }
                    2 => {
                        let region = prev_value.offset() as usize & 1;
                        self.unmap_arm7::<_, _, 0x1_FFFF, 1>(arm7, &self.banks.d, region);
                        self.arm7_status.set_d_used_as_arm7(false);
                    }
                    3 => {
                        let region = prev_value.offset() as usize;
                        engine_3d.set_texture_dirty(1 << region);
                        self.unmap_texture::<3>(region);
                    }
                    4 => self.unmap_b_obj::<_, _, 0x1_FFFF, 0>(arm9, &self.banks.d),
                    _ => {
                        unimplemented!("Specified invalid mapping for bank D: {}", prev_value.mst())
                    }
                }
            }
            if value.enabled() {
                match value.mst() {
                    0 => self.map_lcdc(arm9, 0x18, 0x1F, self.banks.d.as_ptr()),
                    1 => {
                        let base_region = (value.offset() as usize) << 3;
                        self.map_a_bg::<_, _, _, 0x1_FFFF, 3>(
                            arm9,
                            &self.banks.d,
                            base_region..base_region + 8,
                        );
                    }
                    2 => {
                        let region = value.offset() as usize & 1;
                        self.map_arm7::<_, _, 0x1_FFFF, 1>(arm7, &self.banks.d, region);
                        self.arm7_status.set_d_used_as_arm7(true);
                    }
                    3 => {
                        let region = value.offset() as usize;
                        engine_3d.set_texture_dirty(1 << region);
                        self.map_texture::<_, 0x1_FFFF, 3>(&self.banks.d, region);
                    }
                    4 => self.map_b_obj::<_, _, 0x1_FFFF, 0>(arm9, &self.banks.d),
                    _ => {
                        unimplemented!("Specified invalid mapping for bank D: {}", value.mst())
                    }
                }
            }
        }
    }

    pub fn write_bank_control_e<E: cpu::Engine>(
        &mut self,
        mut value: BankControl,
        arm9: &mut Arm9<E>,
        engine_3d: &mut Engine3d,
    ) {
        value.0 &= 0x87;
        unsafe {
            let prev_value = self.bank_control[4];
            if value == prev_value {
                return;
            }
            self.bank_control[4] = value;
            if prev_value.enabled() {
                match prev_value.mst() {
                    0 => self.unmap_lcdc(arm9, 0x20, 0x23),
                    1 => self.unmap_a_bg::<_, _, _, 0xFFFF, false, 4>(arm9, &self.banks.e, 0..4),
                    2 => self.unmap_a_obj::<_, _, _, 0xFFFF, false, 2>(arm9, &self.banks.e, 0..4),
                    3 => {
                        engine_3d.set_tex_pal_dirty(0xF);
                        self.unmap_tex_pal::<_, 0>(0..4);
                    }
                    4 => self.unmap_a_bg_ext_pal::<_, 0>(0..2),
                    _ => {
                        unimplemented!("Specified invalid mapping for bank E: {}", prev_value.mst())
                    }
                }
            }
            if value.enabled() {
                match value.mst() {
                    0 => self.map_lcdc(arm9, 0x20, 0x23, self.banks.e.as_ptr()),
                    1 => self.map_a_bg::<_, _, _, 0xFFFF, 4>(arm9, &self.banks.e, 0..4),
                    2 => self.map_a_obj::<_, _, _, 0xFFFF, 2>(arm9, &self.banks.e, 0..4),
                    3 => {
                        engine_3d.set_tex_pal_dirty(0xF);
                        self.map_tex_pal::<_, _, 0xFFFF, 0>(&self.banks.e, 0..4);
                    }
                    4 => self.map_a_bg_ext_pal::<_, _, 0xFFFF, 0>(&self.banks.e, 0..2),
                    _ => {
                        unimplemented!("Specified invalid mapping for bank E: {}", value.mst())
                    }
                }
            }
        }
    }

    pub fn write_bank_control_f<E: cpu::Engine>(
        &mut self,
        mut value: BankControl,
        arm9: &mut Arm9<E>,
        engine_3d: &mut Engine3d,
    ) {
        value.0 &= 0x9F;
        unsafe {
            let prev_value = self.bank_control[5];
            if value == prev_value {
                return;
            }
            self.bank_control[5] = value;
            if prev_value.enabled() {
                match prev_value.mst() {
                    0 => {
                        self.unmap_lcdc(arm9, 0x24, 0x24);
                    }
                    1 => {
                        let base_region =
                            ((prev_value.offset() & 1) | (prev_value.offset() & 2) << 1) as usize;
                        self.unmap_a_bg::<_, _, _, 0x3FFF, false, 5>(
                            arm9,
                            &self.banks.f,
                            once(base_region),
                        );
                        self.unmap_a_bg::<_, _, _, 0x3FFF, true, 5>(
                            arm9,
                            &self.banks.f,
                            once(base_region | 2),
                        );
                    }
                    2 => {
                        let base_region =
                            ((prev_value.offset() & 1) | (prev_value.offset() & 2) << 1) as usize;
                        self.unmap_a_obj::<_, _, _, 0x3FFF, false, 3>(
                            arm9,
                            &self.banks.f,
                            once(base_region),
                        );
                        self.unmap_a_obj::<_, _, _, 0x3FFF, true, 3>(
                            arm9,
                            &self.banks.f,
                            once(base_region | 2),
                        );
                    }
                    3 => {
                        let region =
                            ((prev_value.offset() & 1) | (prev_value.offset() & 2) << 1) as usize;
                        engine_3d.set_tex_pal_dirty(1 << region);
                        self.unmap_tex_pal::<_, 1>(once(region));
                    }
                    4 => {
                        let region = prev_value.offset() as usize & 1;
                        self.unmap_a_bg_ext_pal::<_, 1>(once(region));
                    }
                    5 => self.unmap_a_obj_ext_pal::<0>(),
                    _ => {
                        unimplemented!("Specified invalid mapping for bank F: {}", prev_value.mst())
                    }
                }
            }
            if value.enabled() {
                match value.mst() {
                    0 => self.map_lcdc(arm9, 0x24, 0x24, self.banks.f.as_ptr()),
                    1 => {
                        let base_region =
                            ((value.offset() & 1) | (value.offset() & 2) << 1) as usize;
                        self.map_a_bg::<_, _, _, 0x3FFF, 5>(
                            arm9,
                            &self.banks.f,
                            [base_region, base_region | 2],
                        );
                    }
                    2 => {
                        let base_region =
                            ((value.offset() & 1) | (value.offset() & 2) << 1) as usize;
                        self.map_a_obj::<_, _, _, 0x3FFF, 3>(
                            arm9,
                            &self.banks.f,
                            [base_region, base_region | 2],
                        );
                    }
                    3 => {
                        let region = ((value.offset() & 1) | (value.offset() & 2) << 1) as usize;
                        engine_3d.set_tex_pal_dirty(1 << region);
                        self.map_tex_pal::<_, _, 0x3FFF, 1>(&self.banks.f, once(region));
                    }
                    4 => {
                        let region = value.offset() as usize & 1;
                        self.map_a_bg_ext_pal::<_, _, 0x3FFF, 1>(&self.banks.f, once(region));
                    }
                    5 => self.map_a_obj_ext_pal::<_, 0x3FFF, 0>(&self.banks.f),
                    _ => {
                        unimplemented!("Specified invalid mapping for bank F: {}", value.mst())
                    }
                }
            }
        }
    }

    pub fn write_bank_control_g<E: cpu::Engine>(
        &mut self,
        mut value: BankControl,
        arm9: &mut Arm9<E>,
        engine_3d: &mut Engine3d,
    ) {
        value.0 &= 0x9F;
        unsafe {
            let prev_value = self.bank_control[6];
            if value == prev_value {
                return;
            }
            self.bank_control[6] = value;
            if prev_value.enabled() {
                match prev_value.mst() {
                    0 => {
                        self.unmap_lcdc(arm9, 0x25, 0x25);
                    }
                    1 => {
                        let base_region =
                            ((prev_value.offset() & 1) | (prev_value.offset() & 2) << 1) as usize;
                        self.unmap_a_bg::<_, _, _, 0x3FFF, false, 6>(
                            arm9,
                            &self.banks.g,
                            once(base_region),
                        );
                        self.unmap_a_bg::<_, _, _, 0x3FFF, true, 6>(
                            arm9,
                            &self.banks.g,
                            once(base_region | 2),
                        );
                    }
                    2 => {
                        let base_region =
                            ((prev_value.offset() & 1) | (prev_value.offset() & 2) << 1) as usize;
                        self.unmap_a_obj::<_, _, _, 0x3FFF, false, 4>(
                            arm9,
                            &self.banks.g,
                            once(base_region),
                        );
                        self.unmap_a_obj::<_, _, _, 0x3FFF, true, 4>(
                            arm9,
                            &self.banks.g,
                            once(base_region | 2),
                        );
                    }
                    3 => {
                        let region =
                            ((prev_value.offset() & 1) | (prev_value.offset() & 2) << 1) as usize;
                        engine_3d.set_tex_pal_dirty(1 << region);
                        self.unmap_tex_pal::<_, 2>(once(region));
                    }
                    4 => {
                        let region = prev_value.offset() as usize & 1;
                        self.unmap_a_bg_ext_pal::<_, 2>(once(region));
                    }
                    5 => self.unmap_a_obj_ext_pal::<1>(),
                    _ => {
                        unimplemented!("Specified invalid mapping for bank G: {}", prev_value.mst())
                    }
                }
            }
            if value.enabled() {
                match value.mst() {
                    0 => self.map_lcdc(arm9, 0x25, 0x25, self.banks.g.as_ptr()),
                    1 => {
                        let base_region =
                            ((value.offset() & 1) | (value.offset() & 2) << 1) as usize;
                        self.map_a_bg::<_, _, _, 0x3FFF, 6>(
                            arm9,
                            &self.banks.g,
                            [base_region, base_region | 2],
                        );
                    }
                    2 => {
                        let base_region =
                            ((value.offset() & 1) | (value.offset() & 2) << 1) as usize;
                        self.map_a_obj::<_, _, _, 0x3FFF, 4>(
                            arm9,
                            &self.banks.g,
                            [base_region, base_region | 2],
                        );
                    }
                    3 => {
                        let region = ((value.offset() & 1) | (value.offset() & 2) << 1) as usize;
                        engine_3d.set_tex_pal_dirty(1 << region);
                        self.map_tex_pal::<_, _, 0x3FFF, 2>(&self.banks.g, once(region));
                    }
                    4 => {
                        let region = value.offset() as usize & 1;
                        self.map_a_bg_ext_pal::<_, _, 0x3FFF, 2>(&self.banks.g, once(region));
                    }
                    5 => self.map_a_obj_ext_pal::<_, 0x3FFF, 1>(&self.banks.g),
                    _ => {
                        unimplemented!("Specified invalid mapping for bank G: {}", value.mst())
                    }
                }
            }
        }
    }

    pub fn write_bank_control_h<E: cpu::Engine>(
        &mut self,
        mut value: BankControl,
        arm9: &mut Arm9<E>,
    ) {
        value.0 &= 0x83;
        unsafe {
            let prev_value = self.bank_control[7];
            if value == prev_value {
                return;
            }
            self.bank_control[7] = value;
            if prev_value.enabled() {
                match prev_value.mst() & 3 {
                    0 => {
                        self.unmap_lcdc(arm9, 0x26, 0x27);
                    }
                    1 => {
                        self.unmap_b_bg::<_, _, _, 0x7FFF, false, 1>(arm9, &self.banks.h, once(0));
                        self.unmap_b_bg::<_, _, _, 0x7FFF, true, 1>(arm9, &self.banks.h, once(2));
                    }
                    2 => {
                        self.b_bg_ext_pal_ptr = self.zero_buffer.as_ptr();
                        modify_bg_obj_updates!(self, 1, |updates| {
                            updates.bg_ext_palette = 3;
                        });
                    }
                    _ => {
                        unimplemented!("Specified invalid mapping for bank H: {}", prev_value.mst())
                    }
                }
            }
            if value.enabled() {
                match value.mst() & 3 {
                    0 => self.map_lcdc(arm9, 0x26, 0x27, self.banks.h.as_ptr()),
                    1 => self.map_b_bg::<_, _, _, 0x7FFF, 1>(arm9, &self.banks.h, [0, 2]),
                    2 => {
                        self.b_bg_ext_pal_ptr = self.banks.h.as_ptr();
                        modify_bg_obj_updates!(self, 1, |updates| {
                            updates.bg_ext_palette = 3;
                        });
                    }
                    _ => {
                        unimplemented!("Specified invalid mapping for bank H: {}", value.mst())
                    }
                }
            }
        }
    }

    pub fn write_bank_control_i<E: cpu::Engine>(
        &mut self,
        mut value: BankControl,
        arm9: &mut Arm9<E>,
    ) {
        // Bank I requires special code for unmapping, as it gets mirrored inside what is
        // considered a single region by other code.
        value.0 &= 0x83;
        unsafe {
            let prev_value = self.bank_control[8];
            if value == prev_value {
                return;
            }
            self.bank_control[8] = value;
            if prev_value.enabled() {
                match prev_value.mst() & 3 {
                    0 => {
                        self.unmap_lcdc(arm9, 0x28, 0x28);
                    }
                    1 => {
                        modify_bg_obj_updates!(self, 1, |updates| {
                            updates.bg |= 0xCC;
                        });
                        if self.map.b_bg[1].get() == 1 << 2 {
                            self.map.b_bg[1].set(0);
                            self.map.b_bg[3].set(0);
                            let mut b_bg = self.b_bg.as_byte_mut_slice();
                            self.banks
                                .i
                                .as_byte_mut_slice()
                                .copy_from_slice(&b_bg[0x8000..0xC000]);
                            b_bg[0x8000..0x1_0000].fill(0);
                            b_bg[0x1_8000..0x2_0000].fill(0);
                        } else {
                            if self.bg_obj_updates.is_none() {
                                for region in [1, 3] {
                                    map_cpu_visible!(
                                        arm9,
                                        ptr_mask::R | ptr_mask::W_16_32,
                                        0x0620_0000,
                                        0x0640_0000,
                                        self.b_bg,
                                        0x2_0000,
                                        region,
                                        15
                                    );
                                }
                            }
                            self.map.b_bg[1].set(1);
                            let writeback_arr = self.writeback.b_bg.get_mut();
                            for usage_addr in 1 << 15..2 << 15 {
                                if writeback_arr[usage_addr / usize::BITS as usize]
                                    & 1 << (usage_addr & (usize::BITS - 1) as usize)
                                    == 0
                                {
                                    self.b_bg.write_unchecked(
                                        usage_addr,
                                        self.banks.c.read_unchecked(usage_addr),
                                    );
                                } else {
                                    self.banks.i.write_unchecked(
                                        usage_addr & 0x3FFF,
                                        self.b_bg.read_unchecked(usage_addr),
                                    );
                                }
                            }
                            for usage_addr in 3 << 15..4 << 15 {
                                if writeback_arr[usage_addr / usize::BITS as usize]
                                    & 1 << (usage_addr & (usize::BITS - 1) as usize)
                                    == 0
                                {
                                    self.b_bg.write_unchecked(
                                        usage_addr,
                                        self.banks.c.read_unchecked(usage_addr),
                                    );
                                }
                            }
                        }
                    }
                    2 => {
                        modify_bg_obj_updates!(self, 1, |updates| {
                            updates.obj = 0xFF;
                        });
                        let new = self.map.b_obj[0].get() & !(1 << 1);
                        self.map.b_obj[0].set(new);
                        if new == 0 {
                            self.banks
                                .i
                                .as_byte_mut_slice()
                                .copy_from_slice(&self.b_obj.as_byte_slice()[..0x4000]);
                            self.b_obj.as_byte_mut_slice().fill(0);
                        } else {
                            if self.bg_obj_updates.is_none() {
                                arm9.map_sys_bus_ptr_range(
                                    ptr_mask::R | ptr_mask::W_16_32,
                                    self.b_obj.as_ptr(),
                                    0x2_0000,
                                    (0x0660_0000, 0x0680_0000),
                                );
                            }
                            let writeback_arr = self.writeback.b_obj.get_mut();
                            for (usage_addr, byte) in
                                self.b_obj.as_byte_mut_slice().iter_mut().enumerate()
                            {
                                if writeback_arr[usage_addr / usize::BITS as usize]
                                    & 1 << (usage_addr & (usize::BITS - 1) as usize)
                                    == 0
                                {
                                    *byte = self.banks.d.read_unchecked(usage_addr);
                                } else {
                                    self.banks.i.write_unchecked(usage_addr & 0x3FFF, *byte);
                                }
                            }
                        }
                    }
                    _ => {
                        self.b_obj_ext_pal_ptr = self.zero_buffer.as_ptr();
                        modify_bg_obj_updates!(self, 1, |updates| {
                            updates.obj_ext_palette = true;
                        });
                    }
                }
            }
            if value.enabled() {
                match value.mst() & 3 {
                    0 => self.map_lcdc(arm9, 0x28, 0x28, self.banks.i.as_ptr()),
                    1 => self.map_b_bg::<_, _, _, 0x3FFF, 2>(arm9, &self.banks.i, [1, 3]),
                    2 => self.map_b_obj::<_, _, 0x3FFF, 1>(arm9, &self.banks.i),
                    _ => {
                        self.b_obj_ext_pal_ptr = self.banks.i.as_ptr();
                        modify_bg_obj_updates!(self, 1, |updates| {
                            updates.obj_ext_palette = true;
                        });
                    }
                }
            }
        }
    }
}
