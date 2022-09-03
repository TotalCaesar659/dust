use crate::{
    cpu::{Irqs, Schedule},
    utils::{schedule::RawTimestamp, Savestate},
};
use core::ops::Add;

#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Savestate)]
pub struct Timestamp(pub RawTimestamp);

impl Add for Timestamp {
    type Output = Self;
    #[inline]
    fn add(self, rhs: Self) -> Self {
        Self(self.0 + rhs.0)
    }
}

proc_bitfield::bitfield! {
    #[derive(Clone, Copy, PartialEq, Eq, Savestate)]
    pub const struct Control(pub u8): Debug {
        pub prescaler: u8 @ 0..=1,
        pub count_up_timing: bool @ 2,
        pub irq_enabled: bool @ 6,
        pub running: bool @ 7,
    }
}

mod bounded {
    use crate::utils::{bounded_int_lit, bounded_int_savestate};
    bounded_int_lit!(pub struct Index(u8), max 3);
    bounded_int_savestate!(Index(u8));
}
pub use bounded::Index;

// NOTE: There are four "real" timers that are always running, one for every possible frequency,
// and the correct one gets selected based on the prescaler value. From the point of view of the
// emulator, this means that the cycle counter doesn't have to be reset but just masked based on the
// new prescaler value, and is equivalent to the (masked) current timestamp when starting a timer.

#[derive(Clone, Copy, Savestate)]
#[load(in_place_only)]
pub struct Timer {
    control: Control,
    cycle_shift: u8,
    count_up: bool,
    schedule_overflows: bool,
    counter: u16,
    reload: u16,
    cycle_counter: u16,
    last_update_time: Timestamp,
}

impl Timer {
    const fn new() -> Self {
        Timer {
            control: Control(0),
            cycle_shift: 0,
            count_up: false,
            schedule_overflows: false,
            counter: 0,
            reload: 0,
            cycle_counter: 0,
            last_update_time: Timestamp(0),
        }
    }

    #[inline]
    pub fn control(&self) -> Control {
        self.control
    }

    #[inline]
    pub fn cycle_shift(&self) -> u8 {
        self.cycle_shift
    }

    #[inline]
    pub fn count_up(&self) -> bool {
        self.count_up
    }

    #[inline]
    pub fn reload(&self) -> u16 {
        self.reload
    }

    #[inline]
    pub fn cycle_counter(&self) -> u16 {
        self.cycle_counter
    }

    fn cycles_until_overflow(&self) -> u32 {
        ((0x1_0000 - self.counter as u32) << self.cycle_shift) - self.cycle_counter as u32
    }
}

#[derive(Savestate)]
#[load(in_place_only)]
pub struct Timers(pub [Timer; 4]);

impl Timers {
    pub(super) fn new(schedule: &mut impl Schedule) -> Self {
        for i in 0..4 {
            schedule.set_timer_event(Index::new(i));
        }

        Timers([Timer::new(); 4])
    }

    pub(crate) fn handle_scheduled_overflow<S: Schedule>(
        &mut self,
        i: Index,
        event_time: S::Timestamp,
        schedule: &mut S,
        irqs: &mut impl Irqs<Schedule = S>,
    ) {
        let event_time = event_time.into();

        // The CPU might have run for a few cycles more than the scheduler and made the timer run
        if self.0[i.get() as usize].last_update_time < event_time {
            self.run_timer(i, event_time, schedule, irqs);
        }

        let timer = &self.0[i.get() as usize];
        let target =
            timer.last_update_time + Timestamp(timer.cycles_until_overflow() as RawTimestamp);
        schedule.schedule_event(S::timer_event_slot(i), target.into());
    }

    fn inc_timer<I: Irqs>(
        &mut self,
        i: Index,
        increments: RawTimestamp,
        schedule: &mut I::Schedule,
        irqs: &mut I,
    ) {
        let timer = &mut self.0[i.get() as usize];
        let mut overflow_incs = 0x1_0000 - timer.counter as RawTimestamp;
        if increments >= overflow_incs {
            if timer.control.irq_enabled() {
                irqs.request_timer(i, schedule);
            }

            let remaining = increments - overflow_incs;
            overflow_incs = 0x1_0000 - timer.reload as RawTimestamp;
            timer.counter = timer
                .reload
                .wrapping_add((remaining % overflow_incs) as u16);

            if i.get() < 3 {
                let next_i = Index::new(i.get() + 1);
                if self.0[next_i.get() as usize].count_up {
                    let overflows = 1 + remaining / overflow_incs;
                    self.inc_timer(next_i, overflows, schedule, irqs);
                }
            }
        } else {
            timer.counter += increments as u16;
        }
    }

    fn run_timer<I: Irqs>(
        &mut self,
        i: Index,
        time: Timestamp,
        schedule: &mut I::Schedule,
        irqs: &mut I,
    ) {
        let timer = &mut self.0[i.get() as usize];
        let new_cycle_counter =
            timer.cycle_counter as RawTimestamp + (time.0 - timer.last_update_time.0);
        timer.cycle_counter = new_cycle_counter as u16 & ((1 << timer.cycle_shift) - 1);
        timer.last_update_time = time;
        let increments = new_cycle_counter >> timer.cycle_shift;
        self.inc_timer(i, increments, schedule, irqs);
    }

    // NOTE: This is theoretically safe to call in memory handlers even for debug accesses, as it
    // doesn't change state visible to the emulated program
    pub fn read_counter<I: Irqs>(
        &mut self,
        i: Index,
        schedule: &mut I::Schedule,
        irqs: &mut I,
    ) -> u16 {
        let mut j = i;
        // Find the closest non-count-up timer and make it run
        loop {
            let timer = &self.0[j.get() as usize];
            if !timer.count_up {
                if timer.control.running() {
                    self.run_timer(j, schedule.cur_time().into(), schedule, irqs);
                }
                break;
            }
            j = Index::new(j.get() - 1);
        }
        self.0[i.get() as usize].counter
    }

    #[inline]
    pub fn write_reload<I: Irqs>(
        &mut self,
        i: Index,
        value: u16,
        schedule: &mut I::Schedule,
        irqs: &mut I,
    ) {
        let timer = &self.0[i.get() as usize];
        // Since it might be expected to overflow before the reload value is updated but actually
        // have a few cycles of delay due to imperfections in the scheduler, update the timer now.
        if timer.control.running() && !timer.count_up {
            self.run_timer(i, schedule.cur_time().into(), schedule, irqs);
        }
        self.0[i.get() as usize].reload = value;
    }

    fn update_control<S: Schedule>(&mut self, i: Index, value: Control, schedule: &mut S) {
        let timer = &mut self.0[i.get() as usize];
        let prev_value = timer.control;
        timer.control = value;
        timer.cycle_shift = [0, 6, 8, 10][timer.control.prescaler() as usize];
        if value.running() {
            if !prev_value.running() {
                timer.counter = timer.reload;
            }
            // Unused for count-up timers
            timer.last_update_time = schedule.cur_time().into();
            timer.cycle_counter = timer.last_update_time.0 as u16 & ((1 << timer.cycle_shift) - 1);
        }
        let scheduled_overflows = timer.schedule_overflows;
        if value.0 & 0xC4 != prev_value.0 & 0xC4 {
            let mut flows_into_irq = false;
            for i in (0..4).rev() {
                let i = Index::new(i);
                let timer = &mut self.0[i.get() as usize];
                let scheduled_overflows = timer.schedule_overflows;
                timer.schedule_overflows = false;
                if timer.control.running() {
                    flows_into_irq |= timer.control.irq_enabled();
                    if !timer.count_up {
                        timer.schedule_overflows = flows_into_irq;
                        flows_into_irq = false;
                    }
                } else {
                    flows_into_irq = false;
                }
                if !timer.schedule_overflows && scheduled_overflows {
                    schedule.cancel_event(S::timer_event_slot(i));
                } else if timer.schedule_overflows && !scheduled_overflows {
                    let target = timer.last_update_time
                        + Timestamp(timer.cycles_until_overflow() as RawTimestamp);
                    schedule.schedule_event(S::timer_event_slot(i), target.into());
                }
            }
        }
        let timer = &self.0[i.get() as usize];
        if timer.schedule_overflows
            && scheduled_overflows
            && value.prescaler() != prev_value.prescaler()
        {
            let event_slot = S::timer_event_slot(i);
            schedule.cancel_event(event_slot);
            let target =
                timer.last_update_time + Timestamp(timer.cycles_until_overflow() as RawTimestamp);
            schedule.schedule_event(event_slot, target.into());
        }
    }

    pub fn write_control<I: Irqs>(
        &mut self,
        i: Index,
        mut value: Control,
        schedule: &mut I::Schedule,
        irqs: &mut I,
    ) {
        value.0 &= 0xC7;
        let count_up = value.count_up_timing() && value.running() && i.get() != 0;
        let timer = &mut self.0[i.get() as usize];
        let same = !Control(value.0 | timer.control.0).running()
            || (!(count_up ^ timer.count_up)
                && if count_up {
                    value.0 & 0xC0 == timer.control.0 & 0xC0
                } else {
                    value.0 & 0xC3 == timer.control.0 & 0xC3
                });
        if same {
            timer.control = value;
            return;
        }
        let mut j = i;
        if !timer.count_up && count_up {
            // Update all timers that will begin flowing into this one, plus this one if previously
            // running.
            if timer.control.running() {
                self.run_timer(i, schedule.cur_time().into(), schedule, irqs);
            }
            j = Index::new(i.get() - 1);
        } else {
            // Update all timers that are/were flowing into this one, or this timer if it was
            // running and not in count-up timing mode.
        }
        loop {
            let timer = &self.0[j.get() as usize];
            if !timer.count_up {
                if timer.control.running() {
                    self.run_timer(j, schedule.cur_time().into(), schedule, irqs);
                }
                break;
            }
            j = Index::new(j.get() - 1);
        }
        let timer = &mut self.0[i.get() as usize];
        timer.count_up = count_up;
        self.update_control(i, value, schedule);
    }

    pub fn write_control_reload<I: Irqs>(
        &mut self,
        i: Index,
        reload: u16,
        mut control: Control,
        schedule: &mut I::Schedule,
        irqs: &mut I,
    ) {
        control.0 &= 0xC7;
        let count_up = control.count_up_timing() && control.running() && i.get() != 0;
        let timer = &mut self.0[i.get() as usize];
        let same = !Control(control.0 | timer.control.0).running()
            || (reload == timer.reload
                && !(count_up ^ timer.count_up)
                && if count_up {
                    control.0 & 0xC0 == timer.control.0 & 0xC0
                } else {
                    control.0 & 0xC3 == timer.control.0 & 0xC3
                });
        if same {
            timer.control = control;
            timer.reload = reload;
            return;
        }
        let mut j = i;
        if !timer.count_up && count_up {
            if timer.control.running() {
                self.run_timer(i, schedule.cur_time().into(), schedule, irqs);
            }
            j = Index::new(i.get() - 1);
        }
        loop {
            let timer = &self.0[j.get() as usize];
            if !timer.count_up {
                if timer.control.running() {
                    self.run_timer(j, schedule.cur_time().into(), schedule, irqs);
                }
                break;
            }
            j = Index::new(j.get() - 1);
        }
        let timer = &mut self.0[i.get() as usize];
        timer.reload = reload;
        timer.count_up = count_up;
        self.update_control(i, control, schedule);
    }
}
