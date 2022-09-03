use super::timers;
use crate::{
    emu,
    utils::{Loadable, LoadableInPlace, Storable},
};

#[const_trait]
pub trait ScheduleConst {
    type Timestamp: Copy
        + From<emu::Timestamp>
        + Into<emu::Timestamp>
        + From<timers::Timestamp>
        + Into<timers::Timestamp>
        + PartialEq
        + Eq
        + PartialOrd
        + Ord
        + Loadable
        + LoadableInPlace
        + Storable;
    type Event: Copy + Loadable + LoadableInPlace + Storable;
    type EventSlotIndex: Copy + Loadable + LoadableInPlace + Storable;

    fn timer_event_slot(i: timers::Index) -> Self::EventSlotIndex;
}

pub trait Schedule: ~const ScheduleConst {
    fn cur_time(&self) -> Self::Timestamp;
    fn set_cur_time(&mut self, value: Self::Timestamp);
    #[inline]
    fn set_cur_time_after(&mut self, value: Self::Timestamp) {
        self.set_cur_time(self.cur_time().max(value));
    }

    fn target_time(&self) -> Self::Timestamp;
    fn set_target_time(&mut self, value: Self::Timestamp);
    #[inline]
    fn set_target_time_before(&mut self, target: Self::Timestamp) {
        self.set_target_time(self.target_time().min(target));
    }

    fn set_event(&mut self, slot_index: Self::EventSlotIndex, event: Self::Event);
    fn set_timer_event(&mut self, i: timers::Index);

    fn schedule_event(&mut self, slot_index: Self::EventSlotIndex, time: Self::Timestamp);
    fn cancel_event(&mut self, slot_index: Self::EventSlotIndex);
}
